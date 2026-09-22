use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionItemTag, CompletionList,
    CompletionResponse, Documentation, InsertTextFormat, Position,
};
use qbx_fivem_data::{native, native_docs, natives, Side};
use qbx_lua_analysis::manifest::KNOWN_DIRECTIVES;
use qbx_lua_analysis::project::relative_slash_path;
use qbx_lua_analysis::scope::LocalKind;
use qbx_lua_syntax::ast::ExprKind;
use qbx_lua_syntax::{CommentKind, TokenKind};
use rustc_hash::FxHashSet;
use serde_json::json;

use super::{lua_block, markdown, with_infer};
use crate::document::Document;
use crate::index::{EventKind, FileOrigin, SymbolKind};
use crate::infer::{Infer, MemberInfo};
use crate::locate::locate;
use crate::types::Type;
use crate::workspace::Workspace;

const MAX_NATIVES: usize = 120;
const MAX_ITEMS: usize = 600;

const KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil",
    "not", "or", "repeat", "return", "then", "true", "until", "while",
];

const THREAD_LOOP: &str = "CreateThread(function()\n\twhile true do\n\t\t$0\n\t\tWait(${1:0})\n\tend\nend)";

const SNIPPETS: &[(&str, &str, &str)] = &[
    ("CreateThread", THREAD_LOOP, "Thread with a loop that yields every iteration"),
    ("thread", THREAD_LOOP, "Thread with a loop that yields every iteration"),
    ("CreateThread once", "CreateThread(function()\n\t$0\nend)", "Thread that runs its body once"),
    ("SetTimeout", "SetTimeout(${1:1000}, function()\n\t$0\nend)", "Run a function after a delay"),
    (
        "RegisterNetEvent",
        "RegisterNetEvent('${1:resource}:${2:event}', function(${3})\n\t$0\nend)",
        "Register a network event with a handler",
    ),
    ("AddEventHandler", "AddEventHandler('${1:eventName}', function(${2})\n\t$0\nend)", "Handle a local event"),
    (
        "RegisterCommand",
        "RegisterCommand('${1:name}', function(source, args, raw)\n\t$0\nend, ${2:false})",
        "Register a console/chat command",
    ),
    (
        "lib.callback.register",
        "lib.callback.register('${1:resource}:${2:name}', function(source${3})\n\t$0\nend)",
        "Register an ox_lib server callback",
    ),
    ("lib.callback.await", "lib.callback.await('${1:resource}:${2:name}', ${3:false}$0)", "Await an ox_lib callback"),
    ("for pairs", "for ${1:k}, ${2:v} in pairs(${3:t}) do\n\t$0\nend", "Iterate over a table"),
    ("for ipairs", "for ${1:i}, ${2:v} in ipairs(${3:t}) do\n\t$0\nend", "Iterate over an array"),
    ("for i", "for ${1:i} = ${2:1}, ${3:#t} do\n\t$0\nend", "Numeric for loop"),
    ("function", "function ${1:name}(${2})\n\t$0\nend", "Function declaration"),
    ("local function", "local function ${1:name}(${2})\n\t$0\nend", "Local function declaration"),
    ("if", "if ${1:condition} then\n\t$0\nend", "If statement"),
    ("while", "while ${1:condition} do\n\t$0\nend", "While loop"),
];

const DOC_TAGS: &[(&str, &str)] = &[
    ("param", "param ${1:name} ${2:type}"),
    ("return", "return ${1:type}"),
    ("type", "type ${1:type}"),
    ("class", "class ${1:Name}"),
    ("field", "field ${1:name} ${2:type}"),
    ("alias", "alias ${1:Name} ${2:type}"),
    ("enum", "enum ${1:Name}"),
    ("generic", "generic ${1:T}"),
    ("overload", "overload fun(${1}): ${2:any}"),
    ("deprecated", "deprecated"),
    ("async", "async"),
    ("nodiscard", "nodiscard"),
    ("meta", "meta"),
    ("diagnostic", "diagnostic disable-next-line: ${1:undefined-global}"),
    ("see", "see ${1:symbol}"),
];

const PRIMITIVE_TYPES: &[&str] = &[
    "any",
    "nil",
    "boolean",
    "number",
    "integer",
    "string",
    "table",
    "function",
    "thread",
    "userdata",
    "unknown",
    "fun()",
    "table<string, any>",
];

