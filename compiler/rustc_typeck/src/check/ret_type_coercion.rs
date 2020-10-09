use super::coercion::DynamicCoerceMany;

use rustc_middle::ty::Ty;
use rustc_span::{self, Span};

use std::cell::RefCell;

#[derive(Default)]
pub(super) struct RetTypeCoercion<'tcx> {
    /// If `Some`, this stores coercion information for returned
    /// expressions. If `None`, this is in a context where return is
    /// inappropriate, such as a const expression.
    ///
    /// This is a `RefCell<DynamicCoerceMany>`, which means that we
    /// can track all the return expressions and then use them to
    /// compute a useful coercion from the set, similar to a match
    /// expression or other branching context. You can use methods
    /// like `expected_ty` to access the declared return type (if
    /// any).
    pub(super) coercion: Option<RefCell<DynamicCoerceMany<'tcx>>>,

    pub(super) coercion_impl_trait: Option<Ty<'tcx>>,

    pub(super) type_span: Option<Span>,

    /// First span of a return site that we find. Used in error messages.
    pub(super) coercion_span: RefCell<Option<Span>>,
}
