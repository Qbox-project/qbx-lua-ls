use lsp_types::{DocumentSymbol, Location, SymbolInformation, SymbolKind as LspKind};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::Span;

use crate::document::Document;
use crate::index::{FileOrigin, SymbolKind};
use crate::workspace::Workspace;

const MAX_WORKSPACE_SYMBOLS: usize = 300;

#[allow(deprecated)]
fn symbol(
    doc: &Document,
    name: String,
    detail: Option<String>,
    kind: LspKind,
    full: Span,
    selection: Span,
    children: Vec<DocumentSymbol>,
) -> DocumentSymbol {
    let selection = if full.contains_span(selection) { selection } else { full };
    DocumentSymbol {
        name: if name.is_empty() { "<anonymous>".into() } else { name },
        detail,
        kind,
        tags: None,
        deprecated: None,
        range: doc.range(full),
        selection_range: doc.range(selection),
        children: (!children.is_empty()).then_some(children),
    }
}

fn params_detail(func: &FuncBody) -> String {
    let mut params: Vec<&str> = func.params.iter().map(|p| p.text.as_str()).collect();
    if func.vararg.is_some() {
        params.push("...");
    }
    format!("({})", params.join(", "))
}

fn block_symbols(doc: &Document, block: &Block, out: &mut Vec<DocumentSymbol>) {
    for stmt in &block.stmts {
        match &stmt.kind {
            StmtKind::Function { name, func } => {
                let mut label = name.base.text.to_string();
                for segment in &name.path {
                    label.push('.');
                    label.push_str(&segment.text);
                }
                if let Some(method) = &name.method {
                    label.push(':');
                    label.push_str(&method.text);
                }
                let kind = if name.method.is_some() { LspKind::METHOD } else { LspKind::FUNCTION };
                let mut children = Vec::new();
                block_symbols(doc, &func.body, &mut children);
                out.push(symbol(doc, label, Some(params_detail(func)), kind, stmt.span, name.span, children));
            }
            StmtKind::LocalFunction { name, func } => {
                let mut children = Vec::new();
                block_symbols(doc, &func.body, &mut children);
                out.push(symbol(
                    doc,
                    name.text.to_string(),
                    Some(params_detail(func)),
                    LspKind::FUNCTION,
                    stmt.span,
                    name.span,
                    children,
                ));
            }
            StmtKind::Local { names, exprs, .. } => {
                for (i, name) in names.iter().enumerate() {
                    out.push(value_symbol(doc, name.name.text.to_string(), name.name.span, stmt.span, exprs.get(i)));
                }
            }
            StmtKind::Assign { targets, exprs } => {
                for (i, target) in targets.iter().enumerate() {
                    if let Some(path) = target.dotted_path() {
                        out.push(value_symbol(doc, path, target.span, stmt.span, exprs.get(i)));
                    }
                }
            }
            StmtKind::Expr(expr) => call_symbols(doc, expr, stmt.span, out),
            StmtKind::Do(body) | StmtKind::While { body, .. } | StmtKind::Repeat { body, .. } => {
                block_symbols(doc, body, out)
            }
            StmtKind::NumericFor { body, .. } | StmtKind::GenericFor { body, .. } => block_symbols(doc, body, out),
            StmtKind::If { branches, else_block } => {
                branches.iter().for_each(|b| block_symbols(doc, &b.block, out));
                if let Some(block) = else_block {
                    block_symbols(doc, block, out);
                }
            }
            _ => {}
        }
    }
}

fn value_symbol(doc: &Document, name: String, selection: Span, full: Span, value: Option<&Expr>) -> DocumentSymbol {
    let mut children = Vec::new();
    let (kind, detail) = match value.map(|v| &v.kind) {
        Some(ExprKind::Function(func)) => {
            block_symbols(doc, &func.body, &mut children);
            (LspKind::FUNCTION, Some(params_detail(func)))
        }
        Some(ExprKind::Table(fields)) => {
            for field in fields.iter().take(200) {
                if let TableField::Named { name, value } = field {
                    children.push(value_symbol(
                        doc,
                        name.text.to_string(),
                        name.span,
                        name.span.to(value.span),
                        Some(value),
                    ));
                }
            }
            (LspKind::OBJECT, None)
        }
        Some(ExprKind::String(_)) => (LspKind::STRING, None),
        Some(ExprKind::Number(_)) => (LspKind::NUMBER, None),
        Some(ExprKind::True | ExprKind::False) => (LspKind::BOOLEAN, None),
        _ => (LspKind::VARIABLE, None),
    };
    symbol(doc, name, detail, kind, full, selection, children)
}