const EVENT_NAME_CALLS: &[&str] = &[
    "TriggerEvent",
    "TriggerServerEvent",
    "TriggerClientEvent",
    "TriggerLatentServerEvent",
    "TriggerLatentClientEvent",
    "AddEventHandler",
    "RegisterNetEvent",
    "RegisterServerEvent",
];
const CALLBACK_NAME_CALLS: &[&str] = &["lib.callback", "lib.callback.await", "lib.callback.register"];
const REQUIRE_CALLS: &[&str] = &["require", "lib.require", "lib.load"];
const RESOURCE_NAME_CALLS: &[&str] = &[
    "GetResourceState",
    "StartResource",
    "StopResource",
    "GetResourcePath",
    "GetResourceMetadata",
    "LoadResourceFile",
];

fn kind_of(kind: SymbolKind, ty: &Type) -> CompletionItemKind {
    match kind {
        SymbolKind::Function | SymbolKind::Export => CompletionItemKind::FUNCTION,
        SymbolKind::Method => CompletionItemKind::METHOD,
        SymbolKind::Class => CompletionItemKind::CLASS,
        SymbolKind::Alias => CompletionItemKind::INTERFACE,
        SymbolKind::Table => CompletionItemKind::MODULE,
        _ if ty.as_fun().is_some() => CompletionItemKind::FUNCTION,
        SymbolKind::Field => CompletionItemKind::FIELD,
        SymbolKind::Variable => CompletionItemKind::VARIABLE,
    }
}

fn detail_of(name: &str, ty: &Type) -> Option<String> {
    match ty {
        Type::Unknown => None,
        Type::Fun(fun) => Some(fun.signature(name)),
        Type::GlobalTable(path) if path.starts_with('%') => Some("table".into()),
        other => Some(other.to_string()),
    }
}

fn item(label: &str, kind: CompletionItemKind, sort_group: u8) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        sort_text: Some(format!("{sort_group}{label}")),
        ..CompletionItem::default()
    }
}

/// Snippets sort ahead of the plain name they share a label with, otherwise accepting the first
/// `CreateThread` would only insert the word.
fn snippet_item(label: &str, body: &str, description: &str) -> CompletionItem {
    let is_statement = body.starts_with(|c: char| c.is_ascii_lowercase()) && !body.starts_with("lib.");
    let kind = if is_statement { CompletionItemKind::KEYWORD } else { CompletionItemKind::EVENT };
    let mut out = item(label, kind, 0);
    out.sort_text = Some(format!("/{label}"));
    out.filter_text = Some(label.to_string());
    out.insert_text = Some(body.to_string());
    out.insert_text_format = Some(InsertTextFormat::SNIPPET);
    out.detail = Some(description.to_string());
    out.label_details = Some(CompletionItemLabelDetails { detail: None, description: Some("snippet".to_string()) });
    out.documentation = Some(Documentation::MarkupContent(markdown(lua_block(&snippet_preview(body)))));
    out
}

/// The snippet as it looks right after insertion: `${1:0}` becomes `0`, `${1|a,b|}` becomes `a`.
pub fn snippet_preview(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        if let Some(inner) = rest.strip_prefix('{') {
            let end = inner.find('}').unwrap_or(inner.len());
            let placeholder = &inner[..end];
            let shown = match placeholder.split_once([':', '|']) {
                Some((_, default)) => default.split([',', '|']).next().unwrap_or_default(),
                None => "",
            };
            out.push_str(shown);
            rest = inner.get(end + 1..).unwrap_or_default();
        } else {
            rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        }
    }
    out.push_str(rest);
    out.replace('\t', "    ")
}

pub struct SnippetInfo {
    pub label: String,
    pub description: String,
    pub body: String,
}

/// Every snippet the server offers, for the editor's "show snippets" picker.
pub fn all_snippets(ws: &Workspace, doc: Option<&Document>) -> Vec<SnippetInfo> {
    let mut out: Vec<SnippetInfo> = SNIPPETS
        .iter()
        .map(|(label, body, description)| SnippetInfo {
            label: label.to_string(),
            description: description.to_string(),
            body: body.to_string(),
        })
        .collect();
    let on_cache = match doc {
        Some(doc) => with_infer(ws, doc, |infer| on_cache_snippet(infer, "lib.")),
        None => on_cache_item(Vec::new(), "lib."),
    };
    out.push(SnippetInfo {
        label: on_cache.label,
        description: on_cache.detail.unwrap_or_default(),
        body: on_cache.insert_text.unwrap_or_default(),
    });
    out
}

