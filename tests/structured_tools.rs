use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::Url;
use serde_json::{json, Value};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qbx-structured-tools-{}-{}",
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
    fn manifest(&self, relative: &str, extra: &str) {
        self.write(&format!("{relative}/fxmanifest.lua"), &format!("fx_version 'cerulean'\ngame 'gta5'\n{extra}"));
    }
    fn uri(&self, relative: &str) -> Url {
        Url::from_file_path(self.0.join(relative)).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.0.starts_with(std::env::temp_dir()));
        assert!(self.0.file_name().unwrap().to_string_lossy().starts_with("qbx-structured-tools-"));
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
struct Client {
    connection: Connection,
    server: Option<JoinHandle<()>>,
    next: i32,
}
impl Client {
    fn new(fixture: &Fixture, enabled: bool) -> Self {
        let (server, connection) = Connection::memory();
        let thread = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || qbx_lua_ls::server::run_connection(server).unwrap())
            .unwrap();
        let mut client = Self { connection, server: Some(thread), next: 0 };
        let uri = fixture.uri("");
        client.request(
            "initialize",
            json!({"processId":null,"rootUri":uri,"capabilities":{},"workspaceFolders":[{"uri":uri,"name":"tools"}],
            "initializationOptions":{"diagnostics":{"enable":enabled,"workspace":false}}}),
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
            match self.connection.receiver.recv_timeout(Duration::from_secs(30)).unwrap() {
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
        response.result.unwrap()
    }
    fn open(&self, uri: Url, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri,"languageId":"lua","version":1,"text":text}}),
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
fn assets_extract_manifest_arguments_and_precise_native_references() {
    let f = Fixture::new();
    f.manifest("app", "client_script 'client.lua'\nfiles {'web/index.html','web/*.png'}\ndata_file 'DLC_ITYP_REQUEST' 'stream/props.ytyp'\ndata_file('AUDIO_WAVEPACK', 'audio/waves')\nui_page 'web/index.html'\nloadscreen 'load.html'\nfiles {dynamic, ['key']='not-a-path'}\n");
    let source = "local marker = '😀'; RequestModel(`adder`)\nCreatePed(4, GetHashKey('sultan'), 0, 0, 0)\nCreatePedInsideVehicle(veh, 4, joaat('blista'), -1)\nCreateVehicle(0xB779A091, 0, 0, 0)\nCreateObject(-1216765807, 0, 0, 0)\nDrawSprite('inventory', 'water', 0, 0, 1, 1)\nRequestNamedPtfxAsset('core')\nRequestScriptAudioBank('DLC_AUDIO/test', false)\n";
    f.write("app/client.lua", source);
    let mut client = Client::new(&f, false);
    let value = client.request("qbx/resourceAssets", json!({"uri":f.uri("app")}));
    let declarations = value["declarations"].as_array().unwrap();
    assert_eq!(declarations.len(), 7);
    assert!(declarations.iter().any(|d| d["dataType"] == "AUDIO_WAVEPACK" && d["value"] == "audio/waves"));
    let refs = value["references"].as_array().unwrap();
    assert_eq!(refs.len(), 9, "{value:#}");
    assert_eq!(refs[0]["value"], "adder");
    assert_eq!(refs[0]["hash"], 0xB779A091u32);
    assert_eq!(
        refs[0]["location"]["range"]["start"]["character"],
        source[..source.find('`').unwrap()].encode_utf16().count()
    );
    assert_eq!(refs[5]["kind"], "textureDictionary");
    assert_eq!(refs[6]["dictionary"], "inventory");
    assert_eq!(refs[6]["value"], "water");
    assert_eq!(refs[8]["kind"], "audioBank");
    assert!(refs[3].get("value").is_none());
}

