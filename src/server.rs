use std::error::Error;
use std::path::PathBuf;

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{self as notif, Notification as _};
use lsp_types::request::{self as req, Request as _};
use lsp_types::*;
use qbx_lua_analysis::project::is_manifest_file;
use qbx_lua_analysis::Level;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::document::Document;
use crate::features::{
    code_action, completion, definition, diagnostics, folding, hover, inlay, references, semantic_tokens, signature,
    symbols,
};
use crate::index::FileOrigin;
use crate::workspace::{uri_to_path, Workspace};

pub type Documents = FxHashMap<Url, Document>;

type AnyResult<T> = Result<T, Box<dyn Error + Sync + Send>>;

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct DiagnosticSettings {
    pub enable: Option<bool>,
    pub rules: FxHashMap<String, String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct ToggleSettings {
    pub enable: Option<bool>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub library: Vec<String>,
    pub diagnostics: DiagnosticSettings,
    pub inlay_hints: ToggleSettings,
    pub semantic_tokens: ToggleSettings,
}

impl Settings {
    fn rule_overrides(&self) -> Vec<(String, Level)> {
        self.diagnostics
            .rules
            .iter()
            .filter(|(code, _)| qbx_lua_analysis::rules::find(code).is_some())
            .filter_map(|(code, level)| {
                let level = match level.as_str() {
                    "off" => Level::Off,
                    "hint" => Level::Hint,
                    "info" => Level::Info,
                    "warning" | "warn" => Level::Warning,
                    "error" => Level::Error,
                    _ => return None,
                };
                Some((code.clone(), level))
            })
            .collect()
    }
}

pub struct Server {
    connection: Connection,
    ws: Workspace,
    docs: Documents,
    settings: Settings,
    dirty: FxHashSet<Url>,
    next_request_id: i32,
}

pub fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::INCREMENTAL),
            save: Some(TextDocumentSyncSaveOptions::Supported(true)),
            ..TextDocumentSyncOptions::default()
        })),
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(true),
            trigger_characters: Some(vec![".".into(), ":".into(), "'".into(), "\"".into(), "@".into(), "{".into()]),
            ..CompletionOptions::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".into(), ",".into()]),
            retrigger_characters: None,
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        inlay_hint_provider: Some(OneOf::Left(true)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: semantic_tokens::legend(),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: Some(false),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            },
        )),
        ..ServerCapabilities::default()
    }
}

pub fn run() -> AnyResult<()> {
    let (connection, io_threads) = Connection::stdio();
    run_connection(connection)?;
    io_threads.join()?;
    Ok(())
}

pub fn run_connection(connection: Connection) -> AnyResult<()> {
    let (id, params) = connection.initialize_start()?;
    let params: InitializeParams = serde_json::from_value(params)?;
    let result = json!({
        "capabilities": capabilities(),
        "serverInfo": { "name": "qbx-lua-ls", "version": env!("CARGO_PKG_VERSION") },
    });
    connection.initialize_finish(id, result)?;

    let mut server = Server::new(connection, params);
    server.start();
    server.main_loop()
}

#[allow(deprecated)]
fn workspace_roots(params: &InitializeParams) -> Vec<PathBuf> {
    let folders = params.workspace_folders.iter().flatten().filter_map(|f| uri_to_path(&f.uri));
    let mut roots: Vec<PathBuf> = folders.collect();
    if roots.is_empty() {
        roots.extend(params.root_uri.as_ref().and_then(uri_to_path));
    }
    roots
}

impl Server {
    pub fn new(connection: Connection, params: InitializeParams) -> Self {
        let settings: Settings =
            params.initialization_options.clone().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        let mut ws = Workspace::default();
        ws.roots = workspace_roots(&params);
        ws.library = settings.library.iter().map(PathBuf::from).collect();
        Self { connection, ws, docs: Documents::default(), settings, dirty: FxHashSet::default(), next_request_id: 0 }
    }