fn member_item(member: &MemberInfo) -> CompletionItem {
    let mut out = item(&member.name, kind_of(member.kind, &member.ty), 0);
    out.detail = match (detail_of(&member.name, &member.ty), &member.literal) {
        (Some(ty), Some(value)) => Some(format!("{ty} = {value}")),
        (detail, _) => detail,
    };
    out.documentation = member.doc.as_ref().map(|d| Documentation::MarkupContent(markdown(d.to_string())));
    if member.deprecated {
        out.tags = Some(vec![CompletionItemTag::DEPRECATED]);
    }
    if !is_identifier(&member.name) {
        out.filter_text = Some(member.name.to_string());
    }
    out
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn identifier_prefix(before: &str) -> &str {
    let start = before.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(0, |i| i + 1);
    &before[start..]
}

pub fn completion(ws: &Workspace, doc: &Document, position: Position, snippets: bool) -> Option<CompletionResponse> {
    let offset = doc.offset(position);
    let line_start = doc.lines.line_start(position.line) as usize;
    let before = doc.text.get(line_start..offset as usize)?;

    if let Some(comment) = doc.chunk.comments.iter().find(|c| c.span.start < offset && offset <= c.span.end) {
        let is_doc = comment.kind == CommentKind::Line && comment.span.text(&doc.text).starts_with("---");
        return is_doc.then(|| respond(doc_comment_items(ws, before, snippets), false));
    }

    let in_string = doc.chunk.tokens.iter().position(|t| {
        let text = t.span.text(&doc.text);
        let unterminated = text.len() < 2 || text.as_bytes()[0] != text.as_bytes()[text.len() - 1];
        matches!(t.kind, TokenKind::String)
            && t.span.start < offset
            && (offset < t.span.end || (unterminated && offset == t.span.end))
    });
    if let Some(token_index) = in_string {
        return Some(respond(string_items(ws, doc, offset, token_index), false));
    }

    let prefix = identifier_prefix(before);
    let head = before[..before.len() - prefix.len()].trim_end();
    if prefix.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }

    if (head.ends_with('.') && !head.ends_with("..")) || (head.ends_with(':') && !head.ends_with("::")) {
        let via_colon = head.ends_with(':');
        let mut items = with_infer(ws, doc, |infer| member_items(infer, doc, offset, head, via_colon, snippets));
        let base = head[..head.len() - 1].trim_end();
        if !via_colon && (base.ends_with(".state") || base.ends_with("GlobalState")) {
            items.extend(state_key_items(ws));
        }
        return Some(respond(items, false));
    }

    if doc.is_manifest() {
        return Some(respond(manifest_items(before, prefix, snippets), false));
    }

    if head.ends_with('{') || head.ends_with(',') || head.is_empty() {
        let fields = with_infer(ws, doc, |infer| expected_field_items(infer, doc, offset));
        if !fields.is_empty() {
            return Some(respond(fields, false));
        }
    }
    if prefix.is_empty() {
        return None;
    }
    let (items, incomplete) = with_infer(ws, doc, |infer| scope_items(ws, infer, doc, offset, prefix, snippets));
    Some(respond(items, incomplete))
}

fn respond(mut items: Vec<CompletionItem>, incomplete: bool) -> CompletionResponse {
    let truncated = items.len() > MAX_ITEMS;
    items.truncate(MAX_ITEMS);
    CompletionResponse::List(CompletionList { is_incomplete: incomplete || truncated, items })
}

fn doc_comment_items(ws: &Workspace, before: &str, snippets: bool) -> Vec<CompletionItem> {
    let Some(at) = before.rfind("---") else { return Vec::new() };
    let content = before[at + 3..].trim_start();
    if let Some(tag_prefix) = content.strip_prefix('@').filter(|rest| !rest.contains(char::is_whitespace)) {
        return DOC_TAGS
            .iter()
            .filter(|(tag, _)| tag.starts_with(tag_prefix))
            .map(|(tag, snippet)| {
                let mut out = item(tag, CompletionItemKind::KEYWORD, 0);
                out.insert_text =
                    Some(if snippets { snippet.to_string() } else { tag.trim_start_matches('@').to_string() });
                if snippets {
                    out.insert_text_format = Some(InsertTextFormat::SNIPPET);
                }
                out
            })
            .collect();
    }
    let takes_type = ["@type", "@return", "@param", "@field", "@alias", "@class", "@overload", "@generic", "|"]
        .iter()
        .any(|tag| content.starts_with(tag));
    if !takes_type {
        return Vec::new();
    }
    let mut items: Vec<CompletionItem> =
        PRIMITIVE_TYPES.iter().map(|t| item(t, CompletionItemKind::KEYWORD, 1)).collect();
    let mut seen = FxHashSet::default();
    for name in ws.index.class_names().filter(|n| seen.insert((*n).clone())) {
        items.push(item(name, CompletionItemKind::CLASS, 0));
    }
    items
}

