//! Bounded source metadata for the on-demand asset browser. Asset bytes stay in the editor.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use lsp_types::Location;
use qbx_lua_analysis::project::{is_manifest_file, is_not_source};
use qbx_lua_analysis::scope::Resolved;
use qbx_lua_syntax::ast::{Expr, ExprKind, StmtKind, TableField, UnOp};
use qbx_lua_syntax::lexer::NumberValue;
use qbx_lua_syntax::visit::{walk_expr, Visitor};
use serde::Serialize;

use super::resource_details::{identity, normalized, selected_resource, DetailsParams, ResourceIdentity};
use crate::document::Document;
use crate::framework_callbacks::global_field_redefined;
use crate::index::{FileOrigin, Index};
use crate::infer::FileContext;
use crate::server::Documents;
use crate::workspace::path_to_uri;

const DECLARATIONS: usize = 1000;
const REFERENCES: usize = 2000;
const VALUE_BYTES: usize = 4096;
pub(crate) const SOURCE_BYTES: usize = 2 * 1024 * 1024;
const TOTAL_BYTES: usize = 32 * 1024 * 1024;
const FILES: usize = 2000;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetDeclaration {
    pub kind: &'static str,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    pub location: Location,
}

#[derive(Debug, Serialize)]
pub struct AssetReference {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dictionary: Option<String>,
    pub location: Location,
}

#[derive(Debug, Default, Serialize)]
pub struct Omitted {
    pub declarations: usize,
    pub references: usize,
}

#[derive(Debug, Serialize)]
pub struct ResourceAssets {
    pub resource: ResourceIdentity,
    pub declarations: Vec<AssetDeclaration>,
    pub references: Vec<AssetReference>,
    pub truncated: Omitted,
    pub notes: Vec<String>,
}

/// Prevent growth races and avoid parsing escrow/binary files as editable Lua.
pub(crate) fn read_bounded_source(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|_| "Source file is unavailable.")?;
    let stat = file.metadata().map_err(|_| "Source file is unavailable.")?;
    if !stat.is_file() || stat.len() > SOURCE_BYTES as u64 {
        return Err("Source exceeds the 2 MiB inspection limit.".into());
    }
    let mut bytes = Vec::new();
    file.take(SOURCE_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|_| "Source file is unavailable.")?;
    if bytes.len() > SOURCE_BYTES {
        return Err("Source exceeds the 2 MiB inspection limit.".into());
    }
    if is_not_source(&bytes) {
        return Err("Encrypted or binary source is not readable.".into());
    }
    String::from_utf8(bytes).map_err(|_| "Source is not valid UTF-8.".into())
}

fn unparen(mut expr: &Expr) -> &Expr {
    while let ExprKind::Paren(inner) = &expr.kind {
        expr = inner;
    }
    expr
}

fn strings<'a>(expr: &'a Expr, result: &mut Vec<&'a Expr>) {
    match &unparen(expr).kind {
        ExprKind::String(_) => result.push(unparen(expr)),
        ExprKind::Table(fields) => {
            for field in fields {
                if let TableField::Positional(value) = field {
                    if matches!(unparen(value).kind, ExprKind::String(_)) {
                        result.push(unparen(value));
                    }
                }
            }
        }
        _ => {}
    }
}

fn manifest(doc: &Document, result: &mut ResourceAssets) {
    for stmt in &doc.chunk.block.stmts {
        let StmtKind::Expr(first_expr) = &stmt.kind else { continue };
        let mut expr = first_expr;
        let mut groups = Vec::new();
        let name = loop {
            match &expr.kind {
                ExprKind::Call { callee, args, .. } => {
                    groups.push(args.as_slice());
                    expr = callee;
                }
                ExprKind::Name(name) => break name.text.as_str(),
                _ => break "",
            }
        };
        groups.reverse();
        let kind = match name {
            "file" | "files" => "file",
            "client_script" | "client_scripts" => "client_script",
            "server_script" | "server_scripts" => "server_script",
            "shared_script" | "shared_scripts" => "shared_script",
            "ui_page" => "ui_page",
            "loadscreen" => "loadscreen",
            "data_file" => "data_file",
            "map" => "map",
            _ => continue,
        };
        let Some(first) = groups.first() else { continue };
        let (args, data_type) = if kind == "data_file" {
            let Some(Expr { kind: ExprKind::String(ty), .. }) = first.first() else { continue };
            if ty.len() > VALUE_BYTES {
                result.truncated.declarations += 1;
                continue;
            }
            let paths = if first.len() > 1 { &first[1..] } else { groups.get(1).copied().unwrap_or_default() };
            (paths, Some(ty.to_string()))
        } else {
            (*first, None)
        };
        for arg in args {
            let mut values = Vec::new();
            strings(arg, &mut values);
            for value in values {
                let ExprKind::String(text) = &value.kind else { continue };
                if text.len() > VALUE_BYTES || result.declarations.len() >= DECLARATIONS {
                    result.truncated.declarations += 1;
                    continue;
                }
                result.declarations.push(AssetDeclaration {
                    kind,
                    value: text.to_string(),
                    data_type: data_type.clone(),
                    location: Location::new(doc.uri.clone(), doc.range(value.span)),
                });
            }
        }
    }
}

