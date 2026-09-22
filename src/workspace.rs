use std::path::{Path, PathBuf};

use lsp_types::Url;
use qbx_fivem_data::{known_import, Side, STUBS};
use qbx_lua_analysis::manifest::Manifest;
use qbx_lua_analysis::project::{
    find_manifest_dir, is_manifest_file, lua_files_under, manifest_path, read_source, relative_slash_path, side_of,
    split_import, ResourceEnv, ResourceLocator, UnresolvedImport,
};
use qbx_lua_analysis::scope::resolve;
use qbx_lua_analysis::Config;
use qbx_lua_syntax::{parse, SmolStr};
use rustc_hash::FxHashSet;

use crate::index::{FileEntry, FileId, FileOrigin, Index, ResourceEntry, ResourceId};
use crate::indexer::index_file;

const MAX_INDEXED_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Default)]
pub struct Workspace {
    pub index: Index,
    pub roots: Vec<PathBuf>,
    pub library: Vec<PathBuf>,
    pub lint_config: Config,
    /// Convars assigned with `set`, `setr` or `sets` in the workspace's .cfg files.
    pub cfg_convars: Vec<SmolStr>,
    locator: ResourceLocator,
}

fn cfg_convars(roots: &[PathBuf]) -> Vec<SmolStr> {
    let mut names: Vec<SmolStr> = Vec::new();
    for root in roots {
        let files = walkdir::WalkDir::new(root).max_depth(2).into_iter().flatten();
        for entry in files.filter(|e| e.path().extension().is_some_and(|ext| ext == "cfg")) {
            let Ok(text) = read_source(entry.path()) else { continue };
            for line in text.lines() {
                let mut words = line.split_whitespace();
                if let (Some("set" | "setr" | "sets"), Some(name)) = (words.next(), words.next()) {
                    let name = name.trim_matches(['"', '\'']);
                    if !name.is_empty() && !names.iter().any(|n| n == name) {
                        names.push(SmolStr::new(name));
                    }
                }
            }
        }
    }
    names
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub files: usize,
    pub resources: usize,
    pub millis: u128,
}

pub fn path_to_uri(path: &Path) -> Url {
    Url::from_file_path(path).unwrap_or_else(|_| Url::parse("file:///invalid").expect("static url"))
}

pub fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

impl Workspace {
    pub fn load_stubs(&mut self) {
        for stub in STUBS {
            let path = PathBuf::from(format!("/qbx-lua-ls/stubs/{}", stub.name));
            let id = self.index.allocate(&path);
            let chunk = parse(stub.source);
            let resolution = resolve(&chunk);
            let uri = Url::parse(&format!("qbx-stub:///{}", stub.name)).expect("static url");
            let side = (stub.side != Side::Shared).then_some(stub.side);
            let index = index_file(id, stub.source, &chunk, &resolution, &self.index, side);
            self.index.set_file(id, FileEntry { path, uri, origin: FileOrigin::Stub, resource: None, side, index });
        }
    }

    pub fn scan(&mut self) -> ScanStats {
        let started = std::time::Instant::now();
        let mut stats = ScanStats::default();
        self.index.clear_workspace();
        self.locator = ResourceLocator::default();
        self.lint_config =
            self.roots.first().and_then(|root| Config::discover(root).ok().flatten()).unwrap_or_default();
        let roots: Vec<(PathBuf, FileOrigin)> = self
            .roots
            .iter()
            .map(|r| (r.clone(), FileOrigin::Workspace))
            .chain(self.library.iter().map(|r| (r.clone(), FileOrigin::Library)))
            .collect();
        for (root, origin) in roots {
            for path in lua_files_under(&root, &self.lint_config) {
                if is_manifest_file(&path) {
                    if let Some(root) = path.parent() {
                        self.ensure_resource(root);
                    }
                } else if self.index_path(&path, origin, None) {
                    stats.files += 1;
                }
            }
        }
        stats.files += self.index_dependencies();
        self.link_imports();
        self.reindex_all();
        self.cfg_convars = cfg_convars(&self.roots);
        stats.resources = self.index.resources.len();
        stats.millis = started.elapsed().as_millis();
        stats
    }