fn string_items(ws: &Workspace, doc: &Document, offset: u32, token_index: usize) -> Vec<CompletionItem> {
    let tokens = &doc.chunk.tokens;
    let indexes_exports = token_index >= 2
        && tokens[token_index - 1].kind == TokenKind::LBracket
        && tokens[token_index - 2].span.text(&doc.text) == "exports";
    if indexes_exports {
        return resource_items(ws);
    }
    if doc.is_manifest() {
        return manifest_path_items(ws, doc);
    }

    let located = locate(&doc.chunk, offset);
    let Some((_, Some((call, arg_index)))) = located.string else { return Vec::new() };
    let path = match &call.kind {
        ExprKind::Call { callee, .. } => callee.dotted_path(),
        _ => None,
    };
    let Some(path) = path else { return Vec::new() };
    let path = path.as_str();

    if arg_index == 0 && (EVENT_NAME_CALLS.contains(&path) || CALLBACK_NAME_CALLS.contains(&path)) {
        let wants_callbacks = CALLBACK_NAME_CALLS.contains(&path);
        let own_side = ws.index.file(doc.file).and_then(|f| f.side);
        let own_side =
            qbx_lua_analysis::side_guard::SideRegions::of(&doc.text, &doc.chunk).effective(call.span.start, own_side);
        // Where the handler has to live for this call to reach it.
        let target_side = match path {
            "TriggerServerEvent" | "TriggerLatentServerEvent" => Some(Side::Server),
            "TriggerClientEvent" | "TriggerLatentClientEvent" => Some(Side::Client),
            "TriggerEvent" => own_side,
            "lib.callback" | "lib.callback.await" => match own_side {
                Some(Side::Client) => Some(Side::Server),
                Some(Side::Server) => Some(Side::Client),
                _ => None,
            },
            _ => None,
        };
        let handled_on_target = |side: Option<Side>| !matches!((target_side, side), (Some(target), Some(side)) if !side.is_available_on(target));
        let candidates = |strict: bool| {
            let mut seen = FxHashSet::default();
            ws.index
                .events()
                .filter(|(_, e)| (e.kind == EventKind::Callback) == wants_callbacks || e.kind == EventKind::Trigger)
                .filter(|(_, e)| !strict || (e.kind != EventKind::Trigger && handled_on_target(e.side)))
                .filter(|(_, e)| seen.insert(e.name.clone()))
                .map(|(file, event)| {
                    let mut out = item(&event.name, CompletionItemKind::EVENT, 0);
                    let entry = ws.index.file(file);
                    let origin = entry.and_then(|f| f.resource).and_then(|r| ws.index.resource(r));
                    let side = event.side.map_or(String::new(), |s| format!(" ({})", s.label()));
                    out.detail = match (&event.handler, origin) {
                        (Some(handler), Some(resource)) => {
                            Some(format!("{}{side} · {}", resource.name, handler.signature("")))
                        }
                        (None, Some(resource)) => Some(format!("{}{side}", resource.name)),
                        (Some(handler), None) => Some(handler.signature("")),
                        (None, None) => None,
                    };
                    out
                })
                .collect::<Vec<_>>()
        };
        return candidates(target_side.is_some());
    }
    if arg_index == 0 && matches!(path, "lib.onCache") {
        return cache_key_items(ws, doc);
    }
    if arg_index == 0 && path == "locale" {
        let locale = ws.index.resource_of(doc.file).and_then(|r| qbx_lua_analysis::locale::LocaleFile::load(&r.root));
        return locale
            .iter()
            .flat_map(|file| &file.keys)
            .map(|(key, _, text)| {
                let mut out = item(key, CompletionItemKind::TEXT, 0);
                out.detail = Some(text.clone());
                out
            })
            .collect();
    }
    if arg_index == 0 && crate::indexer::CONVAR_CALLS.contains(&path) {
        let mut seen = FxHashSet::default();
        let indexed = ws.index.files().flat_map(|(_, f)| f.index.convars.iter());
        return ws
            .cfg_convars
            .iter()
            .chain(indexed)
            .filter(|name| seen.insert((*name).clone()))
            .map(|name| item(name, CompletionItemKind::CONSTANT, 0))
            .collect();
    }
    if arg_index == 0 && path == "AddStateBagChangeHandler" {
        return state_key_items(ws);
    }
    if arg_index == 0 && REQUIRE_CALLS.contains(&path) {
        return module_items(ws, doc);
    }
    if arg_index == 0 && RESOURCE_NAME_CALLS.contains(&path) {
        return resource_items(ws);
    }
    Vec::new()
}

