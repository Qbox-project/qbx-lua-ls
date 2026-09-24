use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{CompletionResponse, NumberOrString, Url};
use qbx_fivem_data::Side;
use qbx_lua_analysis::crossref::CrossRefs;
use qbx_lua_ls::document::Document;
use qbx_lua_ls::features::{completion, diagnostics, event_call};
use qbx_lua_ls::index::FileOrigin;
use qbx_lua_ls::types::FunType;
use qbx_lua_ls::workspace::{path_to_uri, Workspace};
use serde_json::{json, Value};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qbx-lua-ls-refresh-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn workspace(&self, root: &str) -> Workspace {
        let mut ws = Workspace::default();
        ws.roots.push(self.0.join(root));
        ws.load_stubs();
        ws.scan();
        ws
    }

    fn document(&self, ws: &mut Workspace, relative: &str, text: &str) -> Document {
        let path = self.0.join(relative);
        let mut doc = Document::new(path_to_uri(&path), path, 1, text.to_string());
        doc.file = ws.index_parsed(&doc.path, FileOrigin::Workspace, &doc.text, &doc.chunk, &doc.resolution);
        doc
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
            if root.parent() == Some(temp.as_path()) {
                let _ = std::fs::remove_dir_all(root);
            }
        }
    }
}

const GUARDED: &str = "if IsDuplicityVersion() then\n\
    RegisterNetEvent('demo:server', function(serverValue) end)\n\
    RegisterNetEvent('demo:both', function(serverValue) end)\n\
    lib.callback.register('demo:callback', function(source, serverValue) end)\n\
else\n\
    AddEventHandler('demo:client', function(clientValue) end)\n\
    AddEventHandler('demo:both', function(clientValue) end)\n\
    lib.callback.register('demo:callback', function(clientValue) end)\n\
end\n";

fn event_workspace() -> (Fixture, Workspace) {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "shared_script 'shared.lua'\nclient_script 'client.lua'\n");
    fixture.write("demo/shared.lua", GUARDED);
    fixture.write("demo/client.lua", "");
    let ws = fixture.workspace("demo");
    (fixture, ws)
}

#[test]
fn guarded_event_diagnostics_match_the_cli() {
    let (fixture, mut ws) = event_workspace();
    let refs = ws.crossrefs();
    let mut cli = CrossRefs::default();
    cli.collect(&qbx_lua_syntax::parse(GUARDED), Some(Side::Shared), Some("demo"));
    for (name, expected) in cli.events {
        let actual = &refs.events[&name];
        assert_eq!(actual.len(), expected.len(), "{name}");
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.side, expected.side, "{name}");
            assert_eq!(actual.handler, expected.handler, "{name}");
        }
    }
    let doc = fixture.document(&mut ws, "demo/client.lua", "TriggerEvent('demo:server', 1)\n");
    let found = diagnostics::diagnostics(&ws, &doc, &[], &ws.crossrefs());
    assert!(found.iter().any(|d| d.code == Some(NumberOrString::String("fivem/event-wrong-side".into()))));
}

fn labels(ws: &Workspace, doc: &Document) -> Vec<String> {
    let offset = doc.text.rfind("''").unwrap() + 1;
    let result = completion::completion(ws, doc, doc.position(offset as u32), true).unwrap();
    let items = match result {
        CompletionResponse::List(list) => list.items,
        CompletionResponse::Array(items) => items,
    };
    items.into_iter().map(|item| item.label).collect()
}

fn payload(ws: &Workspace, doc: &Document) -> Option<Vec<String>> {
    let offset = doc.text.rfind("1)").unwrap();
    let site = qbx_lua_ls::locate::locate(&doc.chunk, offset as u32).call.unwrap();
    let event = event_call::event_call(ws, doc, site.base, site.args, &FunType::default())?;
    Some(event.fun.params.into_iter().map(|p| p.name.to_string()).collect())
}

