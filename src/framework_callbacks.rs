//! Small callback conventions backed by proven local framework receivers, never a docs catalog.
use qbx_fivem_data::Side;
use qbx_lua_analysis::scope::{Resolution, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};

use crate::index::{EventFamily, EventKind, FileOrigin, Index};
use crate::infer::{Decl, FileContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameworkCallbackCall {
    pub family: EventFamily,
    pub kind: EventKind,
}

impl FrameworkCallbackCall {
    pub fn required_side(self) -> Side {
        if self.kind == EventKind::Callback {
            Side::Server
        } else {
            Side::Client
        }
    }
}

/// Recognizes only the four standard dot calls. Callers check the effective manifest/guard side.
pub fn classify(ctx: &FileContext<'_>, index: &Index, callee: &Expr) -> Option<FrameworkCallbackCall> {
    let ExprKind::Field { base, name, safe: false } = &callee.kind else { return None };
    let (root, family, kind, path): (&Name, _, _, &[&str]) = match name.text.as_str() {
        "CreateCallback" | "TriggerCallback" => {
            let ExprKind::Field { base, name: functions, safe: false } = &base.kind else { return None };
            if functions.text != "Functions" {
                return None;
            }
            let ExprKind::Name(root) = &base.kind else { return None };
            let kind = if name.text == "CreateCallback" { EventKind::Callback } else { EventKind::Trigger };
            (root, EventFamily::QbCore, kind, &["Functions", name.text.as_str()])
        }
        "RegisterServerCallback" | "TriggerServerCallback" => {
            let ExprKind::Name(root) = &base.kind else { return None };
            let kind = if name.text == "RegisterServerCallback" { EventKind::Callback } else { EventKind::Trigger };
            (root, EventFamily::Esx, kind, &[name.text.as_str()])
        }
        _ => return None,
    };
    if !standard_environment(ctx, index, root.span.start) || receiver_family(ctx, index, root, path, 0)? != family {
        return None;
    }
    Some(FrameworkCallbackCall { family, kind })
}

fn standard_environment(ctx: &FileContext<'_>, index: &Index, offset: u32) -> bool {
    ctx.resolution.lookup_local_at("_ENV", offset).is_none() && !global_redefined(ctx, index, "_ENV")
}

fn global_redefined(ctx: &FileContext<'_>, index: &Index, name: &str) -> bool {
    ctx.resolution.globals.iter().any(|global| global.name == name && global.is_definition())
        || global_field_redefined(ctx, name)
        || index.globals_named(name, ctx.file).iter().any(|(file, _)| {
            *file != ctx.file && index.file(*file).is_some_and(|entry| entry.origin != FileOrigin::Stub)
        })
}

fn global_table(ctx: &FileContext<'_>, root: &Name) -> bool {
    matches!(root.text.as_str(), "_G" | "_ENV")
        && matches!(ctx.resolution.resolve_at(root.span.start), Some(Resolved::Global(_)))
}

pub(crate) fn global_field_redefined(ctx: &FileContext<'_>, name: &str) -> bool {
    struct Writes<'a, 'b> {
        ctx: &'a FileContext<'b>,
        name: &'a str,
        found: bool,
    }
    impl Writes<'_, '_> {
        fn target(&mut self, target: &Expr) {
            let (base, field) = match &target.kind {
                ExprKind::Field { base, name, .. } => (base.as_ref(), Some(name.text.as_str())),
                ExprKind::Index { base, index, .. } => (base.as_ref(), index.as_string().map(|name| name.as_str())),
                _ => return,
            };
            if let ExprKind::Name(root) = &base.kind {
                if global_table(self.ctx, root) && field.is_none_or(|field| field == self.name) {
                    self.found = true;
                }
            }
        }
    }
    impl<'ast> Visitor<'ast> for Writes<'_, '_> {
        fn visit_stmt(&mut self, stmt: &'ast Stmt) {
            match &stmt.kind {
                StmtKind::Assign { targets, .. } => targets.iter().for_each(|target| self.target(target)),
                StmtKind::CompoundAssign { target, .. } => self.target(target),
                StmtKind::Function { name, .. } if global_table(self.ctx, &name.base) => {
                    let mut fields = name.path.iter().chain(name.method.iter());
                    if fields.next().is_some_and(|field| field.text == self.name) && fields.next().is_none() {
                        self.found = true;
                    }
                }
                _ => {}
            }
            if !self.found {
                visit::walk_stmt(self, stmt);
            }
        }
    }
    let mut writes = Writes { ctx, name, found: false };
    writes.visit_block(&ctx.chunk.block);
    writes.found
}

fn receiver_family(ctx: &FileContext<'_>, index: &Index, root: &Name, path: &[&str], depth: u8) -> Option<EventFamily> {
    if depth >= 8 || mutated(ctx, root, path) {
        return None;
    }
    match ctx.resolution.resolve_at(root.span.start)? {
        Resolved::Local(id) => {
            let local = ctx.resolution.local(id);
            if local.refs.iter().any(|reference| reference.write) {
                return None;
            }
            let Decl::Local { stmt, index: position } = ctx.decl(local.decl.start)? else { return None };
            let StmtKind::Local { exprs, in_unpack: false, .. } = &stmt.kind else { return None };
            match &exprs.get(*position)?.unparen().kind {
                ExprKind::Name(alias) => receiver_family(ctx, index, alias, path, depth + 1),
                _ => export_receiver(ctx, index, exprs.get(*position)?.unparen()),
            }
        }
        Resolved::Global(_) => framework_global(ctx, index, root, path),
    }
}

fn export_receiver(ctx: &FileContext<'_>, index: &Index, value: &Expr) -> Option<EventFamily> {
    let ExprKind::MethodCall { base, method, args, safe: false, .. } = &value.kind else { return None };
    if !args.is_empty() {
        return None;
    }
    let (exports, resource) = match &base.kind {
        ExprKind::Field { base, name, safe: false } => (base.as_ref(), name.text.as_str()),
        ExprKind::Index { base, index, safe: false } => (base.as_ref(), index.as_string()?.as_str()),
        _ => return None,
    };
    let ExprKind::Name(exports) = &exports.kind else { return None };
    if exports.text != "exports"
        || !matches!(ctx.resolution.resolve_at(exports.span.start), Some(Resolved::Global(_)))
        || !standard_environment(ctx, index, exports.span.start)
        || global_redefined(ctx, index, "exports")
        || mutated(ctx, exports, &[resource, method.text.as_str()])
    {
        return None;
    }
    match (resource, method.text.as_str()) {
        ("qb-core", "GetCoreObject") => Some(EventFamily::QbCore),
        ("es_extended", "getSharedObject") => Some(EventFamily::Esx),
        _ => None,
    }
}

fn framework_global(ctx: &FileContext<'_>, index: &Index, root: &Name, path: &[&str]) -> Option<EventFamily> {
    if global_field_redefined(ctx, &root.text) {
        return None;
    }
    let resource = index.resource_of(ctx.file)?;
    let (provider, family) = match root.text.as_str() {
        "QBCore" => ("qb-core", EventFamily::QbCore),
        "ESX" => ("es_extended", EventFamily::Esx),
        _ => return None,
    };
    // A canonical global in the provider's own files is supported only when its source is indexed.
    if resource.name == provider {
        let writes: Vec<_> =
            ctx.resolution.globals.iter().filter(|global| global.name == root.text && global.is_definition()).collect();
        match writes.as_slice() {
            [] => {}
            [write]
                if ctx.chunk.block.stmts.iter().any(|stmt| {
                    let StmtKind::Assign { targets, exprs } = &stmt.kind else { return false };
                    matches!((targets.as_slice(), exprs.as_slice()), ([target], [value])
                if matches!(&target.kind, ExprKind::Name(name) if name.span == write.span)
                    && matches!(value.kind, ExprKind::Table(_)))
                }) => {}
            _ => return None,
        }
        return index
            .globals_named(&root.text, ctx.file)
            .iter()
            .any(|(file, _)| index.resource_of(*file).is_some_and(|entry| entry.name == provider))
            .then_some(family);
    }
    if family != EventFamily::Esx
        || ctx.resolution.globals.iter().any(|global| global.name == root.text && global.is_definition())
    {
        return None;
    }
    let side = index.file(ctx.file)?.side;
    let effective =
        qbx_lua_analysis::side_guard::SideRegions::of(ctx.source, ctx.chunk).effective(root.span.start, side)?;
    if !resource.manifest.imports_path("@es_extended/imports.lua", effective) {
        return None;
    }
    // Imported ESX must not be replaced or have the relevant member defined by this resource.
    if index
        .globals_named(&root.text, ctx.file)
        .iter()
        .any(|(file, _)| *file != ctx.file && index.resource_of(*file).is_some_and(|entry| entry.name != provider))
    {
        return None;
    }
    let owner = if path.len() > 1 {
        format!("{}.{}", root.text, path[..path.len() - 1].join("."))
    } else {
        root.text.to_string()
    };
    if index.members_of(&owner, ctx.file).iter().any(|(file, member)| {
        *file != ctx.file
            && path.last().is_some_and(|last| member.name == *last)
            && index.resource_of(*file).is_some_and(|entry| entry.name != provider)
    }) {
        return None;
    }
    Some(family)
}

fn same_binding(resolution: &Resolution, first: &Name, other: &Name) -> bool {
    match (resolution.resolve_at(first.span.start), resolution.resolve_at(other.span.start)) {
        (Some(Resolved::Local(a)), Some(Resolved::Local(b))) => a == b,
        (Some(Resolved::Global(_)), Some(Resolved::Global(_))) => first.text == other.text,
        _ => false,
    }
}

fn same_binding_or_alias(ctx: &FileContext<'_>, first: &Name, other: &Name, depth: u8) -> bool {
    if same_binding(ctx.resolution, first, other) {
        return true;
    }
    if depth >= 8 {
        return false;
    }
    let Some(Resolved::Local(id)) = ctx.resolution.resolve_at(other.span.start) else { return false };
    let local = ctx.resolution.local(id);
    let Some(Decl::Local { stmt, index: position }) = ctx.decl(local.decl.start) else { return false };
    let StmtKind::Local { exprs, in_unpack: false, .. } = &stmt.kind else { return false };
    match exprs.get(*position).map(|value| &value.unparen().kind) {
        Some(ExprKind::Name(alias)) => same_binding_or_alias(ctx, first, alias, depth + 1),
        _ => false,
    }
}

fn mutated(ctx: &FileContext<'_>, root: &Name, path: &[&str]) -> bool {
    struct Mutation<'a, 'b> {
        ctx: &'a FileContext<'b>,
        root: &'a Name,
        path: &'a [&'a str],
        found: bool,
    }
    impl Mutation<'_, '_> {
        fn target(&mut self, target: &Expr) {
            fn split<'a>(expr: &'a Expr, fields: &mut Vec<Option<&'a str>>) -> Option<&'a Name> {
                match &expr.kind {
                    ExprKind::Name(name) => Some(name),
                    ExprKind::Field { base, name, .. } => {
                        let root = split(base, fields)?;
                        fields.push(Some(&name.text));
                        Some(root)
                    }
                    ExprKind::Index { base, index, .. } => {
                        let root = split(base, fields)?;
                        fields.push(index.as_string().map(|key| key.as_str()));
                        Some(root)
                    }
                    _ => None,
                }
            }
            let mut fields = Vec::new();
            if let Some(root) = split(target, &mut fields) {
                self.check(root, &fields);
            }
        }

        fn check(&mut self, root: &Name, fields: &[Option<&str>]) {
            if matches!(self.ctx.resolution.resolve_at(self.root.span.start), Some(Resolved::Global(_)))
                && global_table(self.ctx, root)
                && fields.first().is_some_and(|field| field.is_none_or(|field| field == self.root.text))
            {
                let fields = &fields[1..];
                if fields.len() <= self.path.len()
                    && fields.iter().zip(self.path).all(|(field, wanted)| field.is_none_or(|field| field == *wanted))
                {
                    self.found = true;
                }
            }
            if !fields.is_empty()
                && same_binding_or_alias(self.ctx, self.root, root, 0)
                && fields.len() <= self.path.len()
                && fields.iter().zip(self.path).all(|(field, wanted)| field.is_none_or(|field| field == *wanted))
            {
                self.found = true;
            }
        }
    }
    impl<'ast> Visitor<'ast> for Mutation<'_, '_> {
        fn visit_stmt(&mut self, stmt: &'ast Stmt) {
            match &stmt.kind {
                StmtKind::Assign { targets, .. } => targets.iter().for_each(|target| self.target(target)),
                StmtKind::CompoundAssign { target, .. } => self.target(target),
                StmtKind::Function { name, .. } => {
                    let fields: Vec<_> =
                        name.path.iter().chain(name.method.iter()).map(|name| Some(name.text.as_str())).collect();
                    self.check(&name.base, &fields);
                }
                _ => {}
            }
            if !self.found {
                visit::walk_stmt(self, stmt);
            }
        }
    }
    let mut scan = Mutation { ctx, root, path, found: false };
    scan.visit_block(&ctx.chunk.block);
    scan.found
}