/// State bag keys are plain strings that both sides must agree on, so every key seen anywhere is offered.
fn state_key_items(ws: &Workspace) -> Vec<CompletionItem> {
    let mut seen = FxHashSet::default();
    ws.index
        .files()
        .flat_map(|(_, f)| f.index.state_keys.iter())
        .filter(|key| seen.insert((*key).clone()))
        .map(|key| {
            let mut out = item(key, CompletionItemKind::FIELD, 1);
            out.detail = Some("state bag key".into());
            out
        })
        .collect()
}

/// The value fields of ox_lib's `cache` as seen from this file, read from the indexed ox_lib source.
fn cache_keys(infer: &Infer) -> Vec<MemberInfo> {
    let mut keys: Vec<MemberInfo> =
        infer.members(&infer.global_type("cache")).into_iter().filter(|m| m.ty.as_fun().is_none()).collect();
    keys.sort_by(|a, b| a.name.cmp(&b.name));
    keys
}

fn cache_key_items(ws: &Workspace, doc: &Document) -> Vec<CompletionItem> {
    with_infer(ws, doc, |infer| {
        cache_keys(infer)
            .iter()
            .map(|key| {
                let mut out = item(&key.name, CompletionItemKind::ENUM_MEMBER, 0);
                out.detail = detail_of(&key.name, &key.ty).map(|ty| format!("cache.{}: {ty}", key.name));
                out
            })
            .collect()
    })
}

/// What ox_lib caches on the client, for workspaces that do not contain ox_lib itself.
const DEFAULT_CACHE_KEYS: &[&str] = &["ped", "vehicle", "seat", "weapon", "playerId", "serverId", "coords"];

fn on_cache_snippet(infer: &Infer, prefix: &str) -> CompletionItem {
    on_cache_item(cache_keys(infer).iter().map(|k| k.name.to_string()).collect(), prefix)
}

fn on_cache_item(mut keys: Vec<String>, prefix: &str) -> CompletionItem {
    if keys.is_empty() {
        keys = DEFAULT_CACHE_KEYS.iter().map(|k| k.to_string()).collect();
    }
    let body =
        format!("{prefix}onCache('${{1|{}|}}', function(${{2:value}}, ${{3:oldValue}})\n\t$0\nend)", keys.join(","));
    snippet_item("onCache", &body, &format!("React to an ox_lib cache change ({})", keys.join(", ")))
}

fn resource_items(ws: &Workspace) -> Vec<CompletionItem> {
    ws.index.resources.iter().map(|r| item(&r.name, CompletionItemKind::MODULE, 0)).collect()
}

fn module_items(ws: &Workspace, doc: &Document) -> Vec<CompletionItem> {
    let Some(resource) = ws.index.resource_of(doc.file) else { return Vec::new() };
    resource
        .files
        .iter()
        .filter_map(|id| ws.index.file(*id))
        .filter(|f| f.path != doc.path)
        .map(|f| {
            let relative = relative_slash_path(&resource.root, &f.path);
            let module = relative.trim_end_matches(".lua").replace('/', ".");
            let mut out = item(&module, CompletionItemKind::FILE, 0);
            out.detail = Some(relative);
            out
        })
        .collect()
}