#[test]
fn guarded_event_completion_and_signatures_follow_both_sides() {
    let (fixture, mut ws) = event_workspace();
    for (guard, wanted, absent) in [
        ("IsDuplicityVersion()", "demo:server", "demo:client"),
        ("not IsDuplicityVersion()", "demo:client", "demo:server"),
    ] {
        let text = format!("if {guard} then\nTriggerEvent('')\nend");
        let doc = fixture.document(&mut ws, "demo/caller.lua", &text);
        let found = labels(&ws, &doc);
        assert!(found.iter().any(|name| name == wanted), "{found:?}");
        assert!(!found.iter().any(|name| name == absent), "{found:?}");
    }
    for (call, expected) in [("TriggerServerEvent", "serverValue"), ("TriggerEvent", "clientValue")] {
        let text = format!("{call}('demo:both', 1)");
        let doc = fixture.document(&mut ws, "demo/client.lua", &text);
        assert_eq!(payload(&ws, &doc).unwrap().last().unwrap(), expected);
    }
    // A client callback has no implicit player/source argument, even in a shared file.
    for (guard, expected) in [("IsDuplicityVersion()", "clientValue"), ("not IsDuplicityVersion()", "serverValue")] {
        let text = format!("if {guard} then\nlib.callback.await('demo:callback', false, 1)\nend");
        let doc = fixture.document(&mut ws, "demo/caller.lua", &text);
        let params = payload(&ws, &doc).unwrap();
        assert_eq!(params.len(), 3, "{params:?}");
        assert_eq!(params[2], expected);
    }
    let doc = fixture.document(&mut ws, "demo/client.lua", "TriggerEvent('demo:server', 1)");
    assert!(payload(&ws, &doc).is_none(), "an unreachable handler must not supply a signature");
}

#[test]
fn scan_reloads_manifests_and_removes_deleted_and_excluded_state() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "client_script '*.lua'");
    fixture.write("demo/main.lua", "Current = 1");
    fixture.write("demo/deleted.lua", "Deleted = 1");
    fixture.write("demo/skip/old.lua", "Excluded = 1");
    fixture.write("empty/fxmanifest.lua", "fx_version 'cerulean'");
    let mut ws = fixture.workspace("");
    assert!(ws.index.resource_by_name("empty").is_some());
    let main = fixture.0.join("demo/main.lua");
    assert_eq!(ws.index.file(ws.index.file_id(&main).unwrap()).unwrap().side, Some(Side::Client));
    fixture.write("demo/fxmanifest.lua", "server_script '*.lua'");
    fixture.write("qbxlint.toml", "exclude = ['demo/skip/**']");
    std::fs::remove_file(fixture.0.join("demo/deleted.lua")).unwrap();
    std::fs::remove_file(fixture.0.join("empty/fxmanifest.lua")).unwrap();
    ws.scan();
    assert_eq!(ws.index.file(ws.index.file_id(&main).unwrap()).unwrap().side, Some(Side::Server));
    assert!(ws.index.file_id(&fixture.0.join("demo/deleted.lua")).is_none());
    assert!(ws.index.file_id(&fixture.0.join("demo/skip/old.lua")).is_none());
    assert!(ws.index.resource_by_name("empty").is_none());
    std::fs::remove_file(fixture.0.join("demo/fxmanifest.lua")).unwrap();
    ws.scan();
    assert!(ws.index.resources.is_empty());
    let entry = ws.index.file(ws.index.file_id(&main).unwrap()).unwrap();
    assert!(entry.resource.is_none());
    assert!(entry.side.is_none());
}

#[test]
fn scan_rediscovers_missing_external_dependencies_and_prunes_removed_libraries() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "shared_script '@external/init.lua'");
    fixture.write("demo/main.lua", "print(External)");
    let mut ws = fixture.workspace("demo");
    assert!(ws.index.resource_by_name("external").is_none());
    fixture.write("external/fxmanifest.lua", "shared_script 'init.lua'");
    fixture.write("external/init.lua", "External = true");
    ws.scan();
    let (_, resource) = ws.index.resource_by_name("demo").unwrap();
    assert_eq!(resource.imports.len(), 1);
    assert!(ws.index.resource_by_name("external").is_some());
    fixture.write("library/fxmanifest.lua", "shared_script 'library.lua'");
    fixture.write("library/library.lua", "Library = true");
    ws.library.push(fixture.0.join("library"));
    ws.scan();
    assert!(ws.index.resource_by_name("library").is_some());
    ws.library.clear();
    fixture.write("demo/fxmanifest.lua", "shared_script 'main.lua'");
    ws.scan();
    assert!(ws.index.resource_by_name("external").is_none());
    assert!(ws.index.resource_by_name("library").is_none());
}

