/*!

# typeck: check phase

Within the check phase of type check, we check each item one at a time
(bodies of function expressions are checked as part of the containing
function). Inference is used to supply types wherever they are unknown.

By far the most complex case is checking the body of a function. This
can be broken down into several distinct phases:

- gather: creates type variables to represent the type of each local
  variable and pattern binding.

- main: the main pass does the lion's share of the work: it
  determines the types of all expressions, resolves
  methods, checks for most invalid conditions, and so forth.  In
  some cases, where a type is unknown, it may create a type or region
  variable and use that as the type of an expression.

  In the process of checking, various constraints will be placed on
  these type variables through the subtyping relationships requested
  through the `demand` module.  The `infer` module is in charge
  of resolving those constraints.

- regionck: after main is complete, the regionck pass goes over all
  types looking for regions and making sure that they did not escape
  into places they are not in scope.  This may also influence the
  final assignments of the various region variables if there is some
  flexibility.

- writeback: writes the final types within a function body, replacing
  type variables with their final inferred types.  These final types
  are written into the `tcx.node_types` table, which should *never* contain
  any reference to a type variable.

## Intermediate types

While type checking a function, the intermediate types for the
expressions, blocks, and so forth contained within the function are
stored in `fcx.node_types` and `fcx.node_substs`.  These types
may contain unresolved type variables.  After type checking is
complete, the functions in the writeback module are used to take the
types from this table, resolve them, and then write them into their
permanent home in the type context `tcx`.

This means that during inferencing you should use `fcx.write_ty()`
and `fcx.expr_ty()` / `fcx.node_ty()` to write/obtain the types of
nodes within the function.

The types of top-level items, which never contain unbound type
variables, are stored directly into the `tcx` typeck_results.

N.B., a type variable is not the same thing as a type parameter.  A
type variable is rather an "instance" of a type parameter: that is,
given a generic function `fn foo<T>(t: T)`: while checking the
function `foo`, the type `ty_param(0)` refers to the type `T`, which
is treated in abstract.  When `foo()` is called, however, `T` will be
substituted for a fresh type variable `N`.  This variable will
eventually be resolved to some concrete type (which might itself be
type parameter).

*/

pub mod _match;
mod autoderef;
mod callee;
pub mod cast;
mod check;
mod closure;
pub mod coercion;
mod compare_method;
pub mod demand;
mod diverges;
pub mod dropck;
mod expectation;
mod expr;
mod fn_ctxt;
mod gather_locals;
mod generator_interior;
mod inherited;
pub mod intrinsic;
pub mod method;
mod op;
mod pat;
mod place_op;
mod regionck;
mod upvar;
mod wfcheck;
pub mod writeback;

use check::{
    check_abi, check_fn, check_impl_item_well_formed, check_item_well_formed, check_mod_item_types,
    check_trait_item_well_formed,
};
pub use check::{check_item_type, check_wf_new};
pub use diverges::Diverges;
pub use expectation::Expectation;
pub use fn_ctxt::FnCtxt;
pub use inherited::{Inherited, InheritedBuilder};

use crate::astconv::AstConv;
use crate::check::gather_locals::GatherLocalsVisitor;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_errors::{pluralize, struct_span_err, Applicability};
use rustc_hir as hir;
use rustc_hir::def::Res;
use rustc_hir::def_id::{CrateNum, DefId, LocalDefId, LOCAL_CRATE};
use rustc_hir::intravisit::Visitor;
use rustc_hir::itemlikevisit::ItemLikeVisitor;
use rustc_hir::{HirIdMap, Node};
use rustc_index::bit_set::BitSet;
use rustc_index::vec::Idx;
use rustc_middle::ty::fold::{TypeFoldable, TypeFolder};
use rustc_middle::ty::query::Providers;
use rustc_middle::ty::subst::GenericArgKind;
use rustc_middle::ty::subst::{InternalSubsts, Subst, SubstsRef};
use rustc_middle::ty::WithConstness;
use rustc_middle::ty::{self, RegionKind, Ty, TyCtxt, UserType};
use rustc_session::config;
use rustc_session::parse::feature_err;
use rustc_session::Session;
use rustc_span::source_map::DUMMY_SP;
use rustc_span::symbol::{kw, Ident};
use rustc_span::{self, BytePos, MultiSpan, Span};
use rustc_target::abi::VariantIdx;
use rustc_target::spec::abi::Abi;
use rustc_trait_selection::traits;
use rustc_trait_selection::traits::error_reporting::recursive_type_with_infinite_size_error;
use rustc_trait_selection::traits::error_reporting::suggestions::ReturnsVisitor;

use std::cell::{Ref, RefCell, RefMut};

use crate::require_c_abi_if_c_variadic;
use crate::util::common::indenter;

use self::coercion::DynamicCoerceMany;
pub use self::Expectation::*;

#[macro_export]
macro_rules! type_error_struct {
    ($session:expr, $span:expr, $typ:expr, $code:ident, $($message:tt)*) => ({
        if $typ.references_error() {
            $session.diagnostic().struct_dummy()
        } else {
            rustc_errors::struct_span_err!($session, $span, $code, $($message)*)
        }
    })
}