    /// Symbol types are inferred while indexing and may refer to files that were not indexed yet
    /// (exports, imported globals), so a second pass settles them once every file is known.
    fn reindex_all(&mut self) {
        let files: Vec<(PathBuf, FileOrigin)> = self
            .index
            .files()
            .filter(|(_, f)| f.origin != FileOrigin::Stub)
            .map(|(_, f)| (f.path.clone(), f.origin))
            .collect();
        for (path, origin) in files {
            self.index_path(&path, origin, None);
        }
    }

    /// Indexes resources that workspace manifests refer to but that live outside the workspace,
    /// so opening a single resource folder still resolves `@ox_lib`, `@qbx_core` and friends.
    fn index_dependencies(&mut self) -> usize {
        let mut wanted: Vec<(PathBuf, SmolStr)> = Vec::new();
        for resource in &self.index.resources {
            let imports = resource.manifest.imports().filter_map(|s| split_import(&s.pattern)).map(|(name, _)| name);
            let dependencies = resource.manifest.dependencies.iter().map(|d| d.value.as_str());
            for name in imports.chain(dependencies) {
                wanted.push((resource.root.clone(), SmolStr::new(name.trim_start_matches('/'))));
            }
        }
        let mut seen = FxHashSet::default();
        let mut indexed = 0;
        for (from, name) in wanted {
            if !seen.insert(name.clone()) || self.index.resource_by_name(&name).is_some() {
                continue;
            }
            let Some(root) = self.locator.locate(&from, &name) else { continue };
            self.ensure_resource(&root);
            for path in lua_files_under(&root, &self.lint_config) {
                if !is_manifest_file(&path) && self.index_path(&path, FileOrigin::Library, None) {
                    indexed += 1;
                }
            }
        }
        indexed
    }

    fn ensure_resource(&mut self, root: &Path) -> Option<ResourceId> {
        if let Some(id) = self.index.resources.iter().position(|r| r.root == root) {
            return Some(id as ResourceId);
        }
        let manifest_path = manifest_path(root)?;
        let source = read_source(&manifest_path).ok()?;
        let manifest = Manifest::from_chunk(&parse(&source));
        let name = SmolStr::new(root.file_name()?.to_string_lossy());
        self.index.resources.push(ResourceEntry {
            name,
            root: root.to_path_buf(),
            manifest_path,
            manifest,
            files: Vec::new(),
            imports: Vec::new(),
            escrowed: qbx_lua_analysis::project::is_escrowed_resource(root),
        });
        Some(self.index.resources.len() as ResourceId - 1)
    }

    pub fn reload_manifest(&mut self, manifest_file: &Path) {
        let Some(root) = manifest_file.parent() else { return };
        let Some(id) = self.index.resources.iter().position(|r| r.root == root) else {
            self.ensure_resource(root);
            return;
        };
        let Ok(source) = read_source(manifest_file) else { return };
        self.index.resources[id].manifest = Manifest::from_chunk(&parse(&source));
        let files = self.index.resources[id].files.clone();
        for file in files {
            if let Some(path) = self.index.file(file).map(|f| f.path.clone()) {
                self.index_path(&path, FileOrigin::Workspace, None);
            }
        }
        self.link_imports();
    }

    pub fn side_and_resource(&mut self, path: &Path) -> (Option<ResourceId>, Option<Side>) {
        let Some(root) = find_manifest_dir(path) else { return (None, None) };
        let Some(id) = self.ensure_resource(&root) else { return (None, None) };
        let relative = relative_slash_path(&root, path);
        (Some(id), side_of(&self.index.resources[id as usize].manifest, &relative))
    }