fn manifest_path_items(ws: &Workspace, doc: &Document) -> Vec<CompletionItem> {
    let Some(root) = doc.path.parent() else { return Vec::new() };
    let mut items: Vec<CompletionItem> = qbx_lua_analysis::lint::all_files(root)
        .into_iter()
        .filter(|f| !f.ends_with("fxmanifest.lua") && !f.starts_with('.'))
        .take(400)
        .map(|f| item(&f, CompletionItemKind::FILE, 1))
        .collect();
    for import in qbx_fivem_data::KNOWN_IMPORTS {
        items.push(item(import.path, CompletionItemKind::REFERENCE, 0));
    }
    let _ = ws;
    items
}

fn manifest_items(before: &str, prefix: &str, snippets: bool) -> Vec<CompletionItem> {
    if before.trim_start().len() != prefix.len() {
        return Vec::new();
    }
    KNOWN_DIRECTIVES
        .iter()
        .map(|directive| {
            let mut out = item(directive, CompletionItemKind::PROPERTY, 0);
            if !snippets {
                return out;
            }
            let snippet = match *directive {
                "fx_version" => "fx_version '${1|cerulean,bodacious,adamant|}'".to_string(),
                "game" => "game '${1|gta5,rdr3|}'".to_string(),
                "lua54" | "use_experimental_fxv2_oal" => format!("{directive} 'yes'"),
                d if d.ends_with('s') && !matches!(d, "this_is_a_map") => format!("{d} {{\n\t'$0',\n}}"),
                d => format!("{d} '$0'"),
            };
            out.insert_text = Some(snippet);
            out.insert_text_format = Some(InsertTextFormat::SNIPPET);
            out
        })
        .collect()
}

fn member_items(
    infer: &Infer,
    doc: &Document,
    offset: u32,
    head: &str,
    via_colon: bool,
    snippets: bool,
) -> Vec<CompletionItem> {
    let located = locate(&doc.chunk, offset);
    let base_type = match &located.member {
        Some(access) => infer.expr(access.base()),
        None => type_of_path(infer, &head[..head.len() - 1], offset),
    };
    let members = infer.members(&base_type);
    let has_methods = members.iter().any(|m| m.ty.as_fun().is_some());
    members
        .iter()
        .filter(|m| !via_colon || !has_methods || m.ty.as_fun().is_some())
        .filter(|m| is_identifier(&m.name))
        .map(|m| {
            let mut out = member_item(m);
            let is_method = m.ty.as_fun().is_some_and(|f| f.is_method);
            if via_colon != is_method && m.ty.as_fun().is_some() {
                out.sort_text = Some(format!("1{}", m.name));
            }
            out
        })
        .chain((snippets && head == "lib.").then(|| on_cache_snippet(infer, "")))
        .collect()
}

/// Fallback for member completion when the parser could not attach the trailing `.` to an expression.
fn type_of_path(infer: &Infer, text: &str, offset: u32) -> Type {
    let start = text.rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':'))).map_or(0, |i| i + 1);
    let mut segments = text[start..].split(['.', ':']).filter(|s| !s.is_empty());
    let Some(root) = segments.next() else { return Type::Unknown };
    let mut ty = match infer.ctx.resolution.lookup_local_at(root, offset) {
        Some(id) => infer.local_type(id),
        None => infer.global_type(root),
    };
    for segment in segments {
        ty = infer.member(&ty, segment).map(|m| m.ty).unwrap_or_default();
    }
    ty
}

/// Field names of the table type a call expects, when the cursor is inside a table argument.
fn expected_field_items(infer: &Infer, doc: &Document, offset: u32) -> Vec<CompletionItem> {
    let located = locate(&doc.chunk, offset);
    let Some((call, arg_index, table)) = located.table_in_call else { return Vec::new() };
    let ExprKind::Table(existing) = &table.kind else { return Vec::new() };
    let fun = match &call.kind {
        ExprKind::Call { callee, .. } => infer.callee_fun(callee, None),
        ExprKind::MethodCall { base, method, .. } => infer.callee_fun(base, Some(method)),
        _ => None,
    };
    let Some((fun, _)) = fun else { return Vec::new() };
    let (skip_params, skip_args) = fun.call_offsets(matches!(call.kind, ExprKind::MethodCall { .. }));
    let Some(param) = (arg_index + skip_params).checked_sub(skip_args).and_then(|i| fun.params.get(i)) else {
        return Vec::new();
    };
    let present: FxHashSet<&str> = existing
        .iter()
        .filter_map(|f| match f {
            qbx_lua_syntax::ast::TableField::Named { name, .. } => Some(name.text.as_str()),
            _ => None,
        })
        .collect();
    infer
        .members(&param.ty.without_nil())
        .iter()
        .filter(|m| !present.contains(m.name.as_str()) && is_identifier(&m.name))
        .map(|m| {
            let mut out = member_item(m);
            out.kind = Some(CompletionItemKind::PROPERTY);
            out.insert_text = Some(format!("{} = ", m.name));
            out
        })
        .collect()
}