/// The type of a local binding, including the revealed type for anon types.
#[derive(Copy, Clone, Debug)]
pub struct LocalTy<'tcx> {
    decl_ty: Ty<'tcx>,
    revealed_ty: Ty<'tcx>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Needs {
    MutPlace,
    None,
}

impl Needs {
    fn maybe_mut_place(m: hir::Mutability) -> Self {
        match m {
            hir::Mutability::Mut => Needs::MutPlace,
            hir::Mutability::Not => Needs::None,
        }
    }
}

#[derive(Copy, Clone)]
pub struct UnsafetyState {
    pub def: hir::HirId,
    pub unsafety: hir::Unsafety,
    pub unsafe_push_count: u32,
    from_fn: bool,
}

impl UnsafetyState {
    pub fn function(unsafety: hir::Unsafety, def: hir::HirId) -> UnsafetyState {
        UnsafetyState { def, unsafety, unsafe_push_count: 0, from_fn: true }
    }

    pub fn recurse(&mut self, blk: &hir::Block<'_>) -> UnsafetyState {
        use hir::BlockCheckMode;
        match self.unsafety {
            // If this unsafe, then if the outer function was already marked as
            // unsafe we shouldn't attribute the unsafe'ness to the block. This
            // way the block can be warned about instead of ignoring this
            // extraneous block (functions are never warned about).
            hir::Unsafety::Unsafe if self.from_fn => *self,

            unsafety => {
                let (unsafety, def, count) = match blk.rules {
                    BlockCheckMode::PushUnsafeBlock(..) => {
                        (unsafety, blk.hir_id, self.unsafe_push_count.checked_add(1).unwrap())
                    }
                    BlockCheckMode::PopUnsafeBlock(..) => {
                        (unsafety, blk.hir_id, self.unsafe_push_count.checked_sub(1).unwrap())
                    }
                    BlockCheckMode::UnsafeBlock(..) => {
                        (hir::Unsafety::Unsafe, blk.hir_id, self.unsafe_push_count)
                    }
                    BlockCheckMode::DefaultBlock => (unsafety, self.def, self.unsafe_push_count),
                };
                UnsafetyState { def, unsafety, unsafe_push_count: count, from_fn: false }
            }
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum PlaceOp {
    Deref,
    Index,
}

pub struct BreakableCtxt<'tcx> {
    may_break: bool,

    // this is `null` for loops where break with a value is illegal,
    // such as `while`, `for`, and `while let`
    coerce: Option<DynamicCoerceMany<'tcx>>,
}

#[derive(Default)]
pub struct EnclosingBreakables<'tcx> {
    stack: Vec<BreakableCtxt<'tcx>>,
    by_id: HirIdMap<usize>,
}

impl<'tcx> EnclosingBreakables<'tcx> {
    fn new() -> Self {
        Default::default()
    }

    fn find_breakable(&mut self, target_id: hir::HirId) -> &mut BreakableCtxt<'tcx> {
        self.opt_find_breakable(target_id).unwrap_or_else(|| {
            bug!("could not find enclosing breakable with id {}", target_id);
        })
    }

    fn opt_find_breakable(&mut self, target_id: hir::HirId) -> Option<&mut BreakableCtxt<'tcx>> {
        match self.by_id.get(&target_id) {
            Some(ix) => Some(&mut self.stack[*ix]),
            None => None,
        }
    }
}

pub fn provide(providers: &mut Providers) {
    method::provide(providers);
    *providers = Providers {
        typeck_item_bodies,
        typeck_const_arg,
        typeck,
        diagnostic_only_typeck,
        has_typeck_results,
        adt_destructor,
        used_trait_imports,
        check_item_well_formed,
        check_trait_item_well_formed,
        check_impl_item_well_formed,
        check_mod_item_types,
        ..*providers
    };
}

fn adt_destructor(tcx: TyCtxt<'_>, def_id: DefId) -> Option<ty::Destructor> {
    tcx.calculate_dtor(def_id, &mut dropck::check_drop_impl)
}

/// If this `DefId` is a "primary tables entry", returns
/// `Some((body_id, header, decl))` with information about
/// it's body-id, fn-header and fn-decl (if any). Otherwise,
/// returns `None`.
///
/// If this function returns `Some`, then `typeck_results(def_id)` will
/// succeed; if it returns `None`, then `typeck_results(def_id)` may or
/// may not succeed. In some cases where this function returns `None`
/// (notably closures), `typeck_results(def_id)` would wind up
/// redirecting to the owning function.
fn primary_body_of(
    tcx: TyCtxt<'_>,
    id: hir::HirId,
) -> Option<(hir::BodyId, Option<&hir::Ty<'_>>, Option<&hir::FnHeader>, Option<&hir::FnDecl<'_>>)> {
    match tcx.hir().get(id) {
        Node::Item(item) => match item.kind {
            hir::ItemKind::Const(ref ty, body) | hir::ItemKind::Static(ref ty, _, body) => {
                Some((body, Some(ty), None, None))
            }
            hir::ItemKind::Fn(ref sig, .., body) => {
                Some((body, None, Some(&sig.header), Some(&sig.decl)))
            }
            _ => None,
        },
        Node::TraitItem(item) => match item.kind {
            hir::TraitItemKind::Const(ref ty, Some(body)) => Some((body, Some(ty), None, None)),
            hir::TraitItemKind::Fn(ref sig, hir::TraitFn::Provided(body)) => {
                Some((body, None, Some(&sig.header), Some(&sig.decl)))
            }
            _ => None,
        },
        Node::ImplItem(item) => match item.kind {
            hir::ImplItemKind::Const(ref ty, body) => Some((body, Some(ty), None, None)),
            hir::ImplItemKind::Fn(ref sig, body) => {
                Some((body, None, Some(&sig.header), Some(&sig.decl)))
            }
            _ => None,
        },
        Node::AnonConst(constant) => Some((constant.body, None, None, None)),
        _ => None,
    }
}

