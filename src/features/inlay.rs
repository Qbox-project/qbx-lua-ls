use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Range};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;

use super::event_call::event_call;
use super::with_infer;
use crate::document::Document;
use crate::infer::Infer;
use crate::workspace::Workspace;

const MAX_HINTS: usize = 400;

struct Hints<'a, 'b> {
    doc: &'a Document,
    infer: &'a Infer<'b>,
    ws: &'a Workspace,
    range: Span,
    out: Vec<InlayHint>,
}

fn is_literal(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Nil | ExprKind::True | ExprKind::False | ExprKind::Number(_) | ExprKind::String(_) => true,
        ExprKind::Unary { op: UnOp::Neg, expr } => matches!(expr.kind, ExprKind::Number(_)),
        ExprKind::Table(fields) => fields.is_empty(),
        _ => false,
    }
}

impl Hints<'_, '_> {
    fn call(&mut self, base: &Expr, method: Option<&Name>, args: &[Expr]) {
        if self.out.len() >= MAX_HINTS || !args.iter().any(is_literal) {
            return;
        }
        let native = self.infer.callee_fun(base, method).map(|(fun, _)| fun);
        let event = method
            .is_none()
            .then(|| event_call(self.ws, self.doc, self.infer, base, args, native.as_deref()))
            .flatten();
        let Some(fun) = event.map(|e| e.fun.into()).or(native) else { return };
        let (skip_params, skip_args) = fun.call_offsets(method.is_some());
        for (arg, param) in args.iter().skip(skip_args).zip(fun.params.iter().skip(skip_params)) {
            if !is_literal(arg) || param.name == "..." || param.name.is_empty() || !self.range.contains(arg.span.start)
            {
                continue;
            }
            self.out.push(InlayHint {
                position: self.doc.position(arg.span.start),
                label: InlayHintLabel::String(format!("{}:", param.name)),
                kind: Some(InlayHintKind::PARAMETER),
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: Some(true),
                data: None,
            });
        }
    }
}

impl<'ast> Visitor<'ast> for Hints<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if stmt.span.end >= self.range.start && stmt.span.start <= self.range.end {
            visit::walk_stmt(self, stmt);
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Call { callee, args, style: CallStyle::Paren, .. } => self.call(callee, None, args),
            ExprKind::MethodCall { base, method, args, style: CallStyle::Paren, .. } => {
                self.call(base, Some(method), args)
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

pub fn inlay_hints(ws: &Workspace, doc: &Document, range: Range) -> Vec<InlayHint> {
    let span = Span::new(doc.offset(range.start), doc.offset(range.end));
    with_infer(ws, doc, |infer| {
        let mut hints = Hints { doc, infer, ws, range: span, out: Vec::new() };
        hints.visit_block(&doc.chunk.block);
        hints.out
    })
}
