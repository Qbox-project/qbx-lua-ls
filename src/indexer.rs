use std::fmt::Write as _;
use std::sync::Arc;

use lsp_types::{Position, Range};
use qbx_fivem_data::Side;
use qbx_lua_analysis::scope::{Resolution, Resolved};
use qbx_lua_analysis::side_guard::SideRegions;
use qbx_lua_analysis::summary::summarize;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::{Comment, LineIndex, SmolStr, Span};

use crate::index::{
    AliasDef, ClassDef, Element, EventDef, EventKind, FileId, FileIndex, Index, Member, Symbol, SymbolKind,
};
use crate::infer::{table_fields, FileContext, Infer};
use crate::luacats::{parse_doc_lines, DocGroup};
use crate::types::Type;

const MAX_TABLE_DEPTH: u32 = 4;
const MAX_TABLE_FIELDS: usize = 400;
const MAX_MEMBERS_PER_FILE: usize = 6000;

pub fn span_to_range(source: &str, lines: &LineIndex, span: Span) -> Range {
    let start = lines.line_col_utf16(source, span.start);
    let end = lines.line_col_utf16(source, span.end.max(span.start));
    Range::new(Position::new(start.line, start.col), Position::new(end.line, end.col))
}

pub fn render_doc(doc: &DocGroup) -> Option<Arc<str>> {
    let mut out = doc.description.clone();
    if let Some(reason) = &doc.deprecated {
        let _ = write!(out, "\n\n**Deprecated** {reason}");
    }
    let documented: Vec<_> = doc.params.iter().filter(|p| !p.description.is_empty()).collect();
    if !documented.is_empty() {
        out.push('\n');
        for param in documented {
            let _ = write!(out, "\n- `{}`: {}", param.name, param.description);
        }
    }
    for ret in doc.returns.iter().filter(|r| !r.description.is_empty()) {
        let name = ret.name.as_deref().unwrap_or("returns");
        let _ = write!(out, "\n\n*{name}*: {}", ret.description);
    }
    let out = out.trim();
    (!out.is_empty()).then(|| Arc::from(out))
}

pub fn index_file(
    file: FileId,
    source: &str,
    chunk: &Chunk,
    resolution: &Resolution,
    index: &Index,
    side: Option<Side>,
) -> FileIndex {
    let ctx = FileContext::new(file, source, chunk, resolution);
    let infer = Infer::new(&ctx, index);
    let lines = LineIndex::new(source);
    let mut indexer = Indexer {
        file,
        source,
        lines: &lines,
        ctx: &ctx,
        infer: &infer,
        side,
        regions: SideRegions::of(source, chunk),
        out: FileIndex::default(),
        depth: 0,
    };
    indexer.doc_comments(&chunk.comments);
    indexer.block(&chunk.block);
    if let Some(Stmt { kind: StmtKind::Return(exprs), .. }) = chunk.block.stmts.last() {
        indexer.module_return(exprs);
    }
    let mut out = indexer.out;
    out.summary = summarize(chunk, resolution);
    out
}

struct Indexer<'a> {
    file: FileId,
    source: &'a str,
    lines: &'a LineIndex,
    ctx: &'a FileContext<'a>,
    infer: &'a Infer<'a>,
    side: Option<Side>,
    regions: SideRegions,
    out: FileIndex,
    depth: u32,
}

pub const CONVAR_CALLS: &[&str] = &[
    "GetConvar",
    "GetConvarInt",
    "GetConvarBool",
    "GetConvarFloat",
    "SetConvar",
    "SetConvarReplicated",
    "SetConvarServerInfo",
];

/// `Entity(x).state`, `LocalPlayer.state`, `Player(src).state` and `GlobalState`.
pub fn is_state_bag(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Field { name, .. } => name.text == "state",
        ExprKind::Name(name) => name.text == "GlobalState",
        _ => false,
    }
}

const NET_EVENT_CALLS: &[&str] = &["RegisterNetEvent", "RegisterServerEvent"];
const HANDLER_CALLS: &[&str] = &["AddEventHandler"];
const CALLBACK_CALLS: &[&str] = &["lib.callback.register"];
const TRIGGER_CALLS: &[&str] = &[
    "TriggerEvent",
    "TriggerServerEvent",
    "TriggerClientEvent",
    "TriggerLatentServerEvent",
    "TriggerLatentClientEvent",
    "lib.callback",
    "lib.callback.await",
];