fn has_typeck_results(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
    // Closures' typeck results come from their outermost function,
    // as they are part of the same "inference environment".
    let outer_def_id = tcx.closure_base_def_id(def_id);
    if outer_def_id != def_id {
        return tcx.has_typeck_results(outer_def_id);
    }

    if let Some(def_id) = def_id.as_local() {
        let id = tcx.hir().local_def_id_to_hir_id(def_id);
        primary_body_of(tcx, id).is_some()
    } else {
        false
    }
}

fn used_trait_imports(tcx: TyCtxt<'_>, def_id: LocalDefId) -> &FxHashSet<LocalDefId> {
    &*tcx.typeck(def_id).used_trait_imports
}

/// Inspects the substs of opaque types, replacing any inference variables
/// with proper generic parameter from the identity substs.
///
/// This is run after we normalize the function signature, to fix any inference
/// variables introduced by the projection of associated types. This ensures that
/// any opaque types used in the signature continue to refer to generic parameters,
/// allowing them to be considered for defining uses in the function body
///
/// For example, consider this code.
///
/// ```rust
/// trait MyTrait {
///     type MyItem;
///     fn use_it(self) -> Self::MyItem
/// }
/// impl<T, I> MyTrait for T where T: Iterator<Item = I> {
///     type MyItem = impl Iterator<Item = I>;
///     fn use_it(self) -> Self::MyItem {
///         self
///     }
/// }
/// ```
///
/// When we normalize the signature of `use_it` from the impl block,
/// we will normalize `Self::MyItem` to the opaque type `impl Iterator<Item = I>`
/// However, this projection result may contain inference variables, due
/// to the way that projection works. We didn't have any inference variables
/// in the signature to begin with - leaving them in will cause us to incorrectly
/// conclude that we don't have a defining use of `MyItem`. By mapping inference
/// variables back to the actual generic parameters, we will correctly see that
/// we have a defining use of `MyItem`
fn fixup_opaque_types<'tcx, T>(tcx: TyCtxt<'tcx>, val: &T) -> T
where
    T: TypeFoldable<'tcx>,
{
    struct FixupFolder<'tcx> {
        tcx: TyCtxt<'tcx>,
    }

    impl<'tcx> TypeFolder<'tcx> for FixupFolder<'tcx> {
        fn tcx<'a>(&'a self) -> TyCtxt<'tcx> {
            self.tcx
        }

        fn fold_ty(&mut self, ty: Ty<'tcx>) -> Ty<'tcx> {
            match *ty.kind() {
                ty::Opaque(def_id, substs) => {
                    debug!("fixup_opaque_types: found type {:?}", ty);
                    // Here, we replace any inference variables that occur within
                    // the substs of an opaque type. By definition, any type occurring
                    // in the substs has a corresponding generic parameter, which is what
                    // we replace it with.
                    // This replacement is only run on the function signature, so any
                    // inference variables that we come across must be the rust of projection
                    // (there's no other way for a user to get inference variables into
                    // a function signature).
                    if ty.needs_infer() {
                        let new_substs = InternalSubsts::for_item(self.tcx, def_id, |param, _| {
                            let old_param = substs[param.index as usize];
                            match old_param.unpack() {
                                GenericArgKind::Type(old_ty) => {
                                    if let ty::Infer(_) = old_ty.kind() {
                                        // Replace inference type with a generic parameter
                                        self.tcx.mk_param_from_def(param)
                                    } else {
                                        old_param.fold_with(self)
                                    }
                                }
                                GenericArgKind::Const(old_const) => {
                                    if let ty::ConstKind::Infer(_) = old_const.val {
                                        // This should never happen - we currently do not support
                                        // 'const projections', e.g.:
                                        // `impl<T: SomeTrait> MyTrait for T where <T as SomeTrait>::MyConst == 25`
                                        // which should be the only way for us to end up with a const inference
                                        // variable after projection. If Rust ever gains support for this kind
                                        // of projection, this should *probably* be changed to
                                        // `self.tcx.mk_param_from_def(param)`
                                        bug!(
                                            "Found infer const: `{:?}` in opaque type: {:?}",
                                            old_const,
                                            ty
                                        );
                                    } else {
                                        old_param.fold_with(self)
                                    }
                                }
                                GenericArgKind::Lifetime(old_region) => {
                                    if let RegionKind::ReVar(_) = old_region {
                                        self.tcx.mk_param_from_def(param)
                                    } else {
                                        old_param.fold_with(self)
                                    }
                                }
                            }
                        });
                        let new_ty = self.tcx.mk_opaque(def_id, new_substs);
                        debug!("fixup_opaque_types: new type: {:?}", new_ty);
                        new_ty
                    } else {
                        ty
                    }
                }
                _ => ty.super_fold_with(self),
            }
        }
    }

    debug!("fixup_opaque_types({:?})", val);
    val.fold_with(&mut FixupFolder { tcx })
}

