use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::Url;
use qbx_lua_analysis::manifest::Manifest;
use qbx_lua_ls::features::workspace_health::health;
use qbx_lua_ls::index::{FileEntry, FileIndex, FileOrigin, Index, ResourceEntry};
use serde_json::{json, Value};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qbx-workspace-health-{}-{}",
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
        assert!(self.0.file_name().unwrap().to_string_lossy().starts_with("qbx-workspace-health-"));
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
                "workspaceFolders": [{"uri":uri,"name":"health fixture"}]
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

    fn health(&mut self) -> Value {
        self.request("qbx/workspaceHealth", Value::Null)
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
    fixture.resource("app", "dependencies {'LIB','missing','MISSING','duplicate','/server:7290','/onesync'}\nshared_scripts {'main.lua','@lib/init.lua','@missing/init.lua','@duplicate/init.lua'}\n");
    fixture.write("app/main.lua", "return {}\n");
    fixture.resource("lib", "shared_script 'init.lua'\n");
    fixture.write("lib/init.lua", "return {}\n");
    fixture.resource("[a]/duplicate", "");
    fixture.resource("[b]/duplicate", "");
    fixture.resource("consumer", "dependency 'missing'\n");
    fixture.resource("replacement", "provide 'missing'\n");
    fixture.write("standalone.lua", "return {}\n");
    fixture
}

#[test]
fn protocol_reports_grouped_dependencies_and_duplicates_without_rescanning() {
    let fixture = sample();
    let mut client = Client::start(&fixture.0);
    let status = client.request("qbx/status", Value::Null);
    let result = client.health();
    assert_eq!(result, client.request("qbx/workspaceHealth", json!({})));
    assert_eq!(result["files"], 3);
    assert_eq!(result["resources"], 6);
    assert_eq!(result["counts"], json!({"duplicates":1,"missing":2,"ambiguous":1}));
    assert_eq!(result["truncated"], 0);
    let issues = result["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 4);
    assert_eq!(issues[0]["kind"], "duplicate");
    assert_eq!(issues[0]["name"], "duplicate");
    assert!(issues[0].get("resource").is_none());
    assert_eq!(issues[0]["kinds"], json!([]));
    assert_eq!(issues[0]["targetCount"], 2);
    assert_eq!(issues[0]["targets"][0]["uri"], json!(fixture.uri("[a]/duplicate")));
    let ambiguous = issues.iter().find(|issue| issue["kind"] == "ambiguous").unwrap();
    assert_eq!(ambiguous["resource"]["uri"], json!(fixture.uri("app")));
    assert_eq!(ambiguous["kinds"], json!(["dependency", "import"]));
    assert_eq!(ambiguous["targetCount"], 2);
    let missing = issues.iter().find(|issue| issue["kind"] == "missing" && issue["resource"]["name"] == "app").unwrap();
    assert_eq!(missing["name"], "missing");
    assert_eq!(missing["kinds"], json!(["dependency", "import"]));
    assert_eq!(missing["targets"], json!([]));
    assert_eq!(missing["targetCount"], 0);
    assert!(issues.iter().all(|issue| !issue["name"].as_str().unwrap().starts_with('/')));
    assert!(result["notes"].to_string().contains("provide/provides"));
    assert_eq!(status, client.request("qbx/status", Value::Null), "health is read-only");
    fixture.resource("missing", "");
    assert_eq!(result, client.health(), "new resources are not discovered by a health request");
}

#[test]
fn rejects_nonempty_or_nonobject_parameters_and_accepts_empty_workspace() {
    let fixture = Fixture::new();
    let mut client = Client::start(&fixture.0);
    let empty = client.health();
    assert_eq!(empty["files"], 0);
    assert_eq!(empty["resources"], 0);
    assert_eq!(empty["counts"], json!({"duplicates":0,"missing":0,"ambiguous":0}));
    assert_eq!(empty["issues"], json!([]));
    for params in [json!([]), json!(false), json!(3), json!(""), json!({"uri":"file:///unused"})] {
        let response = client.response("qbx/workspaceHealth", params);
        let error = response.error.expect("expected InvalidParams");
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("empty object"));
    }
}

