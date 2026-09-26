//! Conservative recognition of the two Lua NUI registration globals.
use qbx_lua_analysis::scope::Resolved;
use qbx_lua_syntax::ast::{Expr, ExprKind};

use crate::framework_callbacks::global_field_redefined;
use crate::infer::FileContext;

pub(crate) const REGISTRATION_GLOBALS: [&str; 2] = ["RegisterNUICallback", "RegisterNuiCallback"];

/// File-wide mutations are checked once while indexing, not once per callback or on requests.
pub(crate) struct NuiGlobals([bool; 2]);

impl NuiGlobals {
    pub(crate) fn of(ctx: &FileContext<'_>) -> Self {
        if !REGISTRATION_GLOBALS.iter().any(|name| ctx.source.contains(name)) {
            return Self([false; 2]);
        }
        let redefined = |name: &str| {
            ctx.resolution.globals.iter().any(|global| global.name == name && global.is_definition())
                || global_field_redefined(ctx, name)
        };
        if redefined("_ENV") {
            return Self([false; 2]);
        }
        Self(REGISTRATION_GLOBALS.map(|name| !redefined(name)))
    }

    pub(crate) fn registration<'a>(&self, ctx: &FileContext<'_>, callee: &'a Expr) -> Option<&'a str> {
        let ExprKind::Name(name) = &callee.kind else { return None };
        let position = REGISTRATION_GLOBALS.iter().position(|global| *global == name.text)?;
        (self.0[position]
            && matches!(ctx.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
            && ctx.resolution.lookup_local_at("_ENV", name.span.start).is_none())
        .then_some(name.text.as_str())
    }
}
