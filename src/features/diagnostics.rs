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

pub fn diagnostics(ws: &Workspace, doc: &Document, rule_overrides: &[(String, Level)]) -> Vec<Diagnostic> {
    let mut config = ws.lint_config.for_file(&doc.path);
    for (code, level) in rule_overrides {
        config.set(code, *level);
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
        check_manifest(&ManifestInput {
            source: &doc.text,
            chunk: &doc.chunk,
            manifest: &manifest,
            config: &config,
            resource_files: &all_files(&resource.root),
            has_lua_scripts: !resource.files.is_empty(),
        })
    } else {
        let summary = summarize(&doc.chunk, &doc.resolution);
        let env = resource_id.map(|id| ws.resource_env(id));
        let resource_input = match (resource, &env) {
            (Some(resource), Some(env)) => {
                Some(ResourceInput { name: &resource.name, env, manifest: &resource.manifest })
            }
            _ => None,
        };
        check_file(&FileInput {
            source: &doc.text,
            chunk: &doc.chunk,
            resolution: &doc.resolution,
            summary: &summary,
            config: &config,
            side: entry.and_then(|f| f.side),
            resource: resource_input,
        })
    };

    found
        .into_iter()
        .map(|d| {
            let fix = d.fix.as_ref().map(|fix| FixData {
                title: fix.title.clone(),
                edits: fix.edits.iter().map(|e| (doc.range(e.span), e.new_text.clone())).collect(),
            });
            Diagnostic {
                range: doc.range(d.span),
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