fn typeck_const_arg<'tcx>(
    tcx: TyCtxt<'tcx>,
    (did, param_did): (LocalDefId, DefId),
) -> &ty::TypeckResults<'tcx> {
    let fallback = move || tcx.type_of(param_did);
    typeck_with_fallback(tcx, did, fallback)
}

fn typeck<'tcx>(tcx: TyCtxt<'tcx>, def_id: LocalDefId) -> &ty::TypeckResults<'tcx> {
    if let Some(param_did) = tcx.opt_const_param_of(def_id) {
        tcx.typeck_const_arg((def_id, param_did))
    } else {
        let fallback = move || tcx.type_of(def_id.to_def_id());
        typeck_with_fallback(tcx, def_id, fallback)
    }
}

/// Used only to get `TypeckResults` for type inference during error recovery.
/// Currently only used for type inference of `static`s and `const`s to avoid type cycle errors.
fn diagnostic_only_typeck<'tcx>(tcx: TyCtxt<'tcx>, def_id: LocalDefId) -> &ty::TypeckResults<'tcx> {
    let fallback = move || {
        let span = tcx.hir().span(tcx.hir().local_def_id_to_hir_id(def_id));
        tcx.ty_error_with_message(span, "diagnostic only typeck table used")
    };
    typeck_with_fallback(tcx, def_id, fallback)
}

fn typeck_with_fallback<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
    fallback: impl Fn() -> Ty<'tcx> + 'tcx,
) -> &'tcx ty::TypeckResults<'tcx> {
    // Closures' typeck results come from their outermost function,
    // as they are part of the same "inference environment".
    let outer_def_id = tcx.closure_base_def_id(def_id.to_def_id()).expect_local();
    if outer_def_id != def_id {
        return tcx.typeck(outer_def_id);
    }

    let id = tcx.hir().local_def_id_to_hir_id(def_id);
    let span = tcx.hir().span(id);

    // Figure out what primary body this item has.
    let (body_id, body_ty, fn_header, fn_decl) = primary_body_of(tcx, id).unwrap_or_else(|| {
        span_bug!(span, "can't type-check body of {:?}", def_id);
    });
    let body = tcx.hir().body(body_id);

    let typeck_results = Inherited::build(tcx, def_id).enter(|inh| {
        let param_env = tcx.param_env(def_id);
        let fcx = if let (Some(header), Some(decl)) = (fn_header, fn_decl) {
            let fn_sig = if crate::collect::get_infer_ret_ty(&decl.output).is_some() {
                let fcx = FnCtxt::new(&inh, param_env, body.value.hir_id);
                AstConv::ty_of_fn(
                    &fcx,
                    header.unsafety,
                    header.abi,
                    decl,
                    &hir::Generics::empty(),
                    None,
                )
            } else {
                tcx.fn_sig(def_id)
            };

            check_abi(tcx, span, fn_sig.abi());

            // Compute the fty from point of view of inside the fn.
            let fn_sig = tcx.liberate_late_bound_regions(def_id.to_def_id(), &fn_sig);
            let fn_sig = inh.normalize_associated_types_in(
                body.value.span,
                body_id.hir_id,
                param_env,
                &fn_sig,
            );

            let fn_sig = fixup_opaque_types(tcx, &fn_sig);

            let fcx = check_fn(&inh, param_env, fn_sig, decl, id, body, None).0;
            fcx
        } else {
            let fcx = FnCtxt::new(&inh, param_env, body.value.hir_id);
            let expected_type = body_ty
                .and_then(|ty| match ty.kind {
                    hir::TyKind::Infer => Some(AstConv::ast_ty_to_ty(&fcx, ty)),
                    _ => None,
                })
                .unwrap_or_else(fallback);
            let expected_type = fcx.normalize_associated_types_in(body.value.span, &expected_type);
            fcx.require_type_is_sized(expected_type, body.value.span, traits::ConstSized);

            let revealed_ty = if tcx.features().impl_trait_in_bindings {
                fcx.instantiate_opaque_types_from_value(id, &expected_type, body.value.span)
            } else {
                expected_type
            };

            // Gather locals in statics (because of block expressions).
            GatherLocalsVisitor::new(&fcx, id).visit_body(body);

            fcx.check_expr_coercable_to_type(&body.value, revealed_ty, None);

            fcx.write_ty(id, revealed_ty);

            fcx
        };

        // All type checking constraints were added, try to fallback unsolved variables.
        fcx.select_obligations_where_possible(false, |_| {});
        let mut fallback_has_occurred = false;

        // We do fallback in two passes, to try to generate
        // better error messages.
        // The first time, we do *not* replace opaque types.
        for ty in &fcx.unsolved_variables() {
            fallback_has_occurred |= fcx.fallback_if_possible(ty, FallbackMode::NoOpaque);
        }
        // We now see if we can make progress. This might
        // cause us to unify inference variables for opaque types,
        // since we may have unified some other type variables
        // during the first phase of fallback.
        // This means that we only replace inference variables with their underlying
        // opaque types as a last resort.
        //
        // In code like this:
        //
        // ```rust
        // type MyType = impl Copy;
        // fn produce() -> MyType { true }
        // fn bad_produce() -> MyType { panic!() }
        // ```
        //
        // we want to unify the opaque inference variable in `bad_produce`
        // with the diverging fallback for `panic!` (e.g. `()` or `!`).
        // This will produce a nice error message about conflicting concrete
        // types for `MyType`.
        //
        // If we had tried to fallback the opaque inference variable to `MyType`,
        // we will generate a confusing type-check error that does not explicitly
        // refer to opaque types.
        fcx.select_obligations_where_possible(fallback_has_occurred, |_| {});

        // We now run fallback again, but this time we allow it to replace
        // unconstrained opaque type variables, in addition to performing
        // other kinds of fallback.
        for ty in &fcx.unsolved_variables() {
            fallback_has_occurred |= fcx.fallback_if_possible(ty, FallbackMode::All);
        }

        // See if we can make any more progress.
        fcx.select_obligations_where_possible(fallback_has_occurred, |_| {});

        // Even though coercion casts provide type hints, we check casts after fallback for
        // backwards compatibility. This makes fallback a stronger type hint than a cast coercion.
        fcx.check_casts();

        // Closure and generator analysis may run after fallback
        // because they don't constrain other type variables.
        fcx.closure_analyze(body);
        assert!(fcx.deferred_call_resolutions.borrow().is_empty());
        fcx.resolve_generator_interiors(def_id.to_def_id());

        for (ty, span, code) in fcx.deferred_sized_obligations.borrow_mut().drain(..) {
            let ty = fcx.normalize_ty(span, ty);
            fcx.require_type_is_sized(ty, span, code);
        }

        fcx.select_all_obligations_or_error();

        if fn_decl.is_some() {
            fcx.regionck_fn(id, body);
        } else {
            fcx.regionck_expr(body);
        }

        fcx.resolve_type_vars_in_body(body)
    });

    // Consistency check our TypeckResults instance can hold all ItemLocalIds
    // it will need to hold.
    assert_eq!(typeck_results.hir_owner, id.owner);

    typeck_results
}

