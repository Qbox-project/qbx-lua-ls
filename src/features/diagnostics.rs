use std::path::Path;

use lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString};
use qbx_lua_analysis::lint::all_files;
use qbx_lua_analysis::summary::summarize;
use qbx_lua_analysis::{check_file, check_manifest, FileInput, Level, ManifestInput, ResourceInput, Severity, Tag};
use serde::{Deserialize, Serialize};

use crate::document::Document;
use crate::workspace::Workspace;

pub const SOURCE: &str = "qbx-lint";

/// Carried in `Diagnostic.data` so code actions can offer the fix without re-running the linter.
#[derive(Serialize, Deserialize)]
pub struct FixData {
    pub title: String,
    pub edits: Vec<(lsp_types::Range, String)>,
}

pub fn is_silenced(ws: &Workspace, path: &Path) -> bool {
    ws.lint_config.is_excluded(path) || ws.lint_config.ignores_diagnostics(path)
}

pub fn diagnostics(
    ws: &Workspace,
    doc: &Document,
    rule_overrides: &[(String, Level)],
    crossrefs: &qbx_lua_analysis::crossref::CrossRefs,
) -> Vec<Diagnostic> {
    diagnostics_with_support(ws, doc, rule_overrides, crossrefs, None)
}

pub(crate) struct DiagnosticSupport<'a> {
    pub locale: Option<&'a qbx_lua_analysis::locale::LocaleFile>,
    pub resource_files: &'a [String],
    pub inventory_complete: bool,
    pub start_order: Option<&'a qbx_lua_analysis::startup::StartOrder>,
    pub start_order_complete: bool,
}

pub(crate) fn diagnostics_with_support(
    ws: &Workspace,
    doc: &Document,
    rule_overrides: &[(String, Level)],
    crossrefs: &qbx_lua_analysis::crossref::CrossRefs,
    support: Option<&DiagnosticSupport<'_>>,
) -> Vec<Diagnostic> {
    // Escrow-encrypted and binary files can still be opened in the editor; they are not Lua.
    if qbx_lua_analysis::project::is_not_source(doc.text.as_bytes()) || is_silenced(ws, &doc.path) {
        return Vec::new();
    }
    let mut config = ws.lint_config.for_file(&doc.path);
    for (code, level) in rule_overrides {
        config.set(code, *level);
    }
    if support.is_some_and(|support| !support.inventory_complete) {
        config.set(qbx_lua_analysis::rules::MANIFEST_MISSING_FILE, Level::Off);
    }
    if support.is_some_and(|support| !support.start_order_complete) {
        config.set(qbx_lua_analysis::rules::MANIFEST_MISSING_DEPENDENCY, Level::Off);
        config.set(qbx_lua_analysis::rules::RESOURCE_NOT_FOUND, Level::Off);
    }
    let entry = ws.index.file(doc.file);
    let resource_id = entry.and_then(|f| f.resource);
    let resource = resource_id.and_then(|id| ws.index.resource(id));

    let found = if doc.is_manifest() {
        let Some(resource) = resource.or_else(|| ws.index.resources.iter().find(|r| r.manifest_path == doc.path))
        else {
            return Vec::new();
        };
        let manifest = qbx_lua_analysis::manifest::Manifest::from_chunk(&doc.chunk);
        let files = if support.is_none() { all_files(&resource.root) } else { Vec::new() };
        check_manifest(&ManifestInput {
            source: &doc.text,
            chunk: &doc.chunk,
            manifest: &manifest,
            config: &config,
            resource_files: support.map_or(files.as_slice(), |support| support.resource_files),
            has_lua_scripts: !resource.files.is_empty(),
        })
    } else {
        let summary = summarize(&doc.chunk, &doc.resolution);
        let is_map = resource.is_some_and(|r| {
            r.manifest.is_map_file(&qbx_lua_analysis::project::relative_slash_path(&r.root, &doc.path))
        });
        if is_map {
            config.set(qbx_lua_analysis::rules::UNDEFINED_GLOBAL, Level::Off);
        }
        let env = resource_id.map(|id| ws.resource_env(id));
        let owned_order = if support.is_none() {
            resource.and_then(|r| qbx_lua_analysis::startup::StartOrder::discover(&r.root))
        } else {
            None
        };
        let start_order = support.and_then(|support| support.start_order).or(owned_order.as_deref());
        let started_before = resource.zip(start_order).map(|(r, order)| order.started_before(&r.name));
        let resource_input = match (resource, &env) {
            (Some(resource), Some(env)) => Some(ResourceInput {
                name: &resource.name,
                env,
                manifest: &resource.manifest,
                started_before: started_before.as_ref(),
                installed: start_order.map(|order| &order.installed),
            }),
            _ => None,
        };
        let relative_path =
            resource.map(|r| qbx_lua_analysis::project::relative_slash_path(&r.root, &doc.path)).unwrap_or_default();
        let owned_locale = if support.is_none() {
            resource.and_then(|r| qbx_lua_analysis::locale::LocaleFile::load(&r.root))
        } else {
            None
        };
        check_file(&FileInput {
            relative_path: &relative_path,
            source: &doc.text,
            chunk: &doc.chunk,
            resolution: &doc.resolution,
            summary: &summary,
            config: &config,
            side: entry.and_then(|f| f.side),
            resource: resource_input,
            crossrefs: Some(crossrefs),
            locale: support.and_then(|support| support.locale).or(owned_locale.as_ref()),
        })
    };

    let positions = support.map(|_| super::assistant::InspectionPositions::new(&doc.text));
    found
        .into_iter()
        .take(if support.is_some() { 20_000 } else { usize::MAX })
        .map(|d| {
            let range = |span| positions.as_ref().map_or_else(|| doc.range(span), |positions| positions.range(span));
            let fix = d.fix.as_ref().map(|fix| FixData {
                title: fix.title.clone(),
                edits: fix.edits.iter().map(|e| (range(e.span), e.new_text.clone())).collect(),
            });
            Diagnostic {
                range: range(d.span),
                severity: Some(match d.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                    Severity::Info => DiagnosticSeverity::INFORMATION,
                    Severity::Hint => DiagnosticSeverity::HINT,
                }),
                code: Some(NumberOrString::String(d.code.to_string())),
                source: Some(SOURCE.to_string()),
                message: d.message,
                tags: d.tag.map(|tag| {
                    vec![match tag {
                        Tag::Unnecessary => DiagnosticTag::UNNECESSARY,
                        Tag::Deprecated => DiagnosticTag::DEPRECATED,
                    }]
                }),
                data: fix.and_then(|f| serde_json::to_value(f).ok()),
                ..Diagnostic::default()
            }
        })
        .collect()
}