impl<'a> Indexer<'a> {
    fn range(&self, span: Span) -> Range {
        span_to_range(self.source, self.lines, span)
    }

    fn is_global(&self, name: &Name) -> bool {
        matches!(self.ctx.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
    }

    /// Groups adjacent `---` comments and records the classes and aliases they declare.
    fn doc_comments(&mut self, comments: &[Comment]) {
        let mut group: Vec<&Comment> = Vec::new();
        for comment in comments {
            let text = comment.span.text(self.source);
            let adjacent = group.last().is_some_and(|prev: &&Comment| {
                let gap = &self.source[prev.span.end as usize..comment.span.start as usize];
                gap.bytes().filter(|b| *b == b'\n').count() <= 1 && gap.trim().is_empty()
            });
            if !adjacent {
                self.flush_doc_group(&group);
                group.clear();
            }
            if text.starts_with("---") {
                group.push(comment);
            } else {
                self.flush_doc_group(&group);
                group.clear();
            }
        }
        self.flush_doc_group(&group);
    }

    fn flush_doc_group(&mut self, group: &[&Comment]) {
        if !group.iter().any(|c| c.span.text(self.source).contains('@')) {
            return;
        }
        let lines: Vec<&str> =
            group.iter().map(|c| c.span.text(self.source).strip_prefix("---").unwrap_or_default()).collect();
        let doc = parse_doc_lines(&lines);
        for class in doc.classes {
            let range = self.range(group[class.line.min(group.len() - 1)].span);
            let fields = class
                .fields
                .into_iter()
                .map(|field| Symbol {
                    kind: if field.ty.as_fun().is_some() { SymbolKind::Method } else { SymbolKind::Field },
                    ty: if field.optional { field.ty.optional() } else { field.ty },
                    doc: (!field.description.is_empty()).then(|| Arc::from(field.description.as_str())),
                    deprecated: false,
                    literal: None,
                    range: self.range(group[field.line.min(group.len() - 1)].span),
                    name: field.name,
                })
                .collect();
            self.out.classes.push(ClassDef {
                name: class.name,
                parents: class.parents,
                fields,
                index: class.index,
                call: class.call,
                doc: (!class.description.is_empty()).then(|| Arc::from(class.description.as_str())),
                range,
            });
        }
        for alias in doc.aliases {
            let range = self.range(group[alias.line.min(group.len() - 1)].span);
            self.out.aliases.push(AliasDef {
                name: alias.name,
                ty: alias.ty,
                doc: (!alias.description.is_empty()).then(|| Arc::from(alias.description.as_str())),
                range,
            });
        }
    }

    fn owner_of(&self, ty: &Type) -> Option<SmolStr> {
        match ty {
            Type::Named(name, _) => Some(name.clone()),
            Type::GlobalTable(owner) => Some(owner.clone()),
            Type::Union(types) => {
                let owners: Vec<SmolStr> = types.iter().filter_map(|t| self.owner_of(t)).collect();
                owners.iter().find(|o| !o.starts_with('%')).or(owners.first()).cloned()
            }
            _ => None,
        }
    }

    /// The class a global was annotated with, preferring this file so the answer does not depend
    /// on whether the file is already part of the index.
    fn global_class(&self, name: &str) -> Option<SmolStr> {
        let own = self.out.globals.iter().rev().find(|s| s.name == name).map(|s| s.ty.clone());
        match own.unwrap_or_else(|| self.infer.global_type(name)) {
            Type::Named(class, _) => Some(class),
            _ => None,
        }
    }

    fn push_member(&mut self, owner: SmolStr, symbol: Symbol) {
        if self.out.members.len() < MAX_MEMBERS_PER_FILE {
            self.out.members.push(Member { owner, symbol });
        }
    }

    /// The symbol for `name = value`, registering nested table fields under `nested_owner`.
    fn value_symbol(
        &mut self,
        name: &Name,
        value: Option<&Expr>,
        doc_anchor: u32,
        nested_owner: &str,
        table_depth: u32,
    ) -> Symbol {
        let doc = self.ctx.doc_at(doc_anchor);
        let mut kind = SymbolKind::Variable;
        let ty = if let Some(class) = doc.classes.last() {
            if let Some(fields) = value.and_then(table_fields) {
                self.table_members(class.name.clone(), fields, table_depth + 1);
            }
            kind = SymbolKind::Table;
            Type::Named(class.name.clone(), Vec::new())
        } else if let Some(ty) = &doc.ty {
            ty.clone()
        } else {
            match value.map(|v| (&v.kind, v)) {
                Some((_, expr)) if table_fields(expr).is_some_and(<[TableField]>::is_empty) => {
                    kind = SymbolKind::Table;
                    Type::Table
                }
                Some((_, expr)) if table_fields(expr).is_some() && table_depth < MAX_TABLE_DEPTH => {
                    let fields = table_fields(expr).unwrap_or_default();
                    kind = SymbolKind::Table;
                    if let Some(enum_name) = &doc.enum_name {
                        self.enum_class(enum_name.clone(), doc.enum_keys, fields, name.span);
                    }
                    self.table_members(SmolStr::new(nested_owner), fields, table_depth + 1);
                    Type::GlobalTable(SmolStr::new(nested_owner))
                }
                Some((ExprKind::Function(func), _)) => {
                    kind = SymbolKind::Function;
                    Type::Fun(Arc::new(self.infer.fun_type(func, Some(doc_anchor), false)))
                }
                Some((_, expr)) => self.infer.expr(expr).widen(),
                None => Type::Unknown,
            }
        };
        if matches!(ty, Type::GlobalTable(_)) {
            kind = SymbolKind::Table;
        }
        Symbol {
            name: name.text.clone(),
            kind,
            ty,
            doc: render_doc(&doc),
            deprecated: doc.deprecated.is_some(),
            literal: value.and_then(|v| self.literal_text(v)),
            range: self.range(name.span),
        }
    }

    /// Short literals are kept so hovers can show `Debug: boolean = true` without the source file.
    fn literal_text(&self, value: &Expr) -> Option<SmolStr> {
        let is_literal = match &value.kind {
            ExprKind::True | ExprKind::False | ExprKind::Number(_) | ExprKind::String(_) | ExprKind::JenkinsHash(_) => {
                true
            }
            ExprKind::Unary { op: UnOp::Neg, expr } => matches!(expr.kind, ExprKind::Number(_)),
            _ => false,
        };
        let text = value.span.text(self.source);
        (is_literal && text.len() <= 48 && !text.contains('\n')).then(|| SmolStr::new(text))
    }

    fn enum_class(&mut self, name: SmolStr, keys: bool, fields: &[TableField], span: Span) {
        let values: Vec<Type> = fields
            .iter()
            .filter_map(|f| match f {
                TableField::Named { name: key, .. } if keys => Some(Type::StringLit(key.text.clone())),
                TableField::Keyed { key, .. } if keys => Some(self.infer.expr(key)),
                TableField::Named { value, .. } | TableField::Keyed { value, .. } => Some(self.infer.expr(value)),
                _ => None,
            })
            .collect();
        let range = self.range(span);
        self.out.aliases.push(AliasDef { name, ty: Type::union(values), doc: None, range });
    }

    fn push_element(&mut self, owner: SmolStr, key: Type, value: Type) {
        if self.out.elements.len() < MAX_MEMBERS_PER_FILE {
            self.out.elements.push(Element { owner, key, value });
        }
    }

    fn table_members(&mut self, owner: SmolStr, fields: &[TableField], depth: u32) {
        let (mut array, mut keys, mut values) = (Vec::new(), Vec::new(), Vec::new());
        for field in fields.iter().take(MAX_TABLE_FIELDS) {
            let (name, value) = match field {
                TableField::Named { name, value } => (name.clone(), value),
                TableField::Keyed { key: Expr { kind: ExprKind::String(key), span }, value } => {
                    (Name { text: key.clone(), span: *span }, value)
                }
                TableField::Positional(value) => {
                    array.push(self.infer.expr(value).widen());
                    continue;
                }
                TableField::Keyed { key, value } => {
                    keys.push(self.infer.expr(key).widen());
                    values.push(self.infer.expr(value).widen());
                    continue;
                }
                TableField::SetMember(_) => continue,
            };
            let nested = format!("{owner}.{}", name.text);
            let mut symbol = self.value_symbol(&name, Some(value), name.span.start, &nested, depth);
            if symbol.kind == SymbolKind::Variable {
                symbol.kind = SymbolKind::Field;
            }
            self.push_member(owner.clone(), symbol);
        }
        // Kept apart from the `[key]` pairs, since `ipairs` visits only the array part.
        if !array.is_empty() {
            self.push_element(owner.clone(), Type::Integer, Type::union(array));
        }
        if !keys.is_empty() {
            self.push_element(owner, Type::union(keys), Type::union(values));
        }
    }

    fn block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
    }