/// Event handlers, threads, commands and exports are the landmarks of a FiveM script, so they
/// show up in the outline even though they are plain calls.
fn call_symbols(doc: &Document, expr: &Expr, full: Span, out: &mut Vec<DocumentSymbol>) {
    let ExprKind::Call { callee, args, .. } = &expr.kind else { return };
    let Some(path) = callee.dotted_path() else { return };
    let label = match path.as_str() {
        "RegisterNetEvent"
        | "AddEventHandler"
        | "RegisterServerEvent"
        | "RegisterCommand"
        | "RegisterNUICallback"
        | "exports"
        | "lib.callback.register" => {
            let Some(name) = args.first().and_then(|a| a.as_string()) else { return };
            format!("{path} '{name}'")
        }
        "CreateThread" | "Citizen.CreateThread" | "SetTimeout" => path.clone(),
        _ => return,
    };
    let mut children = Vec::new();
    for arg in args {
        if let ExprKind::Function(func) = &arg.kind {
            block_symbols(doc, &func.body, &mut children);
        }
    }
    out.push(symbol(doc, label, None, LspKind::EVENT, full, callee.span, children));
}

pub fn document_symbols(doc: &Document) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    block_symbols(doc, &doc.chunk.block, &mut out);
    out
}

#[allow(deprecated)]
pub fn workspace_symbols(ws: &Workspace, query: &str) -> Vec<SymbolInformation> {
    let query = query.to_ascii_lowercase();
    let matches = |name: &str| query.is_empty() || name.to_ascii_lowercase().contains(&query);
    let mut out = Vec::new();
    for (_, file) in ws.index.files().filter(|(_, f)| f.origin != FileOrigin::Stub) {
        let mut push = |name: String, kind: LspKind, range, container: Option<String>| {
            if out.len() < MAX_WORKSPACE_SYMBOLS {
                out.push(SymbolInformation {
                    name,
                    kind,
                    tags: None,
                    deprecated: None,
                    location: Location::new(file.uri.clone(), range),
                    container_name: container,
                });
            }
        };
        for symbol in file.index.globals.iter().filter(|s| matches(&s.name)) {
            let kind = if symbol.ty.as_fun().is_some() { LspKind::FUNCTION } else { LspKind::VARIABLE };
            push(symbol.name.to_string(), kind, symbol.range, None);
        }
        for member in file.index.members.iter().filter(|m| matches(&m.symbol.name) && !m.owner.starts_with('%')) {
            let kind = match member.symbol.kind {
                SymbolKind::Method => LspKind::METHOD,
                _ if member.symbol.ty.as_fun().is_some() => LspKind::FUNCTION,
                _ => LspKind::FIELD,
            };
            push(
                format!("{}.{}", member.owner, member.symbol.name),
                kind,
                member.symbol.range,
                Some(member.owner.to_string()),
            );
        }
        for class in file.index.classes.iter().filter(|c| matches(&c.name)) {
            push(class.name.to_string(), LspKind::CLASS, class.range, None);
        }
        for export in file.index.exports.iter().filter(|e| matches(&e.name)) {
            push(format!("exports:{}", export.name), LspKind::INTERFACE, export.range, None);
        }
        for event in file.index.events.iter().filter(|e| e.kind != crate::index::EventKind::Trigger && matches(&e.name))
        {
            push(event.name.to_string(), LspKind::EVENT, event.range, None);
        }
        if out.len() >= MAX_WORKSPACE_SYMBOLS {
            break;
        }
    }
    out
}