fn scope_items(
    ws: &Workspace,
    infer: &Infer,
    doc: &Document,
    offset: u32,
    prefix: &str,
    snippets: bool,
) -> (Vec<CompletionItem>, bool) {
    let matches = |name: &str| name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix);
    let mut items = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();

    let mut locals: Vec<_> = doc.resolution.locals_visible_at(offset).filter(|(_, l)| matches(&l.name)).collect();
    locals.sort_by_key(|(_, l)| std::cmp::Reverse(l.visible_from));
    for (id, local) in locals {
        if local.name.is_empty() || !seen.insert(local.name.to_string()) {
            continue;
        }
        let ty = infer.local_type(id);
        let kind = match local.kind {
            _ if ty.as_fun().is_some() => CompletionItemKind::FUNCTION,
            LocalKind::Param => CompletionItemKind::VARIABLE,
            _ => CompletionItemKind::VARIABLE,
        };
        let mut out = item(&local.name, kind, 0);
        out.detail = detail_of(&local.name, &ty);
        items.push(out);
    }

    for (file, symbol) in ws.index.visible_globals(doc.file) {
        if !matches(&symbol.name) || !seen.insert(symbol.name.to_string()) {
            continue;
        }
        let is_stub = ws.index.file(file).is_some_and(|f| f.origin == FileOrigin::Stub);
        let mut out = item(&symbol.name, kind_of(symbol.kind, &symbol.ty), if is_stub { 2 } else { 1 });
        out.detail = detail_of(&symbol.name, &symbol.ty);
        out.documentation = symbol.doc.as_ref().map(|d| Documentation::MarkupContent(markdown(d.to_string())));
        if symbol.deprecated {
            out.tags = Some(vec![CompletionItemTag::DEPRECATED]);
        }
        items.push(out);
    }

    for keyword in KEYWORDS.iter().filter(|k| matches(k)) {
        items.push(item(keyword, CompletionItemKind::KEYWORD, 3));
    }
    if snippets {
        for (label, body, description) in SNIPPETS.iter().filter(|(label, ..)| matches(label)) {
            items.push(snippet_item(label, body, description));
        }
        if matches("onCache") {
            items.push(on_cache_snippet(infer, "lib."));
        }
    }

    let mut incomplete = false;
    if prefix.len() >= 3 {
        let file_side = ws.index.file(doc.file).and_then(|f| f.side);
        let side = qbx_lua_analysis::side_guard::SideRegions::of(&doc.text, &doc.chunk)
            .effective(offset, file_side)
            .unwrap_or(Side::Shared);
        let mut count = 0;
        for native in natives().filter(|n| matches(n.name) && n.side.is_available_on(side)) {
            if native.name.starts_with("N_0x") || !seen.insert(native.name.to_string()) {
                continue;
            }
            if count == MAX_NATIVES {
                incomplete = true;
                break;
            }
            count += 1;
            let mut out = item(native.name, CompletionItemKind::FUNCTION, 5);
            out.detail = Some(native.signature());
            out.data = Some(json!({ "native": native.name }));
            if native.alias_of.is_some() {
                out.tags = Some(vec![CompletionItemTag::DEPRECATED]);
            }
            items.push(out);
        }
    } else {
        incomplete = true;
    }
    (items, incomplete)
}

pub fn resolve(mut item: CompletionItem) -> CompletionItem {
    let name = item.data.as_ref().and_then(|d| d.get("native")).and_then(|n| n.as_str()).map(str::to_string);
    if let Some(native) = name.as_deref().and_then(native) {
        let mut text = lua_block(&native.signature());
        text.push_str(&format!("\n\n*{} native* · `{}`", native.side.label(), native.namespace));
        if let Some(docs) = native_docs(native.name) {
            text.push_str("\n\n");
            text.push_str(&docs);
        }
        item.documentation = Some(Documentation::MarkupContent(markdown(text)));
    }
    item
}
