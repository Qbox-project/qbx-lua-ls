use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{Position, Range, Url};
use qbx_lua_analysis::manifest::Manifest;
use qbx_lua_ls::features::resource_details::{details, DetailsParams};
use qbx_lua_ls::index::{
    EventDef, EventFamily, EventKind, FileEntry, FileIndex, FileOrigin, Index, ResourceEntry, Symbol, SymbolKind,
};
use qbx_lua_ls::types::Type;
use serde_json::{json, Value};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qbx-resource-details-{}-{}",
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

    fn resource(&self, relative: &str, manifest: &str) {
        self.write(&format!("{relative}/fxmanifest.lua"), &format!("fx_version 'cerulean'\ngame 'gta5'\n{manifest}"));
    }

    fn uri(&self, relative: &str) -> Url {
        Url::from_file_path(self.0.join(relative)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.0.starts_with(std::env::temp_dir()));
        assert!(self.0.file_name().unwrap().to_string_lossy().starts_with("qbx-resource-details-"));
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
            json!({
                "processId": null, "rootUri": uri, "capabilities": {},
                "initializationOptions": {"diagnostics":{"enable":false,"workspace":false}},
                "workspaceFolders": [{"uri":uri,"name":"details fixture"}]
            }),
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

    fn details(&mut self, uri: Url) -> Value {
        self.request("qbx/resourceDetails", json!({"uri":uri}))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        self.server.take().unwrap().join().unwrap();
    }
}

fn sample() -> Fixture {
    let fixture = Fixture::new();
    fixture.resource("app", "shared_scripts {'shared.lua','@lib/init.lua'}\nclient_script 'client.lua'\nserver_script 'server.lua'\ndependencies {'lib','missing','duplicate','/server:7290','/onesync'}\nexport 'ManifestOnly'\n");
    fixture.write("app/shared.lua", "Config = {}\n");
    fixture.write("app/module.lua", "return {}\n");
    fixture.write("app/client.lua", "RegisterNetEvent('app:client', function() end)\nTriggerServerEvent('app:trigger')\nexports('ClientThing', function(value) return value end)\n");
    fixture.write("app/server.lua", "RegisterNetEvent('app:server', function(source) end)\nAddEventHandler('app:local', function(value) end)\nlib.callback.register('app:ox', function(source, count) end)\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.CreateCallback('app:qb', function(source, cb, item) end)\nlocal ESX = exports.es_extended:getSharedObject()\nESX.RegisterServerCallback('app:esx', function(source, cb, item) end)\nexports('ServerThing', function(source) return source end)\nexports(computed, function() end)\n");
    fixture.resource("lib", "shared_script 'init.lua'\n");
    fixture.write("lib/init.lua", "lib = {}\nexports('LibraryThing', function() end)\n");
    fixture.resource("[a]/duplicate", "");
    fixture.resource("[b]/duplicate", "");
    fixture.resource("consumer", "dependency 'app'\nshared_script '@app/shared.lua'\n");
    fixture.resource("replacement", "provide 'missing'\n");
    fixture
}