struct Client {
    connection: Connection,
    server: Option<JoinHandle<()>>,
    next: i32,
}

impl Client {
    fn start(root: &Path) -> Self {
        let (server, connection) = Connection::memory();
        let server = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                qbx_lua_ls::server::run_connection(server).unwrap();
            })
            .unwrap();
        let mut client = Self { connection, server: Some(server), next: 0 };
        client.request("initialize", json!({"processId": null, "rootUri": path_to_uri(root), "capabilities": {}}));
        client.notify("initialized", json!({}));
        client
    }

    fn notify(&self, method: &str, params: Value) {
        self.connection.sender.send(Message::Notification(Notification::new(method.to_string(), params))).unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = RequestId::from(self.next);
        self.connection.sender.send(Message::Request(Request::new(id.clone(), method.to_string(), params))).unwrap();
        loop {
            match self.connection.receiver.recv_timeout(Duration::from_secs(20)).expect("server did not respond") {
                Message::Response(response) if response.id == id => {
                    assert!(response.error.is_none(), "{:?}", response.error);
                    return response.result.unwrap_or(Value::Null);
                }
                Message::Request(request) => {
                    self.connection.sender.send(Message::Response(Response::new_ok(request.id, Value::Null))).unwrap();
                }
                _ => {}
            }
        }
    }

    fn open(&self, uri: &Url, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "languageId": "lua", "version": 1, "text": text
            }}),
        );
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        self.server.take().unwrap().join().unwrap();
    }
}

#[test]
fn manual_reindex_restores_unsaved_documents_and_their_new_file_ids() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "client_script '*.lua'");
    fixture.write("demo/a_deleted.lua", "Old = true");
    fixture.write("demo/main.lua", "DiskOnly = 1");
    let mut client = Client::start(&fixture.0);
    let main_uri = path_to_uri(&fixture.0.join("demo/main.lua"));
    let scratch_uri = path_to_uri(&fixture.0.join("demo/scratch.lua"));
    let manifest_uri = path_to_uri(&fixture.0.join("demo/fxmanifest.lua"));
    client.open(&main_uri, "UnsavedOnly = 42\nprint(UnsavedOnly)");
    client.open(&scratch_uri, "ScratchOnly = true");
    client.open(&manifest_uri, "client_script '*.lua'");
    client.request("qbx/status", Value::Null);
    std::fs::remove_file(fixture.0.join("demo/a_deleted.lua")).unwrap();
    fixture.write("demo/fxmanifest.lua", "server_script '*.lua'");
    client.request("qbx/reindex", Value::Null);
    assert_eq!(client.request("qbx/fileInfo", json!({"uri": main_uri}))["side"], "server");
    assert_eq!(client.request("qbx/fileInfo", json!({"uri": scratch_uri}))["side"], "server");
    for name in ["UnsavedOnly", "ScratchOnly"] {
        let symbols = client.request("workspace/symbol", json!({"query": name}));
        assert!(symbols.as_array().unwrap().iter().any(|s| s["name"] == name), "{symbols}");
    }
    for name in ["DiskOnly", "Old"] {
        let symbols = client.request("workspace/symbol", json!({"query": name}));
        assert!(symbols.as_array().unwrap().is_empty(), "{symbols}");
    }
    let hover = client.request(
        "textDocument/hover",
        json!({
            "textDocument": {"uri": main_uri}, "position": {"line": 1, "character": 9}
        }),
    );
    assert!(hover["contents"]["value"].as_str().unwrap().contains("42"), "{hover}");
    client.notify("textDocument/didClose", json!({"textDocument": {"uri": scratch_uri}}));
    client.request("qbx/reindex", Value::Null);
    let symbols = client.request("workspace/symbol", json!({"query": "ScratchOnly"}));
    assert!(symbols.as_array().unwrap().is_empty(), "{symbols}");
}