/// When `check_fn` is invoked on a generator (i.e., a body that
/// includes yield), it returns back some information about the yield
/// points.
struct GeneratorTypes<'tcx> {
    /// Type of generator argument / values returned by `yield`.
    resume_ty: Ty<'tcx>,

    /// Type of value that is yielded.
    yield_ty: Ty<'tcx>,

    /// Types that are captured (see `GeneratorInterior` for more).
    interior: Ty<'tcx>,

    /// Indicates if the generator is movable or static (immovable).
    movability: hir::Movability,
}

/// Given a `DefId` for an opaque type in return position, find its parent item's return
/// expressions.
fn get_owner_return_paths(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
) -> Option<(hir::HirId, ReturnsVisitor<'tcx>)> {
    let hir_id = tcx.hir().local_def_id_to_hir_id(def_id);
    let id = tcx.hir().get_parent_item(hir_id);
    tcx.hir()
        .find(id)
        .map(|n| (id, n))
        .and_then(|(hir_id, node)| node.body_id().map(|b| (hir_id, b)))
        .map(|(hir_id, body_id)| {
            let body = tcx.hir().body(body_id);
            let mut visitor = ReturnsVisitor::default();
            visitor.visit_body(body);
            (hir_id, visitor)
        })
}

/// Emit an error for recursive opaque types in a `let` binding.
fn binding_opaque_type_cycle_error(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
    span: Span,
    partially_expanded_type: Ty<'tcx>,
) {
    let mut err = struct_span_err!(tcx.sess, span, E0720, "cannot resolve opaque type");
    err.span_label(span, "cannot resolve opaque type");
    // Find the owner that declared this `impl Trait` type.
    let hir_id = tcx.hir().local_def_id_to_hir_id(def_id);
    let mut prev_hir_id = hir_id;
    let mut hir_id = tcx.hir().get_parent_node(hir_id);
    while let Some(node) = tcx.hir().find(hir_id) {
        match node {
            hir::Node::Local(hir::Local {
                pat,
                init: None,
                ty: Some(ty),
                source: hir::LocalSource::Normal,
                ..
            }) => {
                err.span_label(pat.span, "this binding might not have a concrete type");
                err.span_suggestion_verbose(
                    ty.span.shrink_to_hi(),
                    "set the binding to a value for a concrete type to be resolved",
                    " = /* value */".to_string(),
                    Applicability::HasPlaceholders,
                );
            }
            hir::Node::Local(hir::Local {
                init: Some(expr),
                source: hir::LocalSource::Normal,
                ..
            }) => {
                let hir_id = tcx.hir().local_def_id_to_hir_id(def_id);
                let typeck_results =
                    tcx.typeck(tcx.hir().local_def_id(tcx.hir().get_parent_item(hir_id)));
                if let Some(ty) = typeck_results.node_type_opt(expr.hir_id) {
                    err.span_label(
                        expr.span,
                        &format!(
                            "this is of type `{}`, which doesn't constrain \
                             `{}` enough to arrive to a concrete type",
                            ty, partially_expanded_type
                        ),
                    );
                }
            }
            _ => {}
        }
        if prev_hir_id == hir_id {
            break;
        }
        prev_hir_id = hir_id;
        hir_id = tcx.hir().get_parent_node(hir_id);
    }
    err.emit();
}