fn argument(name: &str) -> Option<(&'static str, usize)> {
    Some(match name {
        "RequestModel"
        | "HasModelLoaded"
        | "SetModelAsNoLongerNeeded"
        | "IsModelValid"
        | "IsModelInCdimage"
        | "CreateObject"
        | "CreateObjectNoOffset"
        | "CreateVehicle" => ("model", 0),
        "CreatePed" => ("model", 1),
        "CreatePedInsideVehicle" => ("model", 2),
        "RequestStreamedTextureDict"
        | "HasStreamedTextureDictLoaded"
        | "SetStreamedTextureDictAsNoLongerNeeded"
        | "DrawSprite" => ("textureDictionary", 0),
        "RequestNamedPtfxAsset"
        | "HasNamedPtfxAssetLoaded"
        | "RemoveNamedPtfxAsset"
        | "UseParticleFxAsset"
        | "UseParticleFxAssetNextCall" => ("particleAsset", 0),
        "RequestScriptAudioBank" | "RequestMissionAudioBank" | "RequestAmbientAudioBank" => ("audioBank", 0),
        _ => return None,
    })
}

fn ascii_hash(value: &str) -> Option<u32> {
    if !value.is_ascii() || value.contains('\0') {
        return None;
    }
    let mut hash = 0u32;
    for byte in value.bytes() {
        hash = hash.wrapping_add(u32::from(byte.to_ascii_lowercase()));
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    Some(hash.wrapping_add(hash << 15))
}

struct References<'a, 'b> {
    doc: &'a Document,
    ctx: FileContext<'a>,
    index: &'a Index,
    global_cache: BTreeMap<String, bool>,
    result: &'b mut ResourceAssets,
}

impl References<'_, '_> {
    fn global(&mut self, expr: &Expr) -> Option<String> {
        let ExprKind::Name(name) = &unparen(expr).kind else { return None };
        if !matches!(self.doc.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
            || self.doc.resolution.lookup_local_at("_ENV", name.span.start).is_some()
        {
            return None;
        }
        let available = if let Some(available) = self.global_cache.get(name.text.as_str()) {
            *available
        } else {
            let modified = |target: &str| {
                self.doc.resolution.globals.iter().any(|global| global.name == target && global.is_definition())
                    || self
                        .index
                        .globals_named(target, self.doc.file)
                        .iter()
                        .any(|(id, _)| self.index.file(*id).is_some_and(|entry| entry.origin != FileOrigin::Stub))
                    || global_field_redefined(&self.ctx, target)
            };
            let available = !modified("_ENV") && !modified(&name.text);
            self.global_cache.insert(name.text.to_string(), available);
            available
        };
        available.then(|| name.text.to_string())
    }

    fn value(&mut self, expr: &Expr, model: bool) -> Option<(Option<String>, Option<u32>)> {
        let expr = unparen(expr);
        match &expr.kind {
            ExprKind::String(value) => Some((Some(value.to_string()), model.then(|| ascii_hash(value)).flatten())),
            ExprKind::JenkinsHash(value) if model => Some((Some(value.to_string()), ascii_hash(value))),
            ExprKind::Number(NumberValue::Int(value)) if model && (-2_147_483_648..=4_294_967_295).contains(value) => {
                Some((None, Some(*value as u32)))
            }
            ExprKind::Unary { op: UnOp::Neg, expr } if model => {
                if let ExprKind::Number(NumberValue::Int(value)) = unparen(expr).kind {
                    if (0..=2_147_483_648).contains(&value) {
                        return Some((None, Some((-value) as u32)));
                    }
                }
                None
            }
            ExprKind::Call { callee, args, .. } if model && args.len() == 1 => {
                let name = self.global(callee)?;
                if name == "GetHashKey" || name == "joaat" {
                    if let ExprKind::String(value) = &unparen(&args[0]).kind {
                        Some((Some(value.to_string()), ascii_hash(value)))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn add(&mut self, kind: &'static str, expr: &Expr, dictionary: Option<String>) {
        let Some((value, hash)) = self.value(expr, kind == "model") else { return };
        if value.as_ref().is_some_and(|value| value.is_empty()) {
            return;
        }
        if self.result.references.len() >= REFERENCES
            || value.as_ref().is_some_and(|value| value.len() > VALUE_BYTES)
            || dictionary.as_ref().is_some_and(|value| value.len() > VALUE_BYTES)
        {
            self.result.truncated.references += 1;
            return;
        }
        self.result.references.push(AssetReference {
            kind,
            value,
            hash,
            dictionary,
            location: Location::new(self.doc.uri.clone(), self.doc.range(expr.span)),
        });
    }
}

impl<'ast> Visitor<'ast> for References<'_, '_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let ExprKind::Call { callee, args, .. } = &expr.kind {
            if let ExprKind::Name(name) = &unparen(callee).kind {
                if let Some((kind, position)) = argument(&name.text) {
                    if self.global(callee).is_some() {
                        if let Some(value) = args.get(position) {
                            self.add(kind, value, None);
                        }
                        if name.text == "DrawSprite" && args.len() >= 2 {
                            let dictionary = args[0].as_string().map(ToString::to_string);
                            self.add("texture", &args[1], dictionary);
                        }
                    }
                }
            }
        }
        walk_expr(self, expr);
    }
}

pub fn resource(index: &Index, docs: &Documents, params: DetailsParams) -> Result<ResourceAssets, String> {
    let selected = selected_resource(index, params)?;
    let root = normalized(&selected.root);
    let mut result = ResourceAssets { resource: identity(selected), declarations: Vec::new(), references: Vec::new(),
        truncated: Omitted::default(), notes: vec![
            "Literal manifest paths and supported native asset arguments are shown; computed paths, aliases and runtime registrations may be absent.".into(),
            "Unresolved Lua asset names may belong to the base game or another resource. They are not proof of a missing file.".into(),
        ] };
    let manifest_uri = path_to_uri(&selected.manifest_path);
    let manifest_owned;
    let manifest_doc = if let Some(doc) = docs.get(&manifest_uri) {
        doc
    } else {
        manifest_owned = Document::new(
            manifest_uri,
            selected.manifest_path.clone(),
            0,
            read_bounded_source(&selected.manifest_path)?,
        );
        &manifest_owned
    };
    if manifest_doc.text.len() > SOURCE_BYTES {
        return Err("Manifest exceeds the 2 MiB inspection limit.".into());
    }
    manifest(manifest_doc, &mut result);
    let mut files: Vec<_> = index
        .files()
        .filter(|(_, file)| {
            file.origin != FileOrigin::Stub
                && !is_manifest_file(&file.path)
                && file.resource.and_then(|id| index.resource(id)).is_some_and(|owner| normalized(&owner.root) == root)
        })
        .collect();
    files.sort_by(|(_, a), (_, b)| a.uri.as_str().cmp(b.uri.as_str()));
    let mut skipped = files.len().saturating_sub(FILES);
    let candidates = files.len().min(FILES);
    let mut bytes = manifest_doc.text.len();
    for (slot, (id, file)) in files.into_iter().take(FILES).enumerate() {
        let owned;
        let doc = if let Some(doc) = docs.get(&file.uri) {
            doc
        } else {
            let Ok(text) = read_bounded_source(&file.path) else {
                skipped += 1;
                continue;
            };
            if bytes + text.len() > TOTAL_BYTES {
                skipped += candidates - slot;
                break;
            }
            owned = {
                let mut doc = Document::new(file.uri.clone(), file.path.clone(), 0, text);
                doc.file = id;
                doc
            };
            &owned
        };
        if doc.text.len() > SOURCE_BYTES {
            skipped += 1;
            continue;
        }
        if bytes + doc.text.len() > TOTAL_BYTES {
            skipped += candidates - slot;
            break;
        }
        bytes += doc.text.len();
        let ctx = FileContext::new(id, &doc.text, &doc.chunk, &doc.resolution);
        References { doc, ctx, index, global_cache: BTreeMap::new(), result: &mut result }
            .visit_block(&doc.chunk.block);
    }
    result.references.sort_by(|a, b| {
        (a.location.uri.as_str(), a.location.range.start.line, a.location.range.start.character, a.kind).cmp(&(
            b.location.uri.as_str(),
            b.location.range.start.line,
            b.location.range.start.character,
            b.kind,
        ))
    });
    if skipped > 0 {
        result.notes.push(format!("{skipped} Lua files were not inspected because they were unreadable or exceeded inspection limits (2,000 files, 2 MiB per file, 32 MiB total)."));
    }
    if result.truncated.declarations + result.truncated.references > 0 {
        result.notes.push("Some rows were omitted to keep results bounded (1,000 declarations, 2,000 references and 4 KiB per value).".into());
    }
    Ok(result)
}