#[test]
fn protocol_lists_owned_files_registrations_exports_and_relations() {
    let fixture = sample();
    let mut client = Client::start(&fixture.0);
    let before = client.request("qbx/status", Value::Null);
    let result = client.details(fixture.uri("app"));
    assert_eq!(result, client.details(fixture.uri("app/fxmanifest.lua")));
    assert_eq!(result["resource"]["name"], "app");
    assert_eq!(result["resource"]["uri"], json!(fixture.uri("app")));
    assert_eq!(result["files"], json!({"total":4,"client":1,"server":1,"shared":1,"module":1}));
    assert_eq!(result["counts"], json!({"events":6,"exports":2}));
    let event_names: Vec<_> =
        result["events"].as_array().unwrap().iter().map(|row| row["name"].as_str().unwrap()).collect();
    assert_eq!(event_names, ["app:client", "app:esx", "app:local", "app:ox", "app:qb", "app:server"]);
    assert!(!event_names.contains(&"app:trigger"));
    assert_eq!(result["events"][1]["kind"], "ESX callback");
    assert_eq!(result["events"][4]["kind"], "QB-Core callback");
    assert_eq!(result["events"][4]["side"], "server");
    assert!(result["events"][4]["signature"].as_str().unwrap().contains("source"));
    assert_eq!(result["exports"][0]["name"], "ClientThing");
    assert_eq!(result["exports"][0]["location"]["uri"], json!(fixture.uri("app/client.lua")));
    assert_eq!(result["exports"][0]["location"]["range"]["start"], json!({"line":2,"character":8}));
    assert_eq!(result["exports"][1]["name"], "ServerThing");
    let dependencies = result["dependencies"].as_array().unwrap();
    assert_eq!(dependencies.len(), 3);
    let lib = dependencies.iter().find(|row| row["name"] == "lib").unwrap();
    assert_eq!(lib["kinds"], json!(["dependency", "import"]));
    assert_eq!(lib["status"], "resolved");
    assert_eq!(lib["targets"][0]["manifestUri"], json!(fixture.uri("lib/fxmanifest.lua")));
    let duplicate = dependencies.iter().find(|row| row["name"] == "duplicate").unwrap();
    assert_eq!(duplicate["status"], "ambiguous");
    assert_eq!(duplicate["targetCount"], 2);
    let missing = dependencies.iter().find(|row| row["name"] == "missing").unwrap();
    assert_eq!(missing["status"], "missing");
    assert_eq!(missing["targets"], json!([]));
    assert_eq!(result["constraints"], json!(["/onesync", "/server:7290"]));
    assert_eq!(result["dependents"][0]["name"], "consumer");
    assert_eq!(result["dependents"][0]["kinds"], json!(["dependency", "import"]));
    assert_eq!(result["dependents"][0]["targets"][0]["uri"], json!(fixture.uri("consumer")));
    assert_eq!(result["truncated"], json!({"events":0,"exports":0,"dependencies":0,"dependents":0}));
    let notes = result["notes"].to_string();
    assert!(notes.contains("provide/provides") && notes.contains("computed") && notes.contains("Manifest export"));
    assert_eq!(before, client.request("qbx/status", Value::Null), "requests do not change the index");
    fixture.write("app/not-notified.lua", "RegisterNetEvent('not-indexed')\n");
    assert_eq!(result, client.details(fixture.uri("app")), "details must not scan newly created files on demand");
    let ambiguous = client.details(fixture.uri("[a]/duplicate"));
    assert_eq!(ambiguous["dependents"][0]["name"], "app");
    assert_eq!(ambiguous["dependents"][0]["status"], "ambiguous");
    assert_eq!(ambiguous["dependents"][0]["targetCount"], 1);
}

#[test]
fn rejects_invalid_unknown_and_non_resource_uris() {
    let fixture = sample();
    let mut client = Client::start(&fixture.0);
    for params in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"uri":7}),
        json!({"uri":"not a uri"}),
        json!({"uri":"https://example.invalid/app"}),
        json!({"uri":fixture.uri("app/client.lua")}),
        json!({"uri":fixture.uri("unknown")}),
        json!({"uri":format!("{}#fragment", fixture.uri("app"))}),
        json!({"uri":format!("{}?query", fixture.uri("app"))}),
    ] {
        let error = client.response("qbx/resourceDetails", params.clone()).error.expect("expected InvalidParams");
        assert_eq!(error.code, -32602, "{params}");
        assert!(!error.message.is_empty());
    }
    assert_eq!(client.details(fixture.uri("app")), client.details(fixture.uri("unused/../app")));
    #[cfg(windows)]
    assert_eq!(
        client.details(fixture.uri("app")),
        client.details(Url::from_file_path(fixture.0.join("app").to_string_lossy().to_uppercase()).unwrap())
    );
}

