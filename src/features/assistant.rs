//! Explicit, read-only, paginated views for editor agents and the standalone MCP adapter.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{DiagnosticSeverity, Location, NumberOrString, Position, Range, Url};
use qbx_lua_analysis::startup::StartOrder;
use qbx_lua_analysis::{locale, Level};
use serde::{Deserialize, Serialize};

use super::resource_assets::SOURCE_BYTES;
use super::resource_details::{identity, normalized, ResourceIdentity};
use crate::document::Document;
use crate::features::{diagnostics, references};
use crate::index::{FileId, FileOrigin};
use crate::server::Documents;
use crate::workspace::{path_to_uri, uri_to_path, Workspace};

const MAX_FILES: usize = 2000;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESULTS: usize = 20_000;
const MAX_DIRECTORY_ENTRIES: usize = 20_000;

/// Shared agent-only read limits, including auxiliary locale files. Editor LSP features stay unchanged.
pub(crate) struct InspectionBudget {
    roots: Vec<PathBuf>,
    seen: BTreeMap<PathBuf, usize>,
    omitted: BTreeSet<PathBuf>,
    bytes: usize,
    sources: BTreeMap<PathBuf, String>,
    pub result_limit: bool,
}
impl InspectionBudget {
    pub fn new(roots: &[PathBuf]) -> Self {
        Self {
            roots: roots.iter().filter_map(|root| std::fs::canonicalize(root).ok()).collect(),
            seen: BTreeMap::new(),
            omitted: BTreeSet::new(),
            bytes: 0,
            sources: BTreeMap::new(),
            result_limit: false,
        }
    }
    pub fn skip(&mut self, path: &Path) {
        self.omitted.insert(path.to_path_buf());
    }
    pub fn omitted(&self) -> usize {
        self.omitted.len()
    }
    fn contains(&self, path: &Path) -> bool {
        std::fs::canonicalize(path).ok().is_some_and(|path| self.roots.iter().any(|root| path.starts_with(root)))
    }
    pub fn claim(&mut self, path: &Path, bytes: usize) -> bool {
        let extra = bytes.saturating_sub(self.seen.get(path).copied().unwrap_or(0));
        if bytes > SOURCE_BYTES
            || self.bytes + extra > MAX_BYTES
            || (!self.seen.contains_key(path) && self.seen.len() >= MAX_FILES)
        {
            self.skip(path);
            return false;
        }
        self.seen.entry(path.to_path_buf()).and_modify(|count| *count = (*count).max(bytes)).or_insert(bytes);
        self.bytes += extra;
        true
    }
    pub fn read(&mut self, path: &Path) -> Option<String> {
        if let Some(source) = self.sources.get(path) {
            return Some(source.clone());
        }
        if self.omitted.contains(path) {
            return None;
        }
        if (!self.seen.contains_key(path) && self.seen.len() >= MAX_FILES) || self.bytes >= MAX_BYTES {
            self.skip(path);
            return None;
        }
        // Attempts count even when metadata/open/decoding fails. Otherwise many invalid files
        // could each consume the full per-file read allowance without using the shared budget.
        if !self.claim(path, 0) {
            return None;
        }
        let mut load = || -> Option<String> {
            let canonical = std::fs::canonicalize(path).ok()?;
            if !self.roots.iter().any(|root| canonical.starts_with(root)) {
                return None;
            }
            let stat = std::fs::metadata(&canonical).ok()?;
            let remaining = SOURCE_BYTES.min(MAX_BYTES - self.bytes);
            if !stat.is_file() || stat.len() > remaining as u64 {
                return None;
            }
            if !self.claim(path, stat.len() as usize) {
                return None;
            }
            let file = std::fs::File::open(canonical).ok()?;
            let mut data = Vec::new();
            let result = file.take(remaining as u64 + 1).read_to_end(&mut data);
            if !self.claim(path, data.len().min(remaining)) {
                return None;
            }
            result.ok()?;
            if data.len() > remaining || qbx_lua_analysis::project::is_not_source(&data) {
                return None;
            }
            String::from_utf8(data).ok()
        };
        match load() {
            Some(text) if self.claim(path, text.len()) => {
                self.sources.insert(path.to_path_buf(), text.clone());
                Some(text)
            }
            _ => {
                self.skip(path);
                None
            }
        }
    }
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.omitted.is_empty() {
            notes.push(format!("{} candidate files were omitted because they were unreadable, outside the workspace roots, or exceeded inspection limits (2,000 files, 2 MiB each, 32 MiB total, including supporting data). Results are partial.", self.omitted.len()));
        }
        if self.result_limit {
            notes
                .push("Results reached the 20,000-entry inspection limit and may be partial. Narrow the query.".into());
        }
        notes
    }
}