#[test]
fn asset_lookup_uses_unsaved_sources_and_preserves_resource_ownership() {
    let f = Fixture::new();
    f.manifest("app", "client_scripts {'client.lua', '@lib/shared.lua'}\nfiles {'old.png'}");
    f.manifest("lib", "shared_script 'shared.lua'");
    f.write("app/client.lua", "RequestModel('old_model')");
    f.write("lib/shared.lua", "RequestModel('external_model')");
    let mut client = Client::new(&f, false);
    client.open(
        f.uri("app/client.lua"),
        "RequestModel('new_model')\ndo local RequestModel=function() end; RequestModel('shadowed') end\n",
    );
    client.open(
        f.uri("app/fxmanifest.lua"),
        "fx_version 'cerulean'\ngame 'gta5'\nclient_script 'client.lua'\nfiles {'new.png'}",
    );
    let value = client.request("qbx/resourceAssets", json!({"uri":f.uri("app/fxmanifest.lua")}));
    assert_eq!(value["references"].as_array().unwrap().len(), 1, "{value:#}");
    assert_eq!(value["references"][0]["value"], "new_model");
    assert_eq!(value["declarations"][1]["value"], "new.png");
    for uri in [f.uri("app/client.lua").to_string(), "command:evil".into(), f.uri("absent").to_string()] {
        assert!(client.response("qbx/resourceAssets", json!({"uri":uri})).error.is_some());
    }
}

#[test]
fn asset_arguments_respect_shadowed_globals_and_bound_results_without_changing_names() {
    let f = Fixture::new();
    f.manifest("app", "client_scripts {'client.lua','shadow.lua'}");
    f.write("app/client.lua", &"RequestModel('same')\n".repeat(2001));
    f.write("app/shadow.lua", "RequestModel(GetHashKey(123))\nRequestModel(GetHashKey(joaat('nested')))\ndo local _ENV = {}; RequestModel('shadowed') end\n");
    let mut client = Client::new(&f, false);
    let value = client.request("qbx/resourceAssets", json!({"uri":f.uri("app")}));
    assert_eq!(value["references"].as_array().unwrap().len(), 2000);
    assert_eq!(value["truncated"]["references"], 1);
    client.open(f.uri("app/client.lua"), "_G.RequestModel = function() end\nRequestModel('replacement')");
    assert!(client.request("qbx/resourceAssets", json!({"uri":f.uri("app")}))["references"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn resource_listing_is_paginated_and_keeps_duplicate_names_distinct() {
    let f = Fixture::new();
    f.manifest("one/same", "");
    f.manifest("two/same", "");
    f.manifest("other", "");
    let mut client = Client::new(&f, false);
    let a = client.request("qbx/resources", json!({"query":"same","limit":1}));
    let b = client.request("qbx/resources", json!({"query":"same","offset":1,"limit":1}));
    assert_eq!(a["total"], 2);
    assert_eq!(b["total"], 2);
    assert_ne!(a["items"][0]["uri"], b["items"][0]["uri"]);
    assert_eq!(client.request("qbx/resources", json!({"offset":100}))["items"], json!([]));
    for params in [
        Value::Null,
        json!({"limit":101}),
        json!({"limit":0}),
        json!({"offset":-1}),
        json!({"query":"x".repeat(257)}),
        json!({"command":"start"}),
    ] {
        assert!(client.response("qbx/resources", params).error.is_some());
    }
}

#[test]
fn diagnostic_snapshots_include_closed_sources_unsaved_edits_and_rule_settings() {
    let f = Fixture::new();
    f.manifest("app", "client_script 'client.lua'");
    f.write("app/client.lua", "UnknownAssistantCall()\n");
    let mut client = Client::new(&f, true);
    let uri = f.uri("app/client.lua");
    let before = client.request("qbx/diagnostics", json!({"uri":uri,"limit":1}));
    assert!(before["total"].as_u64().unwrap() > 0);
    assert_eq!(before["items"].as_array().unwrap().len(), 1);
    assert_eq!(before["items"][0]["uri"], uri.as_str());
    assert!(before["items"][0].get("data").is_none());
    client.open(uri.clone(), "print('clean')\n");
    assert_eq!(client.request("qbx/diagnostics", json!({"uri":uri}))["total"], 0);
    assert!(client.response("qbx/diagnostics", json!({"uri":f.uri("unindexed.lua")})).error.is_some());
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"qbxLua":{"diagnostics":{"enable":false,"workspace":false}}}}),
    );
    let disabled = client.request("qbx/diagnostics", json!({}));
    assert_eq!(disabled["total"], 0);
    assert!(disabled["notes"][0].as_str().unwrap().contains("disabled"));
}