    fn func_body(&mut self, func: &FuncBody) {
        self.depth += 1;
        self.block(&func.body);
        self.depth -= 1;
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Function { name, func } => {
                self.function_decl(stmt, name, func);
                self.func_body(func);
            }
            StmtKind::LocalFunction { func, .. } => self.func_body(func),
            StmtKind::Local { names, exprs, .. } => {
                if self.depth == 0 {
                    for (i, name) in names.iter().enumerate() {
                        if let Some(fields) = exprs.get(i).and_then(table_fields) {
                            let doc = self.ctx.doc_at(stmt.span.start);
                            let owner = match doc.classes.last() {
                                Some(class) => class.name.clone(),
                                None => self.ctx.local_owner_key(name.name.span.start),
                            };
                            if let Some(enum_name) = &doc.enum_name {
                                self.enum_class(enum_name.clone(), doc.enum_keys, fields, name.name.span);
                            }
                            self.table_members(owner, fields, 1);
                        }
                    }
                }
                exprs.iter().for_each(|e| self.expr(e));
            }
            StmtKind::Assign { targets, exprs } => {
                for (i, target) in targets.iter().enumerate() {
                    self.assignment(stmt, target, exprs.get(i));
                    self.expr(target);
                }
                exprs.iter().for_each(|e| self.expr(e));
            }
            StmtKind::CompoundAssign { expr, .. } => self.expr(expr),
            StmtKind::Expr(expr) => {
                self.call_stmt(stmt, expr);
                self.expr(expr);
            }
            StmtKind::Do(body) | StmtKind::Defer(body) => self.block(body),
            StmtKind::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            StmtKind::Repeat { body, cond } => {
                self.block(body);
                self.expr(cond);
            }
            StmtKind::If { branches, else_block } => {
                for branch in branches {
                    self.expr(&branch.cond);
                    self.block(&branch.block);
                }
                if let Some(block) = else_block {
                    self.block(block);
                }
            }
            StmtKind::NumericFor { body, .. } => self.block(body),
            StmtKind::GenericFor { exprs, body, .. } => {
                exprs.iter().for_each(|e| self.expr(e));
                self.block(body);
            }
            StmtKind::Return(exprs) => exprs.iter().for_each(|e| self.expr(e)),
            StmtKind::Break | StmtKind::Goto(_) | StmtKind::Label(_) | StmtKind::Error => {}
        }
    }

    fn function_decl(&mut self, stmt: &Stmt, name: &FuncName, func: &FuncBody) {
        let doc = self.ctx.doc_at(stmt.span.start);
        let is_method = name.method.is_some();
        let fun = Type::Fun(Arc::new(self.infer.fun_type(func, Some(stmt.span.start), is_method)));
        let last = name.method.as_ref().or(name.path.last()).unwrap_or(&name.base);
        let symbol = Symbol {
            name: last.text.clone(),
            kind: if is_method { SymbolKind::Method } else { SymbolKind::Function },
            ty: fun,
            doc: render_doc(&doc),
            deprecated: doc.deprecated.is_some(),
            literal: None,
            range: self.range(last.span),
        };
        if name.path.is_empty() && name.method.is_none() {
            if self.is_global(&name.base) {
                self.out.globals.push(symbol);
            }
            return;
        }
        let owner_path = if is_method { &name.path[..] } else { &name.path[..name.path.len() - 1] };
        let mut owner = if self.is_global(&name.base) {
            self.global_class(&name.base.text).unwrap_or_else(|| name.base.text.clone())
        } else {
            let base = FuncName { base: name.base.clone(), path: Vec::new(), method: None, span: name.base.span };
            match self.owner_of(&self.infer.func_name_owner_type(&base)) {
                Some(owner) => owner,
                None => return,
            }
        };
        for segment in owner_path {
            owner = SmolStr::new(format!("{owner}.{}", segment.text));
        }
        self.push_member(owner, symbol);
    }

    fn assignment(&mut self, stmt: &Stmt, target: &Expr, value: Option<&Expr>) {
        match &target.kind {
            ExprKind::Name(name) if self.is_global(name) => {
                let symbol = self.value_symbol(name, value, stmt.span.start, &name.text.clone(), 0);
                self.out.globals.push(symbol);
            }
            ExprKind::Field { base, name, .. } if !name.is_missing() => {
                self.member_assignment(stmt, base, name, value);
            }
            ExprKind::Index { base, index, .. } => {
                if let Some(text) = index.as_string() {
                    let name = Name { text: text.clone(), span: index.span };
                    self.member_assignment(stmt, base, &name, value);
                }
            }
            _ => {}
        }
    }

    fn member_assignment(&mut self, stmt: &Stmt, base: &Expr, name: &Name, value: Option<&Expr>) {
        if let ExprKind::Name(root) = &base.kind {
            if self.is_global(root) && matches!(root.text.as_str(), "_ENV" | "_G") {
                let symbol = self.value_symbol(name, value, stmt.span.start, &name.text.clone(), 0);
                self.out.globals.push(symbol);
                return;
            }
        }
        let owner = match base.dotted_path() {
            Some(path) if self.root_is_global(base) => {
                let own_class = match &base.kind {
                    ExprKind::Name(root) => self.global_class(&root.text),
                    _ => None,
                };
                match own_class.map(|class| Type::Named(class, Vec::new())).unwrap_or_else(|| self.infer.expr(base)) {
                    Type::Named(class, _) => class,
                    _ => SmolStr::new(path),
                }
            }
            _ => match self.owner_of(&self.infer.expr(base)) {
                Some(owner) => owner,
                None => return,
            },
        };
        let nested = format!("{owner}.{}", name.text);
        let mut symbol = self.value_symbol(name, value, stmt.span.start, &nested, 1);
        if symbol.kind == SymbolKind::Variable {
            symbol.kind = SymbolKind::Field;
        }
        self.push_member(owner, symbol);
    }

    fn root_is_global(&self, expr: &Expr) -> bool {
        let mut current = expr;
        loop {
            match &current.kind {
                ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => current = base,
                ExprKind::Name(name) => return self.is_global(name),
                _ => return false,
            }
        }
    }

    fn module_return(&mut self, exprs: &[Expr]) {
        let Some(first) = exprs.first() else { return };
        self.out.module_return = Some(match table_fields(first) {
            Some(fields) => {
                let owner = SmolStr::new(format!("%mod{}", self.file));
                self.table_members(owner.clone(), fields, 1);
                Type::GlobalTable(owner)
            }
            None => self.infer.expr(first).widen(),
        });
    }

    fn call_stmt(&mut self, stmt: &Stmt, expr: &Expr) {
        let ExprKind::Call { callee, args, .. } = &expr.kind else { return };
        if callee.dotted_path().as_deref() != Some("exports") {
            return;
        }
        let (Some(name_arg), Some(value)) = (args.first(), args.get(1)) else { return };
        let Some(name) = name_arg.as_string() else {
            self.out.dynamic_exports = true;
            return;
        };
        let doc = self.ctx.doc_at(stmt.span.start);
        let ty = match &value.kind {
            ExprKind::Function(func) => Type::Fun(Arc::new(self.infer.fun_type(func, Some(stmt.span.start), false))),
            ExprKind::Name(global) if self.is_global(global) => {
                let own = self.out.globals.iter().rev().find(|s| s.name == global.text).map(|s| s.ty.clone());
                own.unwrap_or_else(|| self.infer.expr(value))
            }
            _ => self.infer.expr(value),
        };
        let mut symbol_doc = render_doc(&doc);
        if symbol_doc.is_none() {
            symbol_doc = self.referenced_doc(value);
        }
        self.out.exports.push(Symbol {
            name: name.clone(),
            kind: SymbolKind::Export,
            ty,
            doc: symbol_doc,
            deprecated: doc.deprecated.is_some(),
            literal: None,
            range: self.range(name_arg.span),
        });
    }

    /// Docs of the function an export refers to by name, e.g. `exports('GetPlayer', GetPlayer)`.
    fn referenced_doc(&self, value: &Expr) -> Option<Arc<str>> {
        let ExprKind::Name(name) = &value.kind else { return None };
        match self.ctx.resolution.resolve_at(name.span.start)? {
            Resolved::Local(id) => {
                let decl = self.ctx.resolution.local(id).decl.start;
                let anchor = match self.ctx.decl(decl)? {
                    crate::infer::Decl::LocalFunction { stmt, .. } | crate::infer::Decl::Local { stmt, .. } => {
                        stmt.span.start
                    }
                    _ => return None,
                };
                render_doc(&self.ctx.doc_at(anchor))
            }
            Resolved::Global(_) => self.out.globals.iter().find(|s| s.name == name.text).and_then(|s| s.doc.clone()),
        }
    }

    fn state_key(&mut self, key: Option<&SmolStr>) {
        let Some(key) = key.filter(|k| !k.is_empty() && k.as_str() != "set") else { return };
        if !self.out.state_keys.contains(key) {
            self.out.state_keys.push(key.clone());
        }
    }

    /// ox_lib fills its cache through `cache:set('ped', ped)`, so the field names only exist as strings.
    fn keyed_setter(&mut self, base: &Expr, method: &Name, args: &[Expr]) {
        let is_cache = matches!(&base.kind, ExprKind::Name(name) if name.text == "cache");
        let (true, "set", [key, value, ..]) = (is_cache, method.text.as_str(), args) else { return };
        let (Some(field), Some(owner)) = (key.as_string(), self.owner_of(&self.infer.expr(base))) else { return };
        let symbol = Symbol {
            name: field.clone(),
            kind: SymbolKind::Field,
            ty: self.infer.expr(value).widen(),
            doc: None,
            deprecated: false,
            literal: None,
            range: self.range(key.span),
        };
        self.push_member(owner, symbol);
    }

    fn event(&mut self, path: &str, args: &[Expr], offset: u32) {
        let kind = if NET_EVENT_CALLS.contains(&path) {
            EventKind::NetEvent
        } else if HANDLER_CALLS.contains(&path) {
            EventKind::Handler
        } else if CALLBACK_CALLS.contains(&path) {
            EventKind::Callback
        } else if TRIGGER_CALLS.contains(&path) {
            EventKind::Trigger
        } else {
            return;
        };
        let Some(name_arg) = args.first() else { return };
        let Some(name) = name_arg.as_string().filter(|n| !n.is_empty()) else { return };
        let handler = args.iter().skip(1).find_map(|arg| match &arg.kind {
            ExprKind::Function(func) => Some(Arc::new(self.infer.fun_type(func, None, false))),
            _ => None,
        });
        self.out.events.push(EventDef {
            name: name.clone(),
            kind,
            side: self.regions.effective(offset, self.side),
            handler,
            range: self.range(name_arg.span),
        });
    }

    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Function(func) => self.func_body(func),
            ExprKind::Call { callee, args, .. } => {
                if let Some(path) = callee.dotted_path() {
                    self.event(&path, args, expr.span.start);
                    let first = args.first().and_then(|a| a.as_string());
                    if CONVAR_CALLS.contains(&path.as_str()) {
                        if let Some(name) = first.filter(|n| !n.is_empty() && !self.out.convars.contains(n)) {
                            self.out.convars.push(name.clone());
                        }
                    } else if path == "AddStateBagChangeHandler" {
                        self.state_key(first);
                    }
                }
                self.expr(callee);
                args.iter().for_each(|a| self.expr(a));
            }
            ExprKind::MethodCall { base, method, args, .. } => {
                self.keyed_setter(base, method, args);
                if let (true, "set", Some(key)) = (is_state_bag(base), method.text.as_str(), args.first()) {
                    self.state_key(key.as_string());
                }
                self.expr(base);
                args.iter().for_each(|a| self.expr(a));
            }
            ExprKind::Index { base, index, .. } => {
                if is_state_bag(base) {
                    self.state_key(index.as_string());
                }
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Field { base, name, .. } => {
                if is_state_bag(base) {
                    self.state_key(Some(&name.text));
                }
                self.expr(base)
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Unary { expr, .. } | ExprKind::Paren(expr) => self.expr(expr),
            ExprKind::Table(fields) => {
                for field in fields {
                    match field {
                        TableField::Positional(value) | TableField::Named { value, .. } => self.expr(value),
                        TableField::Keyed { key, value } => {
                            self.expr(key);
                            self.expr(value);
                        }
                        TableField::SetMember(_) => {}
                    }
                }
            }
            _ => {}
        }
    }
}