#[cfg(test)]
mod tests {
    use super::*;
    use qbx_lua_analysis::scope::resolve;
    use qbx_lua_syntax::parse;

    fn last_call(source: &str) -> Option<FrameworkCallbackCall> {
        let chunk = parse(source);
        assert!(chunk.errors.is_empty(), "{source}");
        let resolution = resolve(&chunk);
        let ctx = FileContext::new(0, source, &chunk, &resolution);
        let StmtKind::Expr(expr) = &chunk.block.stmts.last()?.kind else { return None };
        let ExprKind::Call { callee, .. } = &expr.kind else { return None };
        classify(&ctx, &Index::default(), callee)
    }

    #[test]
    fn recognizes_export_initialized_receivers_and_local_aliases() {
        let qb = "local core = exports['qb-core']:GetCoreObject()\nlocal alias = core\nalias.Functions.CreateCallback('x', function() end)";
        assert_eq!(
            last_call(qb),
            Some(FrameworkCallbackCall { family: EventFamily::QbCore, kind: EventKind::Callback })
        );
        let esx = "local framework = exports.es_extended:getSharedObject()\nframework.TriggerServerCallback('x', function() end)";
        assert_eq!(last_call(esx), Some(FrameworkCallbackCall { family: EventFamily::Esx, kind: EventKind::Trigger }));
    }