#[test]
fn refresh_observes_unsaved_source_changes_and_saved_manifest_changes() {
    let fixture = sample();
    let mut client = Client::start(&fixture.0);
    let uri = fixture.uri("app/client.lua");
    client.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"lua","version":1,"text":"RegisterNetEvent('new:client')\nexports('Changed', function() end)\n"}}));
    let changed = client.details(fixture.uri("app"));
    assert!(!changed["events"].as_array().unwrap().iter().any(|row| row["name"] == "app:client"));
    assert!(changed["events"].as_array().unwrap().iter().any(|row| row["name"] == "new:client"));
    assert!(changed["exports"].as_array().unwrap().iter().any(|row| row["name"] == "Changed"));
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"-- empty\n"}]}),
    );
    assert_eq!(client.details(fixture.uri("app"))["counts"], json!({"events":5,"exports":1}));
    client.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}));
    assert_eq!(client.details(fixture.uri("app"))["counts"], json!({"events":6,"exports":2}));
    fixture.resource(
        "app",
        "server_scripts {'client.lua','server.lua'}\nshared_script 'shared.lua'\ndependency 'consumer'\n",
    );
    client.notify("textDocument/didSave", json!({"textDocument":{"uri":fixture.uri("app/fxmanifest.lua")}}));
    let saved = client.details(fixture.uri("app"));
    assert_eq!(saved["files"], json!({"total":4,"client":0,"server":2,"shared":1,"module":1}));
    assert_eq!(saved["dependencies"].as_array().unwrap().len(), 1);
    assert_eq!(saved["dependencies"][0]["name"], "consumer");
    fixture.write("app/added.lua", "exports('Added', function() end)\n");
    client
        .notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":fixture.uri("app/added.lua"),"type":1}]}));
    assert_eq!(client.details(fixture.uri("app"))["files"]["total"], 5);
    client
        .notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":fixture.uri("app/added.lua"),"type":3}]}));
    assert_eq!(client.details(fixture.uri("app"))["files"]["total"], 4);
}

fn indexed_resource(index: &mut Index, root: PathBuf, name: &str, manifest: &str) {
    index.resources.push(ResourceEntry {
        name: name.into(),
        manifest_path: root.join("fxmanifest.lua"),
        root,
        manifest: Manifest::from_chunk(&qbx_lua_syntax::parse(manifest)),
        files: Vec::new(),
        imports: Vec::new(),
        escrowed: false,
    });
}