/// Bounded snapshots can return many spans on one minified line. Sparse UTF-16 checkpoints
/// avoid rescanning that entire line for each result, including non-ASCII JSON keys.
pub(crate) struct InspectionPositions<'a> {
    source: &'a str,
    checkpoints: Vec<(usize, Position)>,
}
impl<'a> InspectionPositions<'a> {
    pub fn new(source: &'a str) -> Self {
        let mut checkpoints = vec![(0, Position::new(0, 0))];
        let mut position = Position::new(0, 0);
        let mut last = 0;
        for (offset, character) in source.char_indices() {
            if offset - last >= 256 {
                checkpoints.push((offset, position));
                last = offset;
            }
            if character == '\n' {
                position.line += 1;
                position.character = 0;
            } else {
                position.character += character.len_utf16() as u32;
            }
        }
        Self { source, checkpoints }
    }
    fn position(&self, offset: u32) -> Position {
        let mut offset = (offset as usize).min(self.source.len());
        while !self.source.is_char_boundary(offset) {
            offset -= 1;
        }
        let checkpoint = self.checkpoints.partition_point(|(start, _)| *start <= offset) - 1;
        let (start, mut position) = self.checkpoints[checkpoint];
        for character in self.source[start..offset].chars() {
            if character == '\n' {
                position.line += 1;
                position.character = 0;
            } else {
                position.character += character.len_utf16() as u32;
            }
        }
        position
    }
    pub fn range(&self, span: qbx_lua_syntax::Span) -> Range {
        Range::new(self.position(span.start), self.position(span.end.max(span.start)))
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcesParams {
    pub query: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsParams {
    pub uri: Option<Url>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReferencesParams {
    pub uri: Url,
    pub line: u32,
    pub character: u32,
    pub include_declaration: Option<bool>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub notes: Vec<String>,
}

fn paging(offset: Option<usize>, limit: Option<usize>, max: usize) -> Result<(usize, usize), String> {
    let (offset, limit) = (offset.unwrap_or(0), limit.unwrap_or(50));
    if offset > 1_000_000 || limit == 0 || limit > max {
        return Err(format!("offset must be 0–1,000,000 and limit 1–{max}"));
    }
    Ok((offset, limit))
}

fn page<T>(items: Vec<T>, offset: usize, limit: usize, notes: Vec<String>) -> Page<T> {
    Page { total: items.len(), items: items.into_iter().skip(offset).take(limit).collect(), offset, limit, notes }
}

pub fn resources(ws: &Workspace, params: ResourcesParams) -> Result<Page<ResourceIdentity>, String> {
    let (offset, limit) = paging(params.offset, params.limit, 100)?;
    let query = params.query.unwrap_or_default();
    if query.chars().count() > 256 || query.chars().any(char::is_control) {
        return Err("Resource query must contain at most 256 characters of printable text.".into());
    }
    let query = query.to_lowercase();
    let mut items: Vec<_> = ws
        .index
        .resources
        .iter()
        .filter(|resource| {
            query.is_empty()
                || resource.name.to_lowercase().contains(&query)
                || resource.root.to_string_lossy().to_lowercase().contains(&query)
        })
        .map(identity)
        .collect();
    items.sort_by(|a, b| (&a.name, a.uri.as_str()).cmp(&(&b.name, b.uri.as_str())));
    Ok(page(items, offset, limit, Vec::new()))
}

fn local_path(uri: &Url) -> Result<std::path::PathBuf, String> {
    if uri.scheme() != "file" || uri.query().is_some() || uri.fragment().is_some() {
        return Err("Use an indexed file URI without query or fragment.".into());
    }
    uri_to_path(uri).ok_or_else(|| "Use a local file URI.".into())
}

pub fn symbol_references(ws: &Workspace, docs: &Documents, params: ReferencesParams) -> Result<Page<Location>, String> {
    let (offset, limit) = paging(params.offset, params.limit, 200)?;
    let path = local_path(&params.uri)?;
    let id = ws.index.file_id(&path).ok_or("The Lua file is not indexed.")?;
    let entry =
        ws.index.file(id).filter(|entry| entry.origin != FileOrigin::Stub).ok_or("The Lua file is not indexed.")?;
    if entry.path.extension().is_none_or(|ext| !ext.eq_ignore_ascii_case("lua")) {
        return Err("Choose an indexed Lua source file.".into());
    }
    let mut budget = InspectionBudget::new(&ws.roots);
    let owned;
    let doc = if let Some(doc) = docs.get(&entry.uri) {
        doc
    } else {
        owned = {
            let mut doc = Document::new(
                entry.uri.clone(),
                entry.path.clone(),
                0,
                budget.read(&entry.path).ok_or("Source is unavailable or exceeds inspection limits.")?,
            );
            doc.file = id;
            doc
        };
        &owned
    };
    if !budget.claim(&doc.path, doc.text.len()) {
        return Err("The source exceeds the 2 MiB inspection limit.".into());
    }
    let position = Position::new(params.line, params.character);
    if doc.position(doc.offset(position)) != position {
        return Err("The UTF-16 position is outside the source or inside a surrogate pair.".into());
    }
    let mut items = references::references_bounded(
        ws,
        docs,
        doc,
        position,
        params.include_declaration.unwrap_or(true),
        &mut budget,
    );
    items.retain(|item| item.uri.scheme() == "file");
    items.sort_by(|a, b| {
        (a.uri.as_str(), a.range.start.line, a.range.start.character, a.range.end.line, a.range.end.character).cmp(&(
            b.uri.as_str(),
            b.range.start.line,
            b.range.start.character,
            b.range.end.line,
            b.range.end.character,
        ))
    });
    items.dedup();
    Ok(page(items, offset, limit, budget.notes()))
}

#[derive(Debug, Serialize)]
pub struct DiagnosticItem {
    pub uri: Url,
    pub range: Range,
    pub severity: DiagnosticSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<NumberOrString>,
    pub message: String,
}

#[derive(Default)]
struct SupportingData {
    locale: Option<Option<locale::LocaleFile>>,
    inventory: Option<(Vec<String>, bool)>,
}

fn bounded_locale(
    root: &Path,
    budget: &mut InspectionBudget,
    entries: &mut usize,
    notes: &mut Vec<String>,
) -> Option<locale::LocaleFile> {
    let dir = root.join("locales");
    if !dir.exists() {
        return None;
    }
    if !budget.contains(&dir) {
        budget.skip(&dir);
        return None;
    }
    let preferred = dir.join("en.json");
    let path = if preferred.is_file() {
        preferred
    } else {
        let Ok(mut listing) = std::fs::read_dir(&dir) else {
            budget.skip(&dir);
            return None;
        };
        let mut first: Option<PathBuf> = None;
        while *entries > 0 {
            let Some(entry) = listing.next() else { break };
            *entries -= 1;
            let Ok(entry) = entry else {
                budget.skip(&dir);
                return None;
            };
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") && first.as_ref().is_none_or(|first| path < *first) {
                first = Some(path);
            }
        }
        if listing.next().is_some() {
            notes.push("Locale directory inspection reached the 20,000-entry shared directory limit; locale checks for this resource were omitted.".into());
            return None;
        }
        first?
    };
    let source = budget.read(&path)?;
    // The lightweight locale scanner is recursive. Serde validates syntax and bounds nesting
    // before that scanner sees untrusted input; neither parser reads from disk itself.
    if serde_json::from_str::<serde_json::Value>(&source).is_err() {
        notes.push(format!(
            "Locale checks were omitted for {} because its JSON is invalid or exceeds the nesting limit.",
            path.display()
        ));
        return None;
    }
    Some(locale::LocaleFile::parse(path, source))
}

fn bounded_inventory(root: &Path, budget: &mut InspectionBudget, entries: &mut usize) -> (Vec<String>, bool) {
    if !budget.contains(root) {
        budget.skip(root);
        return (Vec::new(), false);
    }
    let mut files = Vec::new();
    let mut complete = true;
    let mut walker = walkdir::WalkDir::new(root).into_iter().filter_entry(|entry| {
        entry.depth() == 0 || (entry.file_name() != "node_modules" && entry.file_name() != ".git")
    });
    while *entries > 0 {
        let Some(entry) = walker.next() else { break };
        *entries -= 1;
        match entry {
            Ok(entry) if entry.file_type().is_file() => {
                files.push(qbx_lua_analysis::project::relative_slash_path(root, entry.path()))
            }
            Ok(_) => {}
            Err(_) => complete = false,
        }
    }
    if walker.next().is_some() {
        complete = false;
    }
    (files, complete)
}

pub fn diagnostic_snapshot(
    ws: &Workspace,
    docs: &Documents,
    params: DiagnosticsParams,
    enabled: bool,
    overrides: &[(String, Level)],
) -> Result<Page<DiagnosticItem>, String> {
    let (offset, limit) = paging(params.offset, params.limit, 200)?;
    let requested = params.uri.as_ref().map(local_path).transpose()?.map(|path| normalized(&path));
    let mut targets = BTreeMap::<Url, (std::path::PathBuf, Option<FileId>)>::new();
    for (id, file) in ws.index.files().filter(|(_, file)| file.origin != FileOrigin::Stub) {
        if requested.as_ref().is_some_and(|path| *path == normalized(&file.path))
            || (requested.is_none() && file.origin == FileOrigin::Workspace)
        {
            targets.insert(file.uri.clone(), (file.path.clone(), Some(id)));
        }
    }
    for resource in &ws.index.resources {
        if requested.as_ref().is_some_and(|path| *path == normalized(&resource.manifest_path))
            || (requested.is_none()
                && ws.roots.iter().any(|root| normalized(&resource.manifest_path).starts_with(normalized(root))))
        {
            targets.insert(
                path_to_uri(&resource.manifest_path),
                (resource.manifest_path.clone(), ws.index.file_id(&resource.manifest_path)),
            );
        }
    }
    if requested.is_some() && targets.is_empty() {
        return Err("Choose an indexed Lua file or resource manifest.".into());
    }
    if !enabled {
        return Ok(page(
            Vec::new(),
            offset,
            limit,
            vec!["Diagnostics are disabled by the language-server configuration.".into()],
        ));
    }
    let mut items = Vec::new();
    let mut notes = Vec::new();
    let mut budget = InspectionBudget::new(&ws.roots);
    let mut directory_entries = MAX_DIRECTORY_ENTRIES;
    let mut supporting = BTreeMap::<PathBuf, SupportingData>::new();
    let mut orders = BTreeMap::<PathBuf, Option<Arc<StartOrder>>>::new();
    let mut skipped = targets.len().saturating_sub(MAX_FILES);
    let crossrefs = ws.crossrefs();
    let mut usages = BTreeMap::<u32, Vec<locale::LocaleUsage>>::new();
    let mut incomplete = BTreeSet::new();
    for (uri, (path, id)) in targets.into_iter().take(MAX_FILES) {
        let resource = id.and_then(|id| ws.index.file(id)).and_then(|file| file.resource);
        let owned;
        let doc = if let Some(doc) = docs.get(&uri) {
            doc
        } else {
            let text = match budget.read(&path) {
                Some(text) => text,
                None => {
                    skipped += 1;
                    if let Some(resource) = resource {
                        incomplete.insert(resource);
                    }
                    continue;
                }
            };
            owned = {
                let mut doc = Document::new(uri.clone(), path, 0, text);
                doc.file = id.unwrap_or(u32::MAX);
                doc
            };
            &owned
        };
        if !budget.claim(&doc.path, doc.text.len()) {
            skipped += 1;
            if let Some(resource) = resource {
                incomplete.insert(resource);
            }
            continue;
        }
        if requested.is_none() {
            if let Some(resource) = resource {
                usages.entry(resource).or_default().push(locale::locale_usage(&doc.chunk));
            }
        }
        let entry = resource
            .and_then(|id| ws.index.resource(id))
            .or_else(|| ws.index.resources.iter().find(|resource| resource.manifest_path == doc.path));
        let root = entry.map(|entry| &entry.root);
        let mut start_order = None;
        let mut start_order_complete = true;
        if let Some(root) = root {
            let data = supporting.entry(root.clone()).or_default();
            if doc.is_manifest() {
                if data.inventory.is_none() {
                    let inventory = bounded_inventory(root, &mut budget, &mut directory_entries);
                    if !inventory.1 {
                        notes.push(format!("Manifest file checks for {} were omitted because directory inspection was incomplete (20,000 entries shared across supporting data). Other manifest checks remain available.", root.display()));
                    }
                    data.inventory = Some(inventory);
                }
            } else {
                if data.locale.is_none() {
                    data.locale = Some(bounded_locale(root, &mut budget, &mut directory_entries, &mut notes));
                }
                if let Some(cfg) = StartOrder::configuration_path(root) {
                    start_order = orders.entry(cfg.clone()).or_insert_with(|| {
                        let (order, partial) = StartOrder::discover_bounded(root, &mut |path| budget.read(path), &mut directory_entries);
                        if partial || order.is_none() {
                            notes.push(format!("Start-order checks from {} were omitted because configuration or installed-resource inspection was incomplete. Other diagnostics remain available.", cfg.display()));
                            None
                        } else { order.map(Arc::new) }
                    }).as_deref();
                    start_order_complete = start_order.is_some();
                }
            }
        }
        let data = root.and_then(|root| supporting.get(root));
        let support = diagnostics::DiagnosticSupport {
            locale: data.and_then(|data| data.locale.as_ref()).and_then(Option::as_ref),
            resource_files: data
                .and_then(|data| data.inventory.as_ref())
                .map_or(&[], |inventory| inventory.0.as_slice()),
            inventory_complete: data.and_then(|data| data.inventory.as_ref()).is_none_or(|inventory| inventory.1),
            start_order,
            start_order_complete,
        };
        for diagnostic in diagnostics::diagnostics_with_support(ws, doc, overrides, &crossrefs, Some(&support)) {
            if items.len() >= MAX_RESULTS {
                break;
            }
            items.push(DiagnosticItem {
                uri: uri.clone(),
                range: diagnostic.range,
                severity: diagnostic.severity.unwrap_or(DiagnosticSeverity::WARNING),
                code: diagnostic.code,
                message: diagnostic.message.chars().take(4096).collect(),
            });
        }
        if items.len() >= MAX_RESULTS {
            break;
        }
    }
    // Whole-workspace queries include the same unused locale-key checks as the Problems panel.
    // If any file was skipped, do not infer unused keys from incomplete usage information.
    if requested.is_none() && skipped == 0 && budget.omitted() == 0 && items.len() < MAX_RESULTS {
        for (id, usage) in usages {
            let Some(resource) = ws.index.resource(id).filter(|entry| !entry.escrowed && !incomplete.contains(&id))
            else {
                continue;
            };
            let Some(locale) =
                supporting.get(&resource.root).and_then(|data| data.locale.as_ref()).and_then(Option::as_ref)
            else {
                continue;
            };
            if diagnostics::is_silenced(ws, &locale.path) {
                continue;
            }
            let mut config = ws.lint_config.for_file(&locale.path);
            overrides.iter().for_each(|(code, level)| config.set(code, *level));
            let Some(severity) = config.severity(qbx_lua_analysis::rules::UNUSED_LOCALE_KEY) else { continue };
            let positions = InspectionPositions::new(&locale.source);
            for diagnostic in qbx_lua_analysis::lint::unused_locale_keys_from(locale, usage.into_iter()) {
                if items.len() >= MAX_RESULTS {
                    break;
                }
                items.push(DiagnosticItem {
                    uri: path_to_uri(&locale.path),
                    range: positions.range(diagnostic.span),
                    severity: match severity {
                        qbx_lua_analysis::Severity::Error => DiagnosticSeverity::ERROR,
                        qbx_lua_analysis::Severity::Warning => DiagnosticSeverity::WARNING,
                        qbx_lua_analysis::Severity::Info => DiagnosticSeverity::INFORMATION,
                        qbx_lua_analysis::Severity::Hint => DiagnosticSeverity::HINT,
                    },
                    code: Some(NumberOrString::String(diagnostic.code.to_string())),
                    message: diagnostic.message.chars().take(4096).collect(),
                });
            }
        }
    }
    if skipped > 0 {
        notes.push(format!("{skipped} source files were not inspected. Unused locale-key checks were omitted because usage information is incomplete. Query a specific file for a narrower snapshot."));
    }
    if items.len() >= MAX_RESULTS {
        budget.result_limit = true;
    }
    notes.extend(budget.notes());
    items.sort_by(|a, b| {
        (a.uri.as_str(), a.range.start.line, a.range.start.character, &a.message).cmp(&(
            b.uri.as_str(),
            b.range.start.line,
            b.range.start.character,
            &b.message,
        ))
    });
    Ok(page(items, offset, limit, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_budget_counts_unique_sources_growth_and_exhaustion() {
        let mut budget = InspectionBudget::new(&[]);
        let first = Path::new("first.lua");
        assert!(budget.claim(first, 10));
        assert!(budget.claim(first, 10));
        assert_eq!(budget.bytes, 10);
        assert!(budget.claim(first, SOURCE_BYTES));
        assert_eq!(budget.bytes, SOURCE_BYTES);
        assert!(!budget.claim(first, SOURCE_BYTES + 1));
        for index in 1..MAX_BYTES / SOURCE_BYTES {
            assert!(budget.claim(Path::new(&format!("file{index}.lua")), SOURCE_BYTES));
        }
        assert_eq!(budget.bytes, MAX_BYTES);
        assert!(!budget.claim(Path::new("too-many-bytes.lua"), 1));
        assert!(budget.read(Path::new("never-opened.lua")).is_none());
        assert!(budget.sources.is_empty());
        let mut files = InspectionBudget::new(&[]);
        for index in 0..MAX_FILES {
            assert!(files.claim(Path::new(&format!("file{index}.lua")), 0));
        }
        assert!(!files.claim(Path::new("extra.lua"), 0));
        assert!(!files.notes().is_empty());
    }

    #[test]
    fn inspection_positions_match_lsp_utf16_across_long_lines_and_char_boundaries() {
        let source = format!("{}\r\n{}\n", "a😀é".repeat(200), "z😀".repeat(180));
        let positions = InspectionPositions::new(&source);
        let lines = qbx_lua_syntax::LineIndex::new(&source);
        for offset in 0..source.len() as u32 + 5 {
            let expected = lines.line_col_utf16(&source, offset);
            assert_eq!(positions.position(offset), Position::new(expected.line, expected.col));
        }
    }

    #[test]
    fn inspection_budget_charges_invalid_utf8_before_rejecting_it() {
        let root = std::env::temp_dir().join(format!("qbx-assistant-budget-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                assert!(self.0.starts_with(std::env::temp_dir()));
                assert!(self.0.file_name().unwrap().to_string_lossy().starts_with("qbx-assistant-budget-"));
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let _cleanup = Cleanup(root.clone());
        let invalid = root.join("invalid.lua");
        let valid = root.join("valid.lua");
        std::fs::write(&invalid, vec![0xff; SOURCE_BYTES]).unwrap();
        std::fs::write(&valid, "print('valid')").unwrap();
        let mut budget = InspectionBudget::new(std::slice::from_ref(&root));
        for index in 1..MAX_BYTES / SOURCE_BYTES {
            assert!(budget.claim(&root.join(format!("file{index}.lua")), SOURCE_BYTES));
        }
        assert!(budget.read(&invalid).is_none());
        assert_eq!(budget.bytes, MAX_BYTES);
        assert_eq!(budget.seen.len(), MAX_BYTES / SOURCE_BYTES);
        assert!(budget.read(&valid).is_none(), "failed decoding still exhausts the shared read allowance");
        assert!(budget.sources.is_empty());
    }
}