    /// Indexes `path`, reading it from disk unless `text` (an open document) is given.
    pub fn index_path(&mut self, path: &Path, origin: FileOrigin, text: Option<&str>) -> bool {
        let owned;
        let source = match text {
            Some(text) => text,
            None => {
                let too_large = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_INDEXED_FILE_BYTES);
                if too_large {
                    return false;
                }
                match read_source(path) {
                    Ok(text) => {
                        owned = text;
                        &owned
                    }
                    Err(_) => {
                        self.mark_escrowed(path);
                        return false;
                    }
                }
            }
        };
        let chunk = parse(source);
        let resolution = resolve(&chunk);
        self.index_parsed(path, origin, source, &chunk, &resolution);
        true
    }

    fn mark_escrowed(&mut self, unreadable: &Path) {
        if !unreadable.is_file() {
            return;
        }
        let resource = find_manifest_dir(unreadable).and_then(|root| self.ensure_resource(&root));
        if let Some(id) = resource {
            self.index.resources[id as usize].escrowed = true;
        }
    }

    pub fn index_parsed(
        &mut self,
        path: &Path,
        origin: FileOrigin,
        source: &str,
        chunk: &qbx_lua_syntax::ast::Chunk,
        resolution: &qbx_lua_analysis::scope::Resolution,
    ) -> FileId {
        let id = self.index.allocate(path);
        let (resource, side) = self.side_and_resource(path);
        let origin = self.index.file(id).map_or(origin, |f| f.origin);
        let file_index = index_file(id, source, chunk, resolution, &self.index, side);
        let entry =
            FileEntry { path: path.to_path_buf(), uri: path_to_uri(path), origin, resource, side, index: file_index };
        self.index.set_file(id, entry);
        id
    }

    pub fn link_imports(&mut self) {
        for id in 0..self.index.resources.len() {
            let mut imports = Vec::new();
            for script in self.index.resources[id].manifest.imports() {
                let Some((resource, file)) = split_import(&script.pattern) else { continue };
                let Some((_, target)) = self.index.resource_by_name(resource) else { continue };
                if let Some(file_id) = self.index.file_id(&target.root.join(file)) {
                    imports.push((file_id, script.side));
                }
            }
            self.index.resources[id].imports = imports;
        }
    }

    /// The lint environment of a resource, assembled from the per-file summaries in the index.
    pub fn resource_env(&self, resource: ResourceId) -> ResourceEnv {
        let mut env = ResourceEnv::default();
        let Some(entry) = self.index.resource(resource) else { return env };
        env.opaque = entry.escrowed;
        for file in entry.files.iter().filter_map(|id| self.index.file(*id)) {
            env.add_summary(&file.index.summary, file.side);
        }
        for script in entry.manifest.imports().filter(|s| s.pattern.ends_with(".lua")) {
            let resolved = split_import(&script.pattern)
                .and_then(|(name, file)| Some(self.index.resource_by_name(name)?.1.root.join(file)))
                .and_then(|path| self.index.file(self.index.file_id(&path)?));
            if let Some(file) = resolved {
                env.add_summary(&file.index.summary, Some(script.side));
            }
            match known_import(&script.pattern) {
                Some(known) => known.globals.iter().for_each(|g| env.add_global(&SmolStr::new(g), Some(script.side))),
                None if resolved.is_none() => {
                    env.unresolved_imports.push(UnresolvedImport { path: script.pattern.clone(), side: script.side })
                }
                None => {}
            }
        }
        env
    }
}

impl Workspace {
    /// Event handlers and exports of every indexed file, in the shape the cross-file lint rules use.
    pub fn crossrefs(&self) -> qbx_lua_analysis::crossref::CrossRefs {
        use qbx_lua_analysis::crossref::{Arity, CrossRefs};

        use crate::index::EventKind;
        use crate::types::FunType;

        fn arity(fun: &FunType) -> Arity {
            let vararg = fun.params.last().is_some_and(|p| p.name == "...");
            Arity { params: fun.params.len() - usize::from(vararg), vararg }
        }

        let mut refs = CrossRefs::default();
        for resource in &self.index.resources {
            let hidden = resource.escrowed || resource.manifest.has_non_lua_scripts();
            if hidden {
                refs.opaque_resources.insert(resource.name.clone());
            }
        }
        for (_, file) in self.index.files() {
            let resource = file.resource.and_then(|id| self.index.resource(id)).map(|r| r.name.clone());
            if let (true, Some(name)) = (file.index.dynamic_exports, &resource) {
                refs.opaque_resources.insert(name.clone());
            }
            if let Some(name) = &resource {
                refs.resources.insert(name.clone());
            }
            for event in file.index.events.iter().filter(|e| matches!(e.kind, EventKind::NetEvent | EventKind::Handler))
            {
                refs.add_event(event.name.clone(), event.side, event.handler.as_deref().map(arity));
            }
            if let Some(resource) = resource {
                for export in &file.index.exports {
                    let known = export.ty.as_fun().map(|f| arity(f));
                    let arity = known.unwrap_or(Arity { params: 0, vararg: true });
                    refs.exports.insert((resource.clone(), export.name.clone()), arity);
                }
            }
        }
        refs
    }
}