// Forbid defining intrinsics in Rust code,
// as they must always be defined by the compiler.
fn fn_maybe_err(tcx: TyCtxt<'_>, sp: Span, abi: Abi) {
    if let Abi::RustIntrinsic | Abi::PlatformIntrinsic = abi {
        tcx.sess.span_err(sp, "intrinsic must be in `extern \"rust-intrinsic\" { ... }` block");
    }
}

fn maybe_check_static_with_link_section(tcx: TyCtxt<'_>, id: LocalDefId, span: Span) {
    // Only restricted on wasm32 target for now
    if !tcx.sess.opts.target_triple.triple().starts_with("wasm32") {
        return;
    }

    // If `#[link_section]` is missing, then nothing to verify
    let attrs = tcx.codegen_fn_attrs(id);
    if attrs.link_section.is_none() {
        return;
    }

    // For the wasm32 target statics with `#[link_section]` are placed into custom
    // sections of the final output file, but this isn't link custom sections of
    // other executable formats. Namely we can only embed a list of bytes,
    // nothing with pointers to anything else or relocations. If any relocation
    // show up, reject them here.
    // `#[link_section]` may contain arbitrary, or even undefined bytes, but it is
    // the consumer's responsibility to ensure all bytes that have been read
    // have defined values.
    match tcx.eval_static_initializer(id.to_def_id()) {
        Ok(alloc) => {
            if alloc.relocations().len() != 0 {
                let msg = "statics with a custom `#[link_section]` must be a \
                           simple list of bytes on the wasm target with no \
                           extra levels of indirection such as references";
                tcx.sess.span_err(span, msg);
            }
        }
        Err(_) => {}
    }
}

fn report_forbidden_specialization(
    tcx: TyCtxt<'_>,
    impl_item: &hir::ImplItem<'_>,
    parent_impl: DefId,
) {
    let mut err = struct_span_err!(
        tcx.sess,
        impl_item.span,
        E0520,
        "`{}` specializes an item from a parent `impl`, but \
         that item is not marked `default`",
        impl_item.ident
    );
    err.span_label(impl_item.span, format!("cannot specialize default item `{}`", impl_item.ident));

    match tcx.span_of_impl(parent_impl) {
        Ok(span) => {
            err.span_label(span, "parent `impl` is here");
            err.note(&format!(
                "to specialize, `{}` in the parent `impl` must be marked `default`",
                impl_item.ident
            ));
        }
        Err(cname) => {
            err.note(&format!("parent implementation is in crate `{}`", cname));
        }
    }

    err.emit();
}

fn missing_items_err(
    tcx: TyCtxt<'_>,
    impl_span: Span,
    missing_items: &[ty::AssocItem],
    full_impl_span: Span,
) {
    let missing_items_msg = missing_items
        .iter()
        .map(|trait_item| trait_item.ident.to_string())
        .collect::<Vec<_>>()
        .join("`, `");

    let mut err = struct_span_err!(
        tcx.sess,
        impl_span,
        E0046,
        "not all trait items implemented, missing: `{}`",
        missing_items_msg
    );
    err.span_label(impl_span, format!("missing `{}` in implementation", missing_items_msg));

    // `Span` before impl block closing brace.
    let hi = full_impl_span.hi() - BytePos(1);
    // Point at the place right before the closing brace of the relevant `impl` to suggest
    // adding the associated item at the end of its body.
    let sugg_sp = full_impl_span.with_lo(hi).with_hi(hi);
    // Obtain the level of indentation ending in `sugg_sp`.
    let indentation = tcx.sess.source_map().span_to_margin(sugg_sp).unwrap_or(0);
    // Make the whitespace that will make the suggestion have the right indentation.
    let padding: String = (0..indentation).map(|_| " ").collect();

    for trait_item in missing_items {
        let snippet = suggestion_signature(&trait_item, tcx);
        let code = format!("{}{}\n{}", padding, snippet, padding);
        let msg = format!("implement the missing item: `{}`", snippet);
        let appl = Applicability::HasPlaceholders;
        if let Some(span) = tcx.hir().span_if_local(trait_item.def_id) {
            err.span_label(span, format!("`{}` from trait", trait_item.ident));
            err.tool_only_span_suggestion(sugg_sp, &msg, code, appl);
        } else {
            err.span_suggestion_hidden(sugg_sp, &msg, code, appl);
        }
    }
    err.emit();
}

