use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::Url;
use qbx_fivem_data::Side;
use qbx_lua_analysis::{manifest::Manifest, scope::resolve};
use qbx_lua_ls::features::nui_resource::resource;
use qbx_lua_ls::features::resource_details::DetailsParams;
use qbx_lua_ls::index::{FileEntry, FileOrigin, Index, ResourceEntry};
use qbx_lua_ls::indexer::index_file;
use qbx_lua_syntax::parse;
use serde_json::{json, Value};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qbx-nui-resource-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn manifest(&self, relative: &str, text: &str) {
        self.write(&format!("{relative}/fxmanifest.lua"), &format!("fx_version 'cerulean'\ngame 'gta5'\n{text}"));
    }

    fn uri(&self, relative: &str) -> Url {
        Url::from_file_path(self.0.join(relative)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.0.starts_with(std::env::temp_dir()));
        assert!(self.0.file_name().unwrap().to_string_lossy().starts_with("qbx-nui-resource-"));
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

struct Client {
    connection: Connection,
    server: Option<JoinHandle<()>>,
    next: i32,
}

impl Client {
    fn start(root: &Path) -> Self {
        let (server_connection, connection) = Connection::memory();
        let server = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || qbx_lua_ls::server::run_connection(server_connection).unwrap())
            .unwrap();
        let mut client = Self { connection, server: Some(server), next: 0 };
        let uri = Url::from_file_path(root).unwrap();
        client.request(
            "initialize",
            json!({"processId":null,"rootUri":uri,"capabilities":{},
            "initializationOptions":{"diagnostics":{"enable":false,"workspace":false}},
            "workspaceFolders":[{"uri":uri,"name":"NUI fixture"}]}),
        );
        client.notify("initialized", json!({}));
        client
    }

    fn notify(&self, method: &str, params: Value) {
        self.connection.sender.send(Message::Notification(Notification { method: method.into(), params })).unwrap();
    }

    fn response(&mut self, method: &str, params: Value) -> Response {
        self.next += 1;
        let id = RequestId::from(self.next);
        self.connection
            .sender
            .send(Message::Request(Request { id: id.clone(), method: method.into(), params }))
            .unwrap();
        loop {
            match self.connection.receiver.recv_timeout(Duration::from_secs(20)).expect("server did not reply") {
                Message::Response(response) if response.id == id => return response,
                Message::Request(request) => self
                    .connection
                    .sender
                    .send(Message::Response(Response { id: request.id, result: Some(Value::Null), error: None }))
                    .unwrap(),
                _ => {}
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let response = self.response(method, params);
        assert!(response.error.is_none(), "{:?}", response.error);
        response.result.unwrap_or(Value::Null)
    }

    fn nui(&mut self, uri: Url) -> Value {
        self.request("qbx/nuiResource", json!({"uri":uri}))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        self.server.take().unwrap().join().unwrap();
    }
}

fn names(value: &Value) -> Vec<&str> {
    value["callbacks"].as_array().unwrap().iter().map(|item| item["name"].as_str().unwrap()).collect()
}

#[test]
fn protocol_lists_literal_callbacks_with_utf16_ranges_and_client_sides() {
    let fixture = Fixture::new();
    fixture.manifest("app", "ui_page 'web/index.html'\nclient_script 'client.lua'\nshared_script 'shared.lua'\nserver_script 'server.lua'\nshared_script '@lib/shared.lua'\n");
    let source = "local marker = '😀'; RegisterNUICallback('get☃', function(data, cb) cb({}) end)\nRegisterNuiCallback('modern', function() end)\nRegisterNUICallback('duplicate', function() end)\nRegisterNUICallback('duplicate', function() end)\nRegisterNUICallback(dynamic, function() end)\nRegisterNUICallback('missingHandler')\nRegisterNetEvent('ordinary')\n";
    fixture.write("app/client.lua", source);
    fixture.write("app/shared.lua", "RegisterNUICallback('shared', function() end)\nif IsDuplicityVersion() then\n RegisterNUICallback('wrongSharedSide', function() end)\nelse\n RegisterNUICallback('guardedClient', function() end)\nend\n");
    fixture.write("app/server.lua", "RegisterNUICallback = function() end\nRegisterNUICallback('wrongServerSide', function() end)\nif not IsDuplicityVersion() then RegisterNUICallback('unreachableClient', function() end) end\n");
    fixture.write("app/module.lua", "RegisterNUICallback('unknownSide', function() end)\n");
    fixture.manifest("lib", "shared_script 'shared.lua'\n");
    fixture.write("lib/shared.lua", "RegisterNUICallback('foreign', function() end)\n");
    let mut client = Client::start(&fixture.0);
    let status = client.request("qbx/status", Value::Null);
    let result = client.nui(fixture.uri("app"));
    assert_eq!(result, client.nui(fixture.uri("app/fxmanifest.lua")));
    assert_eq!(result["uiPage"], "web/index.html");
    assert_eq!(result["resource"]["uri"], json!(fixture.uri("app")));
    assert_eq!(names(&result), ["duplicate", "duplicate", "get☃", "guardedClient", "modern", "shared"]);
    let found = result["callbacks"].as_array().unwrap().iter().find(|item| item["name"] == "get☃").unwrap();
    let offset = source.find("'get☃'").unwrap();
    let start = source[..offset].encode_utf16().count();
    assert_eq!(found["location"]["uri"], json!(fixture.uri("app/client.lua")));
    assert_eq!(
        found["location"]["range"],
        json!({"start":{"line":0,"character":start},"end":{"line":0,"character":start+6}})
    );
    assert_eq!(result["truncated"], 0);
    let details = client.request("qbx/resourceDetails", json!({"uri":fixture.uri("app")}));
    assert_eq!(details["counts"]["events"], 1, "NUI callbacks must not become network events");
    assert_eq!(status, client.request("qbx/status", Value::Null));
    fixture.write("app/new.lua", "RegisterNUICallback('notYetIndexed', function() end)\n");
    assert_eq!(result, client.nui(fixture.uri("app")), "requests must not scan or parse new files");
}

#[test]
fn rejects_shadowed_redefined_qualified_and_dynamic_registrations() {
    let rejected = [
        "local RegisterNUICallback = function() end; RegisterNUICallback('x', function() end)",
        "local function RegisterNUICallback() end; RegisterNUICallback('x', function() end)",
        "local function run(RegisterNUICallback) RegisterNUICallback('x', function() end) end",
        "local _ENV = {}; RegisterNUICallback('x', function() end)",
        "_ENV = {}; RegisterNUICallback('x', function() end)",
        "RegisterNUICallback = function() end; RegisterNUICallback('x', function() end)",
        "RegisterNUICallback('x', function() end); RegisterNUICallback = function() end",
        "_G.RegisterNUICallback = function() end; RegisterNUICallback('x', function() end)",
        "_ENV['RegisterNUICallback'] = function() end; RegisterNUICallback('x', function() end)",
        "function _G.RegisterNUICallback() end; RegisterNUICallback('x', function() end)",
        "_G[key] = function() end; RegisterNUICallback('x', function() end)",
        "object.RegisterNUICallback('x', function() end)",
        "object:RegisterNUICallback('x', function() end)",
        "local callback = RegisterNUICallback; callback('x', function() end)",
        "RegisterNUICallback('x' .. 'y', function() end)",
        "RegisterNUICallback('', function() end)",
        "local RegisterNuiCallback = function() end; RegisterNuiCallback('x', function() end)",
    ];
    for source in rejected {
        let chunk = parse(source);
        assert!(chunk.errors.is_empty(), "{source}");
        let resolution = resolve(&chunk);
        let file = index_file(0, source, &chunk, &resolution, &Index::default(), Some(Side::Client));
        assert!(file.nui_callbacks.is_empty(), "{source}");
    }
    let source = "do local RegisterNUICallback = function() end end\nRegisterNUICallback('outside', function() end)";
    let chunk = parse(source);
    let file = index_file(0, source, &chunk, &resolve(&chunk), &Index::default(), Some(Side::Client));
    assert_eq!(file.nui_callbacks[0].name, "outside");
}

#[test]
fn unsaved_callback_and_other_file_global_changes_refresh_without_rescanning() {
    let fixture = Fixture::new();
    fixture.manifest("app", "client_scripts {'client.lua','override.lua'}\nui_page 'web/index.html'\n");
    fixture.write(
        "app/client.lua",
        "RegisterNUICallback('old', function() end)\nRegisterNuiCallback('modern', function() end)\n",
    );
    fixture.write("app/override.lua", "-- empty\n");
    let mut client = Client::start(&fixture.0);
    let target = fixture.uri("app");
    let uri = fixture.uri("app/override.lua");
    client.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"lua","version":1,"text":"_G.RegisterNUICallback = function() end\n"}}));
    assert_eq!(names(&client.nui(target.clone())), ["modern"]);
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"_ENV = {}\n"}]}),
    );
    assert!(names(&client.nui(target.clone())).is_empty());
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri,"version":3},"contentChanges":[{"text":"-- restored\n"}]}),
    );
    assert_eq!(names(&client.nui(target.clone())), ["modern", "old"]);
    let client_uri = fixture.uri("app/client.lua");
    client.notify("textDocument/didOpen", json!({"textDocument":{"uri":client_uri,"languageId":"lua","version":1,"text":"RegisterNUICallback('unsaved', function() end)\n"}}));
    assert_eq!(names(&client.nui(target.clone())), ["unsaved"]);
    client.notify("textDocument/didChange", json!({"textDocument":{"uri":client_uri,"version":2},"contentChanges":[{"text":"local RegisterNUICallback = function() end\nRegisterNUICallback('shadowed', function() end)"}]}));
    assert!(names(&client.nui(target.clone())).is_empty());
    client.notify("textDocument/didClose", json!({"textDocument":{"uri":client_uri}}));
    assert_eq!(names(&client.nui(target)), ["modern", "old"]);
}