    #[test]
    fn rejects_shadowed_reassigned_and_unrelated_framework_shapes() {
        for prefix in [
            "local core = {}",
            "local exports = {}; local core = exports['qb-core']:GetCoreObject()",
            "exports = {}; local core = exports['qb-core']:GetCoreObject()",
            "_G.exports = {}; local core = exports['qb-core']:GetCoreObject()",
            "_ENV['exports'] = {}; local core = exports['qb-core']:GetCoreObject()",
            "_G.exports['qb-core'].GetCoreObject = function() end; local core = exports['qb-core']:GetCoreObject()",
            "local _ENV = {}; local core = exports['qb-core']:GetCoreObject()",
            "_ENV = {}; local core = exports['qb-core']:GetCoreObject()",
            "local core = exports['qb-core']:GetCoreObject(); core = {}",
            "local core = exports['qb-core']:GetCoreObject(); core.Functions = {}",
            "local core = exports['qb-core']:GetCoreObject(); core.Functions.CreateCallback = function() end",
            "local core = exports['qb-core']:GetCoreObject(); function core.Functions.CreateCallback() end",
            "local core = exports['qb-core']:GetCoreObject(); local alias = core; alias.Functions = {}",
            "local core = exports['qb-core']:GetCoreObject(); core[key] = {}",
            "local core = exports['other']:GetCoreObject()",
            "local core = exports['qb-core']:GetCoreObject({'Functions'})",
        ] {
            let source = format!("{prefix}\ncore.Functions.CreateCallback('x', function() end)");
            assert!(last_call(&source).is_none(), "{source}");
        }
    }
}