#[test]
fn bounds_arrays_keeps_full_counts_and_preserves_duplicate_dependent_identities() {
    let fixture = Fixture::new();
    let root = fixture.0.join("index-only");
    let mut index = Index::default();
    let manifest = (0..205).map(|i| format!("dependency 'dep{i:03}'\ndependency '/server:{i}'\n")).collect::<String>()
        + "dependency 'duplicate'\n";
    indexed_resource(&mut index, root.clone(), "focus", &manifest);
    for i in 0..23 {
        indexed_resource(&mut index, fixture.0.join(format!("copy{i}/duplicate")), "duplicate", "");
    }
    for i in 0..230 {
        indexed_resource(&mut index, fixture.0.join(format!("consumer{i}")), "same-name", "dependency 'focus'");
    }
    let path = root.join("module.lua");
    let id = index.allocate(&path);
    let range = Range::new(Position::new(0, 0), Position::new(0, 1));
    let mut file = FileIndex::default();
    for i in 0..503 {
        file.events.push(EventDef {
            name: format!("event{i:03}").into(),
            kind: EventKind::NetEvent,
            family: EventFamily::Native,
            side: None,
            handler: None,
            range,
        });
    }
    for i in 0..501 {
        file.exports.push(Symbol {
            name: format!("export{i:03}").into(),
            kind: SymbolKind::Export,
            ty: Type::Unknown,
            doc: None,
            deprecated: false,
            literal: None,
            range,
        });
    }
    index.set_file(
        id,
        FileEntry {
            uri: Url::from_file_path(&path).unwrap(),
            path,
            origin: FileOrigin::Workspace,
            resource: Some(0),
            side: None,
            index: file,
        },
    );
    let result = details(&index, DetailsParams { uri: Url::from_file_path(&root).unwrap() }).unwrap();
    assert_eq!(result.events.len(), 500);
    assert_eq!(result.exports.len(), 500);
    assert_eq!(result.dependencies.len(), 200);
    assert_eq!(result.dependents.len(), 200);
    assert_eq!(result.constraints.len(), 200);
    assert_eq!((result.counts.events, result.counts.exports), (503, 501));
    assert_eq!(
        (result.truncated.events, result.truncated.exports, result.truncated.dependencies, result.truncated.dependents),
        (3, 1, 6, 30)
    );
    assert_eq!(result.files.total, 1);
    assert_eq!(result.files.module, 1);
    let distinct: std::collections::BTreeSet<_> =
        result.dependents.iter().map(|row| row.targets[0].uri.to_string()).collect();
    assert_eq!(distinct.len(), 200);
    assert!(result.notes.iter().any(|note| note.contains("runtime constraints were omitted")));
    // Select a different resource with only the ambiguous dependency so its candidate row is visible.
    indexed_resource(&mut index, fixture.0.join("target-cap"), "target-cap", "dependency 'duplicate'");
    let target_cap = details(&index, DetailsParams { uri: fixture.uri("target-cap") }).unwrap();
    assert_eq!(target_cap.dependencies[0].target_count, 23);
    assert_eq!(target_cap.dependencies[0].targets.len(), 20);
    assert_eq!(target_cap.dependencies[0].status, "ambiguous");
    assert!(target_cap.notes.iter().any(|note| note.contains("3 candidate resource folders")));
}

#[test]
fn long_display_strings_are_bounded_after_full_name_resolution() {
    let fixture = Fixture::new();
    let root = fixture.0.join("focus");
    let long_name = "a".repeat(9000);
    let constraint = format!("/gameBuild:{}", "1".repeat(3000));
    let mut index = Index::default();
    indexed_resource(
        &mut index,
        root.clone(),
        "focus",
        &format!("dependency '{long_name}'\ndependency '{constraint}'"),
    );
    indexed_resource(&mut index, fixture.0.join("provider"), &long_name, "");
    let path = root.join("module.lua");
    let id = index.allocate(&path);
    let mut file = FileIndex::default();
    file.events.push(EventDef {
        name: long_name.clone().into(),
        family: EventFamily::Native,
        kind: EventKind::NetEvent,
        side: None,
        handler: Some(std::sync::Arc::new(qbx_lua_ls::types::FunType {
            params: Vec::new(),
            returns: Vec::new(),
            is_method: false,
            generics: Vec::new(),
            overloads: Vec::new(),
        })),
        range: Range::new(Position::new(0, 0), Position::new(0, 1)),
    });
    index.set_file(
        id,
        FileEntry {
            uri: Url::from_file_path(&path).unwrap(),
            path,
            origin: FileOrigin::Workspace,
            resource: Some(0),
            side: None,
            index: file,
        },
    );
    let result = details(&index, DetailsParams { uri: fixture.uri("focus") }).unwrap();
    assert_eq!(result.dependencies[0].status, "resolved", "resolve the original name before shortening display text");
    assert_eq!(result.dependencies[0].targets[0].uri, fixture.uri("provider"));
    assert_eq!(result.dependencies[0].name.chars().count(), 2048);
    assert_eq!(result.events[0].name.chars().count(), 2048);
    assert_eq!(result.events[0].signature.as_ref().unwrap().chars().count(), 8192);
    assert_eq!(result.constraints[0].chars().count(), 2048);
    assert!(result.events[0].name.ends_with('…'));
    assert!(result.notes.iter().any(|note| note.contains("shortened for display")));
}