/// Resugar `ty::GenericPredicates` in a way suitable to be used in structured suggestions.
fn bounds_from_generic_predicates<'tcx>(
    tcx: TyCtxt<'tcx>,
    predicates: ty::GenericPredicates<'tcx>,
) -> (String, String) {
    let mut types: FxHashMap<Ty<'tcx>, Vec<DefId>> = FxHashMap::default();
    let mut projections = vec![];
    for (predicate, _) in predicates.predicates {
        debug!("predicate {:?}", predicate);
        match predicate.skip_binders() {
            ty::PredicateAtom::Trait(trait_predicate, _) => {
                let entry = types.entry(trait_predicate.self_ty()).or_default();
                let def_id = trait_predicate.def_id();
                if Some(def_id) != tcx.lang_items().sized_trait() {
                    // Type params are `Sized` by default, do not add that restriction to the list
                    // if it is a positive requirement.
                    entry.push(trait_predicate.def_id());
                }
            }
            ty::PredicateAtom::Projection(projection_pred) => {
                projections.push(ty::Binder::bind(projection_pred));
            }
            _ => {}
        }
    }
    let generics = if types.is_empty() {
        "".to_string()
    } else {
        format!(
            "<{}>",
            types
                .keys()
                .filter_map(|t| match t.kind() {
                    ty::Param(_) => Some(t.to_string()),
                    // Avoid suggesting the following:
                    // fn foo<T, <T as Trait>::Bar>(_: T) where T: Trait, <T as Trait>::Bar: Other {}
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let mut where_clauses = vec![];
    for (ty, bounds) in types {
        for bound in &bounds {
            where_clauses.push(format!("{}: {}", ty, tcx.def_path_str(*bound)));
        }
    }
    for projection in &projections {
        let p = projection.skip_binder();
        // FIXME: this is not currently supported syntax, we should be looking at the `types` and
        // insert the associated types where they correspond, but for now let's be "lazy" and
        // propose this instead of the following valid resugaring:
        // `T: Trait, Trait::Assoc = K` → `T: Trait<Assoc = K>`
        where_clauses.push(format!("{} = {}", tcx.def_path_str(p.projection_ty.item_def_id), p.ty));
    }
    let where_clauses = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" where {}", where_clauses.join(", "))
    };
    (generics, where_clauses)
}

/// Return placeholder code for the given function.
fn fn_sig_suggestion<'tcx>(
    tcx: TyCtxt<'tcx>,
    sig: ty::FnSig<'tcx>,
    ident: Ident,
    predicates: ty::GenericPredicates<'tcx>,
    assoc: &ty::AssocItem,
) -> String {
    let args = sig
        .inputs()
        .iter()
        .enumerate()
        .map(|(i, ty)| {
            Some(match ty.kind() {
                ty::Param(_) if assoc.fn_has_self_parameter && i == 0 => "self".to_string(),
                ty::Ref(reg, ref_ty, mutability) if i == 0 => {
                    let reg = match &format!("{}", reg)[..] {
                        "'_" | "" => String::new(),
                        reg => format!("{} ", reg),
                    };
                    if assoc.fn_has_self_parameter {
                        match ref_ty.kind() {
                            ty::Param(param) if param.name == kw::SelfUpper => {
                                format!("&{}{}self", reg, mutability.prefix_str())
                            }

                            _ => format!("self: {}", ty),
                        }
                    } else {
                        format!("_: {}", ty)
                    }
                }
                _ => {
                    if assoc.fn_has_self_parameter && i == 0 {
                        format!("self: {}", ty)
                    } else {
                        format!("_: {}", ty)
                    }
                }
            })
        })
        .chain(std::iter::once(if sig.c_variadic { Some("...".to_string()) } else { None }))
        .filter_map(|arg| arg)
        .collect::<Vec<String>>()
        .join(", ");
    let output = sig.output();
    let output = if !output.is_unit() { format!(" -> {}", output) } else { String::new() };

    let unsafety = sig.unsafety.prefix_str();
    let (generics, where_clauses) = bounds_from_generic_predicates(tcx, predicates);

    // FIXME: this is not entirely correct, as the lifetimes from borrowed params will
    // not be present in the `fn` definition, not will we account for renamed
    // lifetimes between the `impl` and the `trait`, but this should be good enough to
    // fill in a significant portion of the missing code, and other subsequent
    // suggestions can help the user fix the code.
    format!(
        "{}fn {}{}({}){}{} {{ todo!() }}",
        unsafety, ident, generics, args, output, where_clauses
    )
}

/// Return placeholder code for the given associated item.
/// Similar to `ty::AssocItem::suggestion`, but appropriate for use as the code snippet of a
/// structured suggestion.
fn suggestion_signature(assoc: &ty::AssocItem, tcx: TyCtxt<'_>) -> String {
    match assoc.kind {
        ty::AssocKind::Fn => {
            // We skip the binder here because the binder would deanonymize all
            // late-bound regions, and we don't want method signatures to show up
            // `as for<'r> fn(&'r MyType)`.  Pretty-printing handles late-bound
            // regions just fine, showing `fn(&MyType)`.
            fn_sig_suggestion(
                tcx,
                tcx.fn_sig(assoc.def_id).skip_binder(),
                assoc.ident,
                tcx.predicates_of(assoc.def_id),
                assoc,
            )
        }
        ty::AssocKind::Type => format!("type {} = Type;", assoc.ident),
        ty::AssocKind::Const => {
            let ty = tcx.type_of(assoc.def_id);
            let val = expr::ty_kind_suggestion(ty).unwrap_or("value");
            format!("const {}: {} = {};", assoc.ident, ty, val)
        }
    }
}

/// Emit an error when encountering more or less than one variant in a transparent enum.
fn bad_variant_count<'tcx>(tcx: TyCtxt<'tcx>, adt: &'tcx ty::AdtDef, sp: Span, did: DefId) {
    let variant_spans: Vec<_> = adt
        .variants
        .iter()
        .map(|variant| tcx.hir().span_if_local(variant.def_id).unwrap())
        .collect();
    let msg = format!("needs exactly one variant, but has {}", adt.variants.len(),);
    let mut err = struct_span_err!(tcx.sess, sp, E0731, "transparent enum {}", msg);
    err.span_label(sp, &msg);
    if let [start @ .., end] = &*variant_spans {
        for variant_span in start {
            err.span_label(*variant_span, "");
        }
        err.span_label(*end, &format!("too many variants in `{}`", tcx.def_path_str(did)));
    }
    err.emit();
}