#[test]
fn symbol_references_work_on_closed_and_unsaved_sources_and_reject_invalid_positions() {
    let f = Fixture::new();
    f.manifest("app", "client_script 'client.lua'");
    f.write("app/client.lua", "local item = 42\nprint(item)\n");
    let mut client = Client::new(&f, false);
    let uri = f.uri("app/client.lua");
    let result = client.request("qbx/symbolReferences", json!({"uri":uri,"line":1,"character":7}));
    assert_eq!(result["total"], 2, "{result}");
    assert_eq!(
        client.request("qbx/symbolReferences", json!({"uri":uri,"line":1,"character":7,"includeDeclaration":false}))
            ["total"],
        1
    );
    client.open(uri.clone(), "local item = 42\nprint(item,item)\n");
    assert_eq!(client.request("qbx/symbolReferences", json!({"uri":uri,"line":1,"character":7}))["total"], 3);
    for params in [
        json!({"uri":uri,"line":999,"character":0}),
        json!({"uri":uri,"line":0,"character":999}),
        json!({"uri":"https://example.invalid/code.lua","line":0,"character":0}),
        json!({"uri":uri,"line":0,"character":0,"limit":201}),
    ] {
        assert!(client.response("qbx/symbolReferences", params).error.is_some());
    }
}