#[test]
fn reflects_saved_manifest_and_watched_source_refresh() {
    let fixture = sample();
    let mut client = Client::start(&fixture.0);
    let before = client.health();
    let uri = fixture.uri("app/fxmanifest.lua");
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri,"languageId":"lua","version":1,"text":"dependency 'lib'\n"}}),
    );
    assert_eq!(before["counts"], client.health()["counts"], "manifest metadata uses saved text");
    fixture.resource("app", "dependency 'lib'\n");
    client.notify("textDocument/didSave", json!({"textDocument":{"uri":uri}}));
    assert_eq!(client.health()["counts"], json!({"duplicates":1,"missing":1,"ambiguous":0}));
    fixture.write("app/added.lua", "return {}\n");
    client
        .notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":fixture.uri("app/added.lua"),"type":1}]}));
    assert_eq!(client.health()["files"], 4);
    client
        .notify("workspace/didChangeWatchedFiles", json!({"changes":[{"uri":fixture.uri("app/added.lua"),"type":3}]}));
    assert_eq!(client.health()["files"], 3);
    fixture.resource("missing", "");
    client.request("qbx/reindex", Value::Null);
    let refreshed = client.health();
    assert_eq!(refreshed["resources"], 7);
    assert_eq!(refreshed["counts"], json!({"duplicates":1,"missing":0,"ambiguous":0}));
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
fn bounds_issues_and_candidates_without_losing_full_counts_or_identity() {
    let fixture = Fixture::new();
    let mut index = Index::default();
    let manifest =
        (0..520).map(|i| format!("dependency 'missing{i:03}'\n")).collect::<String>() + "dependency 'duplicate'\n";
    indexed_resource(&mut index, fixture.0.join("app"), "app", &manifest);
    for i in 0..23 {
        indexed_resource(&mut index, fixture.0.join(format!("copy{i:02}/duplicate")), "duplicate", "");
    }
    // A repeated index entry for the same lexical root must not add another provider or dependent.
    indexed_resource(&mut index, fixture.0.join("app/unused/.."), "app", &manifest);
    indexed_resource(&mut index, fixture.0.join("copy00/duplicate"), "duplicate", "");
    let result = health(&index);
    assert_eq!(result.resources, 24);
    assert_eq!((result.counts.duplicates, result.counts.missing, result.counts.ambiguous), (1, 520, 1));
    assert_eq!(result.issues.len(), 500);
    assert_eq!(result.truncated, 22);
    for issue in result.issues.iter().take(2) {
        assert_eq!(issue.targets.len(), 20);
        assert_eq!(issue.target_count, 23);
    }
    assert_eq!(result.issues[0].kind, "duplicate");
    assert_eq!(result.issues[1].kind, "ambiguous");
    assert!(result.notes.iter().any(|note| note.contains("6 candidate resource folders")));
    assert!(result.notes.iter().any(|note| note.contains("Some issue rows were omitted")));
    // Input ordering does not change the visible report for repeated equivalent entries.
    index.resources.reverse();
    assert!(
        serde_json::to_value(result).unwrap() == serde_json::to_value(health(&index)).unwrap(),
        "reordering equivalent index entries must not change the report"
    );
}

#[test]
fn resolves_full_names_before_shortening_and_excludes_stubs_and_manifests() {
    let fixture = Fixture::new();
    let mut index = Index::default();
    let long = "α".repeat(4000);
    indexed_resource(&mut index, fixture.0.join("app"), "app", &format!("dependency '{long}'\ndependency ''\n"));
    indexed_resource(&mut index, fixture.0.join("one"), &long, "");
    indexed_resource(&mut index, fixture.0.join("two"), &long, "");
    for (name, origin) in [
        ("module.lua", FileOrigin::Workspace),
        ("lib.lua", FileOrigin::Library),
        ("stub.lua", FileOrigin::Stub),
        ("fxmanifest.lua", FileOrigin::Workspace),
    ] {
        let path = fixture.0.join(name);
        let id = index.allocate(&path);
        index.set_file(
            id,
            FileEntry {
                uri: Url::from_file_path(&path).unwrap(),
                path,
                origin,
                resource: None,
                side: None,
                index: FileIndex::default(),
            },
        );
    }
    let result = health(&index);
    assert_eq!(result.files, 2);
    assert_eq!((result.counts.duplicates, result.counts.missing, result.counts.ambiguous), (1, 0, 1));
    assert_eq!(result.issues[1].target_count, 2, "resolution must use the unshortened name");
    for issue in &result.issues {
        assert_eq!(issue.name.chars().count(), 2048);
        assert!(issue.name.ends_with('…'));
        for target in &issue.targets {
            assert_eq!(target.name.chars().count(), 2048);
        }
    }
    assert!(result.notes.iter().any(|note| note.contains("shortened for display")));
    assert!(result.notes.iter().any(|note| note.starts_with("1 empty dependency")));
}