/// Emit an error when encountering more or less than one non-zero-sized field in a transparent
/// enum.
fn bad_non_zero_sized_fields<'tcx>(
    tcx: TyCtxt<'tcx>,
    adt: &'tcx ty::AdtDef,
    field_count: usize,
    field_spans: impl Iterator<Item = Span>,
    sp: Span,
) {
    let msg = format!("needs exactly one non-zero-sized field, but has {}", field_count);
    let mut err = struct_span_err!(
        tcx.sess,
        sp,
        E0690,
        "{}transparent {} {}",
        if adt.is_enum() { "the variant of a " } else { "" },
        adt.descr(),
        msg,
    );
    err.span_label(sp, &msg);
    for sp in field_spans {
        err.span_label(sp, "this field is non-zero-sized");
    }
    err.emit();
}

fn report_unexpected_variant_res(tcx: TyCtxt<'_>, res: Res, span: Span) {
    struct_span_err!(
        tcx.sess,
        span,
        E0533,
        "expected unit struct, unit variant or constant, found {}{}",
        res.descr(),
        tcx.sess.source_map().span_to_snippet(span).map_or(String::new(), |s| format!(" `{}`", s)),
    )
    .emit();
}

/// Controls whether the arguments are tupled. This is used for the call
/// operator.
///
/// Tupling means that all call-side arguments are packed into a tuple and
/// passed as a single parameter. For example, if tupling is enabled, this
/// function:
///
///     fn f(x: (isize, isize))
///
/// Can be called as:
///
///     f(1, 2);
///
/// Instead of:
///
///     f((1, 2));
#[derive(Clone, Eq, PartialEq)]
enum TupleArgumentsFlag {
    DontTupleArguments,
    TupleArguments,
}

/// Controls how we perform fallback for unconstrained
/// type variables.
enum FallbackMode {
    /// Do not fallback type variables to opaque types.
    NoOpaque,
    /// Perform all possible kinds of fallback, including
    /// turning type variables to opaque types.
    All,
}

/// A wrapper for `InferCtxt`'s `in_progress_typeck_results` field.
#[derive(Copy, Clone)]
struct MaybeInProgressTables<'a, 'tcx> {
    maybe_typeck_results: Option<&'a RefCell<ty::TypeckResults<'tcx>>>,
}

impl<'a, 'tcx> MaybeInProgressTables<'a, 'tcx> {
    fn borrow(self) -> Ref<'a, ty::TypeckResults<'tcx>> {
        match self.maybe_typeck_results {
            Some(typeck_results) => typeck_results.borrow(),
            None => bug!(
                "MaybeInProgressTables: inh/fcx.typeck_results.borrow() with no typeck results"
            ),
        }
    }

    fn borrow_mut(self) -> RefMut<'a, ty::TypeckResults<'tcx>> {
        match self.maybe_typeck_results {
            Some(typeck_results) => typeck_results.borrow_mut(),
            None => bug!(
                "MaybeInProgressTables: inh/fcx.typeck_results.borrow_mut() with no typeck results"
            ),
        }
    }
}

struct CheckItemTypesVisitor<'tcx> {
    tcx: TyCtxt<'tcx>,
}

impl ItemLikeVisitor<'tcx> for CheckItemTypesVisitor<'tcx> {
    fn visit_item(&mut self, i: &'tcx hir::Item<'tcx>) {
        check_item_type(self.tcx, i);
    }
    fn visit_trait_item(&mut self, _: &'tcx hir::TraitItem<'tcx>) {}
    fn visit_impl_item(&mut self, _: &'tcx hir::ImplItem<'tcx>) {}
}

fn typeck_item_bodies(tcx: TyCtxt<'_>, crate_num: CrateNum) {
    debug_assert!(crate_num == LOCAL_CRATE);
    tcx.par_body_owners(|body_owner_def_id| {
        tcx.ensure().typeck(body_owner_def_id);
    });
}

fn fatally_break_rust(sess: &Session) {
    let handler = sess.diagnostic();
    handler.span_bug_no_panic(
        MultiSpan::new(),
        "It looks like you're trying to break rust; would you like some ICE?",
    );
    handler.note_without_error("the compiler expectedly panicked. this is a feature.");
    handler.note_without_error(
        "we would appreciate a joke overview: \
         https://github.com/rust-lang/rust/issues/43162#issuecomment-320764675",
    );
    handler.note_without_error(&format!(
        "rustc {} running on {}",
        option_env!("CFG_VERSION").unwrap_or("unknown_version"),
        config::host_triple(),
    ));
}

fn potentially_plural_count(count: usize, word: &str) -> String {
    format!("{} {}{}", count, word, pluralize!(count))
}