#[test]
fn literal_page_metadata_preserves_remote_paths_and_updates_only_after_save() {
    let fixture = Fixture::new();
    fixture.manifest("app", "ui_page 'web/index.html'\n");
    let mut client = Client::start(&fixture.0);
    let uri = fixture.uri("app/fxmanifest.lua");
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri,"languageId":"lua","version":1,"text":"ui_page 'unsaved.html'\n"}}),
    );
    assert_eq!(client.nui(fixture.uri("app"))["uiPage"], "web/index.html");
    for (manifest, expected) in [
        ("ui_page 'https://example.invalid/ui?view=full#app'", json!("https://example.invalid/ui?view=full#app")),
        ("ui_page 'nui://another-resource/index.html'", json!("nui://another-resource/index.html")),
        ("ui_page prefix .. '/index.html'", Value::Null),
        ("-- no ui_page", Value::Null),
    ] {
        fixture.manifest("app", manifest);
        client.notify("textDocument/didSave", json!({"textDocument":{"uri":uri}}));
        assert_eq!(client.nui(fixture.uri("app"))["uiPage"], expected);
    }
}

#[test]
fn rejects_invalid_uris_and_selects_duplicate_names_by_folder_identity() {
    let fixture = Fixture::new();
    fixture.manifest("[a]/duplicate", "ui_page 'a.html'");
    fixture.manifest("[b]/duplicate", "ui_page 'b.html'");
    fixture.write("[a]/duplicate/client.lua", "return {}\n");
    let mut client = Client::start(&fixture.0);
    assert_eq!(client.nui(fixture.uri("[a]/duplicate"))["uiPage"], "a.html");
    assert_eq!(client.nui(fixture.uri("[b]/duplicate/fxmanifest.lua"))["uiPage"], "b.html");
    for params in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"uri":7}),
        json!({"uri":"bad"}),
        json!({"uri":"https://example.invalid/app"}),
        json!({"uri":fixture.uri("unknown")}),
        json!({"uri":fixture.uri("[a]/duplicate/client.lua")}),
        json!({"uri":format!("{}#fragment",fixture.uri("[a]/duplicate"))}),
    ] {
        assert_eq!(client.response("qbx/nuiResource", params).error.unwrap().code, -32602);
    }
}