#[test]
fn assistant_references_bound_related_sources_for_globals_and_members() {
    let f = Fixture::new();
    f.manifest("app", "client_scripts {'a.lua','b.lua'}");
    f.write("app/a.lua", "Shared = {field = 1}\nprint(Shared.field)\n");
    f.write("app/b.lua", "print(Shared.field)\n");
    let mut client = Client::new(&f, false);
    let query = |character| json!({"uri":f.uri("app/a.lua"),"line":1,"character":character});
    for character in [8, 15] {
        let complete = client.request("qbx/symbolReferences", query(character));
        assert!(
            complete["items"].as_array().unwrap().iter().any(|item| item["uri"] == f.uri("app/b.lua").as_str()),
            "{complete:#}"
        );
    }
    // The indexed related file changes after initialization. A bounded reference request must
    // inspect its size before parsing it, including the member-reference path.
    f.write("app/b.lua", &format!("--{}\nprint(Shared.field)\n", "x".repeat(2 * 1024 * 1024)));
    for character in [8, 15] {
        let partial = client.request("qbx/symbolReferences", query(character));
        assert!(partial["total"].as_u64().unwrap() > 0, "{partial:#}");
        assert!(partial["items"].as_array().unwrap().iter().all(|item| item["uri"] != f.uri("app/b.lua").as_str()));
        assert!(
            partial["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().contains("2 MiB")),
            "{partial:#}"
        );
    }
    assert!(client
        .response("qbx/symbolReferences", json!({"uri":f.uri("app/b.lua"),"line":1,"character":8}))
        .error
        .is_some());
}

#[test]
fn assistant_reference_result_caps_cover_global_local_and_member_paths() {
    let f = Fixture::new();
    f.manifest("app", "client_script 'a.lua'");
    f.write("app/a.lua", "Shared = {field = 1}\n");
    let mut client = Client::new(&f, false);
    f.write("app/a.lua", &format!("Shared = {{field = 1}}\n{}", "print(Shared.field)\n".repeat(20_005)));
    for character in [2, 12] {
        let result = client.request(
            "qbx/symbolReferences",
            json!({"uri":f.uri("app/a.lua"),"line":0,"character":character,"limit":1}),
        );
        assert_eq!(result["total"], 20_000, "{result:#}");
        assert!(
            result["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().contains("20,000")),
            "{result:#}"
        );
    }
    f.write("app/a.lua", &format!("local item = 1\n{}", "print(item)\n".repeat(20_005)));
    let result =
        client.request("qbx/symbolReferences", json!({"uri":f.uri("app/a.lua"),"line":0,"character":8,"limit":1}));
    assert_eq!(result["total"], 20_000, "{result:#}");
    assert!(!result["notes"].as_array().unwrap().is_empty());
}

#[test]
fn diagnostic_snapshots_bound_locale_reads_and_report_the_final_locale_result_cap() {
    let f = Fixture::new();
    f.manifest("app", "client_script 'a.lua'");
    f.write("app/a.lua", "locale('used')\nlocale('missing')\n");
    f.write("app/locales/en.json", r#"{"used":"Used","unused":"Unused"}"#);
    let mut client = Client::new(&f, true);
    let before = client.request("qbx/diagnostics", json!({"limit":200}));
    let has_code = |result: &Value, code| result["items"].as_array().unwrap().iter().any(|item| item["code"] == code);
    assert!(has_code(&before, "qbox/unknown-locale-key"), "{before:#}");
    assert!(has_code(&before, "qbox/unused-locale-key"), "{before:#}");
    f.write("app/locales/en.json", &format!(r#"{{"used":"{}"}}"#, "x".repeat(2 * 1024 * 1024)));
    let partial = client.request("qbx/diagnostics", json!({"limit":200}));
    assert!(!has_code(&partial, "qbox/unknown-locale-key"), "{partial:#}");
    assert!(!has_code(&partial, "qbox/unused-locale-key"), "{partial:#}");
    assert!(
        partial["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().contains("2 MiB")),
        "{partial:#}"
    );

    // This limit is reached in the final unused-locale pass, after ordinary source diagnostics.
    let values: serde_json::Map<String, Value> = (0..20_005)
        .map(|index| (format!("unused{index}"), json!("text")))
        .chain([(String::from("used"), json!("Used"))])
        .collect();
    f.write("app/locales/en.json", &serde_json::to_string(&values).unwrap());
    let capped = client.request("qbx/diagnostics", json!({"limit":1}));
    assert_eq!(capped["total"], 20_000, "{capped:#}");
    assert!(
        capped["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().contains("20,000")),
        "{capped:#}"
    );
}

#[test]
fn diagnostic_snapshots_preserve_bounded_start_order_and_skip_incomplete_config_checks() {
    let f = Fixture::new();
    f.manifest("resources/app", "client_script 'a.lua'");
    f.manifest("resources/lib", "client_script 'a.lua'");
    f.write("resources/app/a.lua", "exports.lib:run()\nUnknownAssistantCall()\n");
    f.write("resources/lib/a.lua", "exports('run', function() end)\n");
    f.write("server.cfg", "exec ordering.cfg\nensure app\n");
    f.write("ordering.cfg", "ensure lib\n");
    let mut client = Client::new(&f, true);
    let query = json!({"uri":f.uri("resources/app/a.lua"),"limit":200});
    let before = client.request("qbx/diagnostics", query.clone());
    assert!(before["notes"].as_array().unwrap().is_empty(), "{before:#}");
    assert!(
        before["items"].as_array().unwrap().iter().all(|item| item["code"] != "manifest/missing-dependency"),
        "{before:#}"
    );
    f.write("ordering.cfg", "# lib deliberately not started\n");
    let changed = client.request("qbx/diagnostics", query.clone());
    assert!(
        changed["items"].as_array().unwrap().iter().any(|item| item["code"] == "manifest/missing-dependency"),
        "{changed:#}"
    );
    f.write("ordering.cfg", &"x".repeat(2 * 1024 * 1024 + 1));
    let partial = client.request("qbx/diagnostics", query);
    assert!(
        partial["items"].as_array().unwrap().iter().all(|item| item["code"] != "manifest/missing-dependency"),
        "{partial:#}"
    );
    assert!(partial["items"].as_array().unwrap().iter().any(|item| item["code"] == "undefined-global"), "{partial:#}");
    assert!(
        partial["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().contains("Start-order")),
        "{partial:#}"
    );
}
