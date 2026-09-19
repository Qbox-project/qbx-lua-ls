use lsp_types::{SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens, SemanticTokensLegend};
use qbx_fivem_data::native;
use qbx_lua_analysis::env::builtins;
use qbx_lua_analysis::scope::{LocalKind, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;

use crate::document::Document;
use crate::workspace::Workspace;

const TYPE_VARIABLE: u32 = 0;
const TYPE_PARAMETER: u32 = 1;
const TYPE_FUNCTION: u32 = 2;
const TYPE_METHOD: u32 = 3;
const TYPE_PROPERTY: u32 = 4;
const TYPE_NAMESPACE: u32 = 5;

const MOD_DECLARATION: u32 = 1 << 0;
const MOD_READONLY: u32 = 1 << 1;
const MOD_STATIC: u32 = 1 << 2;
const MOD_DEFAULT_LIBRARY: u32 = 1 << 3;
const MOD_MODIFICATION: u32 = 1 << 4;

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::METHOD,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::NAMESPACE,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::READONLY,
            SemanticTokenModifier::STATIC,
            SemanticTokenModifier::DEFAULT_LIBRARY,
            SemanticTokenModifier::MODIFICATION,
        ],
    }
}

struct Collector<'a> {
    doc: &'a Document,
    ws: &'a Workspace,
    raw: Vec<(Span, u32, u32)>,
}

impl Collector<'_> {
    fn name(&mut self, name: &Name, called: bool) {
        if name.is_missing() {
            return;
        }
        let Some(resolved) = self.doc.resolution.resolve_at(name.span.start) else { return };
        let (ty, modifiers) = match resolved {
            Resolved::Local(id) => {
                let local = self.doc.resolution.local(id);
                let mut modifiers = if local.decl == name.span { MOD_DECLARATION } else { 0 };
                if local.attrib.is_some() || !local.refs.iter().any(|r| r.write) {
                    modifiers |= MOD_READONLY;
                }
                let ty = match local.kind {
                    LocalKind::Param | LocalKind::ImplicitSelf => TYPE_PARAMETER,
                    LocalKind::LocalFunction => TYPE_FUNCTION,
                    _ if called => TYPE_FUNCTION,
                    _ => TYPE_VARIABLE,
                };
                (ty, modifiers)
            }
            Resolved::Global(index) => {
                let global = &self.doc.resolution.globals[index as usize];
                let mut modifiers = MOD_STATIC;
                if global.is_definition() {
                    modifiers |= MOD_MODIFICATION;
                }
                let user_defined = !self.ws.index.globals_named(&name.text, self.doc.file).is_empty()
                    && builtins().get(&name.text).is_none();
                let is_native = !user_defined && native(&name.text).is_some();
                let is_builtin = builtins().get(&name.text);
                if is_native || is_builtin.is_some() {
                    modifiers |= MOD_DEFAULT_LIBRARY;
                }
                let is_library_table = is_builtin.is_some_and(|b| !b.fields.is_empty());
                let ty = if is_library_table {
                    TYPE_NAMESPACE
                } else if called || is_native {
                    TYPE_FUNCTION
                } else {
                    TYPE_VARIABLE
                };
                (ty, modifiers)
            }
        };
        self.raw.push((name.span, ty, modifiers));
    }

    fn callee(&mut self, callee: &Expr) {
        match &callee.kind {
            ExprKind::Name(name) => self.name(name, true),
            ExprKind::Field { name, .. } if !name.is_missing() => self.raw.push((name.span, TYPE_FUNCTION, 0)),
            _ => {}
        }
    }
}

impl<'ast> Visitor<'ast> for Collector<'_> {
    fn visit_func_body(&mut self, func: &'ast FuncBody) {
        for param in &func.params {
            self.name(param, false);
        }
        visit::walk_func_body(self, func);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::Local { names, .. } => names.iter().for_each(|n| self.name(&n.name, false)),
            StmtKind::LocalFunction { name, .. } => self.name(name, true),
            StmtKind::Function { name, .. } => {
                let is_plain = name.path.is_empty() && name.method.is_none();
                self.name(&name.base, is_plain);
                let last = name.method.as_ref().or(name.path.last());
                for segment in &name.path {
                    let is_last = last.is_some_and(|l| std::ptr::eq(l, segment));
                    self.raw.push((segment.span, if is_last { TYPE_FUNCTION } else { TYPE_PROPERTY }, MOD_DECLARATION));
                }
                if let Some(method) = &name.method {
                    self.raw.push((method.span, TYPE_METHOD, MOD_DECLARATION));
                }
            }
            StmtKind::NumericFor { var, .. } => self.name(var, false),
            StmtKind::GenericFor { names, .. } => names.iter().for_each(|n| self.name(n, false)),
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Name(name) => self.name(name, false),
            ExprKind::Call { callee, args, .. } => {
                self.callee(callee);
                match &callee.kind {
                    ExprKind::Name(_) => {}
                    ExprKind::Field { base, .. } => self.visit_expr(base),
                    _ => self.visit_expr(callee),
                }
                args.iter().for_each(|a| self.visit_expr(a));
                return;
            }
            ExprKind::MethodCall { method, .. } if !method.is_missing() => self.raw.push((method.span, TYPE_METHOD, 0)),
            ExprKind::Field { name, .. } if !name.is_missing() => self.raw.push((name.span, TYPE_PROPERTY, 0)),
            ExprKind::Table(fields) => {
                for field in fields {
                    if let TableField::Named { name, .. } | TableField::SetMember(name) = field {
                        self.raw.push((name.span, TYPE_PROPERTY, MOD_DECLARATION));
                    }
                }
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

pub fn semantic_tokens(ws: &Workspace, doc: &Document) -> SemanticTokens {
    let mut collector = Collector { doc, ws, raw: Vec::new() };
    collector.visit_block(&doc.chunk.block);
    collector.raw.sort_by_key(|(span, ..)| span.start);
    collector.raw.dedup_by_key(|(span, ..)| span.start);

    let mut data = Vec::with_capacity(collector.raw.len());
    let (mut prev_line, mut prev_col) = (0u32, 0u32);
    for (span, token_type, modifiers) in collector.raw {
        let start = doc.position(span.start);
        let end = doc.position(span.end);
        if end.line != start.line || end.character <= start.character {
            continue;
        }
        let delta_line = start.line - prev_line;
        let delta_start = if delta_line == 0 { start.character - prev_col } else { start.character };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: end.character - start.character,
            token_type,
            token_modifiers_bitset: modifiers,
        });
        prev_line = start.line;
        prev_col = start.character;
    }
    SemanticTokens { result_id: None, data }
}