    fn start(&mut self) {
        self.ws.load_stubs();
        let stats = self.ws.scan();
        self.log(format!(
            "indexed {} files in {} resources in {} ms ({} natives available)",
            stats.files,
            stats.resources,
            stats.millis,
            qbx_fivem_data::native_count()
        ));
        self.register_watchers();
    }

    fn log(&self, message: String) {
        self.notify::<notif::LogMessage>(LogMessageParams { typ: MessageType::INFO, message });
    }

    fn notify<N: notif::Notification>(&self, params: N::Params) {
        let _ = self.connection.sender.send(Message::Notification(Notification::new(N::METHOD.to_string(), params)));
    }

    fn register_watchers(&mut self) {
        let watchers = ["**/*.lua", "**/qbxlint.toml", "**/.qbxlint.toml"]
            .iter()
            .map(|glob| FileSystemWatcher { glob_pattern: GlobPattern::String(glob.to_string()), kind: None })
            .collect();
        let registration = Registration {
            id: "qbx-watch-lua".into(),
            method: notif::DidChangeWatchedFiles::METHOD.into(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers }).ok(),
        };
        self.next_request_id += 1;
        let request = Request::new(
            RequestId::from(self.next_request_id),
            req::RegisterCapability::METHOD.to_string(),
            RegistrationParams { registrations: vec![registration] },
        );
        let _ = self.connection.sender.send(Message::Request(request));
    }

    fn main_loop(&mut self) -> AnyResult<()> {
        while let Ok(message) = self.connection.receiver.recv() {
            match message {
                Message::Request(request) => {
                    if self.connection.handle_shutdown(&request)? {
                        return Ok(());
                    }
                    let id = request.id.clone();
                    let response = self.isolated(|server| {
                        server.flush_index();
                        server.handle_request(request)
                    });
                    let response = response.unwrap_or_else(|| {
                        Response::new_err(id, ErrorCode::InternalError as i32, "internal error".to_string())
                    });
                    self.connection.sender.send(Message::Response(response))?;
                }
                Message::Notification(notification) => {
                    self.isolated(|server| server.handle_notification(notification));
                }
                Message::Response(_) => {}
            }
            if self.connection.receiver.is_empty() {
                self.isolated(Self::publish_dirty);
            }
        }
        Ok(())
    }

    /// A bug in one request must not take the editor's language server down with it.
    fn isolated<T>(&mut self, work: impl FnOnce(&mut Self) -> T) -> Option<T> {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(self)));
        if outcome.is_err() {
            self.dirty.clear();
            self.log("recovered from an internal error; please report it with the file that triggered it".to_string());
        }
        outcome.ok()
    }

    /// Open documents are reparsed on every edit but only reindexed once something needs the index.
    fn flush_index(&mut self) {
        for uri in &self.dirty {
            if let Some(doc) = self.docs.get_mut(uri) {
                if !doc.is_manifest() {
                    doc.file =
                        self.ws.index_parsed(&doc.path, FileOrigin::Workspace, &doc.text, &doc.chunk, &doc.resolution);
                }
            }
        }
    }

    fn publish_dirty(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        self.flush_index();
        self.dirty.clear();
        let uris: Vec<Url> = self.docs.keys().cloned().collect();
        for uri in uris {
            self.publish(&uri);
        }
    }

    fn publish(&self, uri: &Url) {
        let Some(doc) = self.docs.get(uri) else { return };
        let enabled = self.settings.diagnostics.enable.unwrap_or(true);
        let diagnostics =
            if enabled { diagnostics::diagnostics(&self.ws, doc, &self.settings.rule_overrides()) } else { Vec::new() };
        self.notify::<notif::PublishDiagnostics>(PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics,
            version: Some(doc.version),
        });
    }

    fn handle_notification(&mut self, notification: Notification) {
        let Notification { method, params } = notification;
        match method.as_str() {
            notif::DidOpenTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<DidOpenTextDocumentParams>(params) else { return };
                let item = params.text_document;
                let Some(path) = uri_to_path(&item.uri) else { return };
                let mut doc = Document::new(item.uri.clone(), path, item.version, item.text);
                if doc.is_manifest() {
                    self.ws.side_and_resource(&doc.path);
                    doc.file = self.ws.index.allocate(&doc.path);
                }
                self.dirty.insert(item.uri.clone());
                self.docs.insert(item.uri, doc);
            }
            notif::DidChangeTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<DidChangeTextDocumentParams>(params) else { return };
                let uri = params.text_document.uri;
                if let Some(doc) = self.docs.get_mut(&uri) {
                    doc.apply_changes(params.text_document.version, params.content_changes);
                    self.dirty.insert(uri);
                }
            }
            notif::DidSaveTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<DidSaveTextDocumentParams>(params) else { return };
                let uri = params.text_document.uri;
                if let Some(path) = uri_to_path(&uri).filter(|p| is_manifest_file(p)) {
                    self.ws.reload_manifest(&path);
                }
                self.dirty.insert(uri);
            }
            notif::DidCloseTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<DidCloseTextDocumentParams>(params) else { return };
                let uri = params.text_document.uri;
                if let Some(doc) = self.docs.remove(&uri) {
                    if !doc.is_manifest() {
                        self.ws.index_path(&doc.path, FileOrigin::Workspace, None);
                    }
                }
                self.dirty.remove(&uri);
                self.notify::<notif::PublishDiagnostics>(PublishDiagnosticsParams {
                    uri,
                    diagnostics: Vec::new(),
                    version: None,
                });
            }
            notif::DidChangeWatchedFiles::METHOD => {
                let Ok(params) = serde_json::from_value::<DidChangeWatchedFilesParams>(params) else { return };
                self.watched_files_changed(params.changes);
            }
            notif::DidChangeConfiguration::METHOD => {
                let Ok(params) = serde_json::from_value::<DidChangeConfigurationParams>(params) else { return };
                let section = params.settings.get("qbxLua").cloned().unwrap_or(params.settings);
                if let Ok(settings) = serde_json::from_value::<Settings>(section) {
                    self.settings = settings;
                    self.dirty.extend(self.docs.keys().cloned());
                }
            }
            _ => {}
        }
    }

    fn watched_files_changed(&mut self, changes: Vec<FileEvent>) {
        let mut manifests_changed = false;
        for change in changes {
            let Some(path) = uri_to_path(&change.uri) else { continue };
            if path.extension().is_some_and(|e| e == "toml") {
                if let Some(root) = self.ws.roots.first() {
                    self.ws.lint_config = qbx_lua_analysis::Config::discover(root).ok().flatten().unwrap_or_default();
                }
            } else if is_manifest_file(&path) {
                self.ws.reload_manifest(&path);
                manifests_changed = true;
            } else if change.typ == FileChangeType::DELETED {
                self.ws.index.remove_file(&path);
            } else if !self.docs.contains_key(&change.uri) {
                self.ws.index_path(&path, FileOrigin::Workspace, None);
            }
        }
        if manifests_changed {
            self.ws.link_imports();
        }
        self.dirty.extend(self.docs.keys().cloned());
    }

    fn handle_request(&mut self, request: Request) -> Response {
        let id = request.id.clone();
        match self.dispatch(request) {
            Ok(value) => Response { id, result: Some(value), error: None },
            Err(message) => Response::new_err(id, ErrorCode::InvalidParams as i32, message),
        }
    }

    fn doc(&self, uri: &Url) -> Result<&Document, String> {
        self.docs.get(uri).ok_or_else(|| format!("document is not open: {uri}"))
    }

    fn dispatch(&mut self, request: Request) -> Result<Value, String> {
        fn params<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
            serde_json::from_value(value).map_err(|e| e.to_string())
        }
        fn reply<T: serde::Serialize>(value: T) -> Result<Value, String> {
            serde_json::to_value(value).map_err(|e| e.to_string())
        }

        let Request { method, params: raw, .. } = request;
        match method.as_str() {
            req::Completion::METHOD => {
                let p: CompletionParams = params(raw)?;
                let doc = self.doc(&p.text_document_position.text_document.uri)?;
                reply(completion::completion(&self.ws, doc, p.text_document_position.position))
            }
            req::ResolveCompletionItem::METHOD => reply(completion::resolve(params(raw)?)),
            req::HoverRequest::METHOD => {
                let p: HoverParams = params(raw)?;
                let doc = self.doc(&p.text_document_position_params.text_document.uri)?;
                reply(hover::hover(&self.ws, doc, p.text_document_position_params.position))
            }
            req::SignatureHelpRequest::METHOD => {
                let p: SignatureHelpParams = params(raw)?;
                let doc = self.doc(&p.text_document_position_params.text_document.uri)?;
                reply(signature::signature_help(&self.ws, doc, p.text_document_position_params.position))
            }
            req::GotoDefinition::METHOD => {
                let p: GotoDefinitionParams = params(raw)?;
                let doc = self.doc(&p.text_document_position_params.text_document.uri)?;
                reply(definition::definition(&self.ws, doc, p.text_document_position_params.position))
            }
            req::References::METHOD => {
                let p: ReferenceParams = params(raw)?;
                let doc = self.doc(&p.text_document_position.text_document.uri)?;
                reply(references::references(
                    &self.ws,
                    &self.docs,
                    doc,
                    p.text_document_position.position,
                    p.context.include_declaration,
                ))
            }
            req::DocumentHighlightRequest::METHOD => {
                let p: DocumentHighlightParams = params(raw)?;
                let doc = self.doc(&p.text_document_position_params.text_document.uri)?;
                reply(references::highlights(doc, p.text_document_position_params.position))
            }
            req::PrepareRenameRequest::METHOD => {
                let p: TextDocumentPositionParams = params(raw)?;
                reply(references::prepare_rename(self.doc(&p.text_document.uri)?, p.position))
            }
            req::Rename::METHOD => {
                let p: RenameParams = params(raw)?;
                let doc = self.doc(&p.text_document_position.text_document.uri)?;
                reply(references::rename(&self.ws, &self.docs, doc, p.text_document_position.position, &p.new_name))
            }
            req::DocumentSymbolRequest::METHOD => {
                let p: DocumentSymbolParams = params(raw)?;
                reply(DocumentSymbolResponse::Nested(symbols::document_symbols(self.doc(&p.text_document.uri)?)))
            }
            req::WorkspaceSymbolRequest::METHOD => {
                let p: WorkspaceSymbolParams = params(raw)?;
                reply(symbols::workspace_symbols(&self.ws, &p.query))
            }
            req::CodeActionRequest::METHOD => {
                let p: CodeActionParams = params(raw)?;
                reply(code_action::code_actions(self.doc(&p.text_document.uri)?, &p.context.diagnostics))
            }
            req::FoldingRangeRequest::METHOD => {
                let p: FoldingRangeParams = params(raw)?;
                reply(folding::folding_ranges(self.doc(&p.text_document.uri)?))
            }
            req::InlayHintRequest::METHOD => {
                let p: InlayHintParams = params(raw)?;
                if self.settings.inlay_hints.enable == Some(false) {
                    return reply(Vec::<InlayHint>::new());
                }
                reply(inlay::inlay_hints(&self.ws, self.doc(&p.text_document.uri)?, p.range))
            }
            req::SemanticTokensFullRequest::METHOD => {
                let p: SemanticTokensParams = params(raw)?;
                if self.settings.semantic_tokens.enable == Some(false) {
                    return reply(SemanticTokens::default());
                }
                reply(semantic_tokens::semantic_tokens(&self.ws, self.doc(&p.text_document.uri)?))
            }
            "qbx/status" => Ok(json!({
                "files": self.ws.index.file_count(),
                "resources": self.ws.index.resources.len(),
                "openDocuments": self.docs.len(),
                "natives": qbx_fivem_data::native_count(),
            })),
            "qbx/reindex" => {
                let stats = self.ws.scan();
                self.dirty.extend(self.docs.keys().cloned());
                Ok(json!({ "files": stats.files, "resources": stats.resources, "millis": stats.millis as u64 }))
            }
            other => Err(format!("unsupported request: {other}")),
        }
    }
}