#[test]
fn caps_callback_rows_and_omits_oversized_action_values_without_modifying_them() {
    let fixture = Fixture::new();
    let root = fixture.0.join("app");
    let mut index = Index::default();
    let page = "x".repeat(16_385);
    index.resources.push(ResourceEntry {
        name: "app".into(),
        root: root.clone(),
        manifest_path: root.join("fxmanifest.lua"),
        manifest: Manifest::from_chunk(&parse(&format!("ui_page '{page}'"))),
        files: Vec::new(),
        imports: Vec::new(),
        escrowed: false,
    });
    let source = (0..503).map(|i| format!("RegisterNUICallback('cb{i:03}', function() end)\n")).collect::<String>()
        + &format!("RegisterNUICallback('{}', function() end)\n", "α".repeat(2049));
    let chunk = parse(&source);
    let path = root.join("client.lua");
    let id = index.allocate(&path);
    let callbacks = index_file(id, &source, &chunk, &resolve(&chunk), &index, Some(Side::Client));
    index.set_file(
        id,
        FileEntry {
            uri: Url::from_file_path(&path).unwrap(),
            path,
            origin: FileOrigin::Workspace,
            resource: Some(0),
            side: Some(Side::Client),
            index: callbacks,
        },
    );
    let result = resource(&index, DetailsParams { uri: fixture.uri("app") }).unwrap();
    assert_eq!(result.callbacks.len(), 500);
    assert_eq!(result.truncated, 4);
    assert_eq!(result.callbacks[0].name, "cb000");
    assert_eq!(result.callbacks[499].name, "cb499");
    assert!(result.ui_page.is_none());
    assert!(result.notes.iter().any(|note| note.contains("16,384 bytes")));
    assert!(result.notes.iter().any(|note| note.contains("1 callback names longer")));
}
