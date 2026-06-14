//! End-to-end tests: spawn the real `clj-lsp` binary and speak LSP over
//! stdio with Content-Length framing, the same way VS Code/Calva drives it.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(20);

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    notifications: Vec<Value>,
    next_id: i64,
}

impl LspClient {
    /// Spawns the server binary with `cwd` set to the project root,
    /// mirroring how an editor launches it.
    fn start(project_root: &Path) -> Self {
        Self::start_with_env(project_root, &[])
    }

    /// Like [`start`] but sets extra environment variables on the server
    /// process (e.g. `LGX_HOME` for hermetic lgx dep resolution).
    fn start_with_env(project_root: &Path, envs: &[(&str, &Path)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clj-lsp"));
        cmd.current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().expect("failed to spawn clj-lsp");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut content_length: Option<usize> = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return; // server exited
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(len) = line.strip_prefix("Content-Length: ") {
                        content_length = len.parse().ok();
                    }
                }
                let Some(len) = content_length else { return };
                let mut buf = vec![0u8; len];
                if reader.read_exact(&mut buf).is_err() {
                    return;
                }
                let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
                    continue;
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
        });

        Self {
            child,
            stdin,
            incoming: rx,
            notifications: Vec::new(),
            next_id: 0,
        }
    }

    fn send(&mut self, msg: Value) {
        let body = serde_json::to_string(&msg).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Sends a request and blocks until its response arrives.
    /// Server-initiated messages received in the meantime are stashed.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let msg = self.request_full(method, params);
        if let Some(err) = msg.get("error") {
            panic!("{} returned error: {}", method, err);
        }
        msg["result"].clone()
    }

    /// Like `request` but expects a JSON-RPC error and returns it.
    fn request_expect_error(&mut self, method: &str, params: Value) -> Value {
        let msg = self.request_full(method, params);
        msg.get("error")
            .unwrap_or_else(|| panic!("{} unexpectedly succeeded: {}", method, msg))
            .clone()
    }

    fn request_full(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));

        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for response to {}", method));
            let msg = self
                .incoming
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("timed out waiting for response to {}", method));
            if msg.get("method").is_none() && msg.get("id") == Some(&json!(id)) {
                return msg;
            }
            self.stash(msg);
        }
    }

    /// Stashes a server-initiated message; server→client *requests*
    /// (e.g. client/registerCapability) get a null success response so the
    /// server never blocks on us.
    fn stash(&mut self, msg: Value) {
        if let (Some(id), Some(_)) = (msg.get("id").cloned(), msg.get("method")) {
            self.send(json!({ "jsonrpc": "2.0", "id": id, "result": null }));
        }
        self.notifications.push(msg);
    }

    /// Waits until a `window/logMessage` whose text contains `needle` has
    /// been received (checks already-stashed notifications first).
    fn wait_for_log(&mut self, needle: &str) {
        let matches = |m: &Value| {
            m["method"] == "window/logMessage"
                && m["params"]["message"]
                    .as_str()
                    .map(|s| s.contains(needle))
                    .unwrap_or(false)
        };
        if self.notifications.iter().any(matches) {
            return;
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for log: {}", needle));
            let msg = self
                .incoming
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("timed out waiting for log: {}", needle));
            let found = matches(&msg);
            self.stash(msg);
            if found {
                return;
            }
        }
    }

    /// Full editor-style startup: initialize (with rootUri), initialized,
    /// then wait for project indexing to finish.
    fn initialize(&mut self, root: &Path) -> Value {
        let root_uri = format!("file://{}", root.display());
        let result = self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": "fixture" }],
                "capabilities": {
                    "textDocument": { "definition": { "linkSupport": true } },
                    "general": { "positionEncodings": ["utf-16"] }
                },
                // Calva passes clojure-lsp settings here; the server must
                // tolerate unknown options.
                "initializationOptions": { "dependency-scheme": "jar" }
            }),
        );
        self.notify("initialized", json!({}));
        self.wait_for_log("Indexed");
        result
    }

    fn did_open(&mut self, path: &Path) {
        let text = std::fs::read_to_string(path).unwrap();
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": format!("file://{}", path.display()),
                    "languageId": "clojure",
                    "version": 1,
                    "text": text
                }
            }),
        );
    }

    fn goto_definition(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character }
            }),
        )
    }

    fn hover(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character }
            }),
        )
    }

    fn completion(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character }
            }),
        )
    }

    /// Incremental edit: inserts `text` at (line, character), version bump.
    fn did_change_insert(&mut self, path: &Path, line: u32, character: u32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()), "version": 2 },
                "contentChanges": [{
                    "range": {
                        "start": { "line": line, "character": character },
                        "end": { "line": line, "character": character }
                    },
                    "text": text
                }]
            }),
        )
    }

    fn text_document_content(&mut self, uri: &str) -> Value {
        self.request("workspace/textDocumentContent", json!({ "uri": uri }))
    }

    fn signature_help(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/signatureHelp",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character }
            }),
        )
    }

    fn document_symbols(&mut self, path: &Path) -> Value {
        self.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": format!("file://{}", path.display()) } }),
        )
    }

    fn workspace_symbols(&mut self, query: &str) -> Value {
        self.request("workspace/symbol", json!({ "query": query }))
    }

    fn code_action(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "range": {
                    "start": { "line": line, "character": character },
                    "end": { "line": line, "character": character }
                },
                "context": { "diagnostics": [] }
            }),
        )
    }

    /// Code action request carrying a diagnostic in context, as VS Code sends
    /// when the cursor is on a squiggle.
    fn code_action_for_diagnostic(&mut self, path: &Path, diagnostic: &Value) -> Value {
        self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "range": diagnostic["range"].clone(),
                "context": { "diagnostics": [diagnostic.clone()] }
            }),
        )
    }

    /// Waits for a `textDocument/publishDiagnostics` whose uri ends with
    /// `uri_suffix` and returns its `params` (checks already-stashed first).
    fn wait_for_diagnostics(&mut self, uri_suffix: &str) -> Value {
        let matches = |m: &Value| {
            m["method"] == "textDocument/publishDiagnostics"
                && m["params"]["uri"]
                    .as_str()
                    .map(|s| s.ends_with(uri_suffix))
                    .unwrap_or(false)
        };
        if let Some(m) = self.notifications.iter().find(|m| matches(m)) {
            return m["params"].clone();
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for diagnostics: {}", uri_suffix));
            let msg = self
                .incoming
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("timed out waiting for diagnostics: {}", uri_suffix));
            let found = matches(&msg);
            let params = msg["params"].clone();
            self.stash(msg);
            if found {
                return params;
            }
        }
    }

    fn references(&mut self, path: &Path, line: u32, character: u32, include_decl: bool) -> Value {
        self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": include_decl }
            }),
        )
    }

    fn rename(&mut self, path: &Path, line: u32, character: u32, new_name: &str) -> Value {
        self.request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character },
                "newName": new_name
            }),
        )
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Copies the simple_project fixture into a temp dir so tests can mutate it
/// (and so `.clj-lsp/` artifacts don't pollute the repo).
fn setup_project() -> tempfile::TempDir {
    setup_named("simple_project")
}

fn setup_named(name: &str) -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    copy_dir(&src, tmp.path());
    tmp
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Finds the (line, character) of `needle` in a file, pointing at its middle.
fn position_of(path: &Path, needle: &str) -> (u32, u32) {
    let text = std::fs::read_to_string(path).unwrap();
    for (i, line) in text.lines().enumerate() {
        if let Some(col) = line.find(needle) {
            return (i as u32, (col + needle.len() / 2) as u32);
        }
    }
    panic!("{:?} not found in {}", needle, path.display());
}

#[test]
fn test_e2e_letgo_navigation_into_lgx_deps() {
    let project = setup_named("letgo_project");
    let root = project.path().canonicalize().unwrap();
    // Hermetic: point LGX_HOME at the fixture's gitlibs tree.
    let lgx_home = root.join("lgxhome");

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let app = root.join("src/app.lg");
    client.did_open(&app);

    // Into an in-workspace :local/root dep (vendor/loc).
    let (line, ch) = position_of(&app, "loc/hello");
    let loc = client.goto_definition(&app, line, ch);
    let loc_uri = loc["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for loc/hello: {}", loc));
    assert!(
        loc_uri.ends_with("/vendor/loc/src/loc/core.lg"),
        "expected vendor/loc/src/loc/core.lg, got {}",
        loc_uri
    );

    // Into a git dep resolved under LGX_HOME/gitlibs.
    let (line, ch) = position_of(&app, "ext/greet");
    let ext = client.goto_definition(&app, line, ch);
    let ext_uri = ext["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for ext/greet: {}", ext));
    assert!(
        ext_uri.ends_with("/gitlibs/github.com/ext/lib/DEADBEEF/src/ext/core.lg"),
        "expected the git dep core.lg, got {}",
        ext_uri
    );
}

#[test]
fn test_e2e_no_diagnostics_on_lgx_edn() {
    // Opening lgx.edn must not flag dependency coordinates (`my/loc`,
    // `ext/lib`) as unresolved namespaces — EDN config files are not source.
    let project = setup_named("letgo_project");
    let root = project.path().canonicalize().unwrap();
    let lgx_home = root.join("lgxhome");

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let lgx = root.join("lgx.edn");
    client.did_open(&lgx);

    let diags = client.wait_for_diagnostics("/lgx.edn");
    let list = diags["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        list.is_empty(),
        "expected no diagnostics on lgx.edn, got {}",
        diags["diagnostics"]
    );
}

#[test]
fn test_e2e_cross_file_definition() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    let core = root.join("src/core.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.goto_definition(&utils, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition on core/add returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "expected core.clj, got {}",
        uri
    );
    let (def_line, _) = position_of(&core, "defn add");
    assert_eq!(result["range"]["start"]["line"], json!(def_line));
}

#[test]
fn test_e2e_definition_from_file_outside_source_paths() {
    // deps.edn has :paths ["src"], so dev/scratch.clj is NOT indexed at
    // startup — but navigation from an opened file must still work.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let scratch = root.join("dev/scratch.clj");
    std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
    std::fs::write(
        &scratch,
        "(ns scratch\n  (:require [simple.core :as core]))\n\n(core/add 1 2)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.did_open(&scratch);

    let (line, ch) = position_of(&scratch, "core/add");
    let result = client.goto_definition(&scratch, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition from a file outside :paths returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "expected core.clj, got {}",
        uri
    );
}

#[test]
fn test_e2e_hover_shows_doc() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.hover(&utils, line, ch);

    assert!(!result.is_null(), "hover returned null");
    let value = result["contents"]["value"].as_str().unwrap();
    assert!(value.contains("```clojure"), "no code block: {}", value);
    assert!(value.contains("Adds two numbers."), "no doc: {}", value);
}

#[test]
fn test_e2e_completion_with_alias_prefix() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.completion(&utils, line, ch);

    assert!(!result.is_null(), "completion returned null");
    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"core/add"),
        "expected core/add in completions, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_completion_bare_prefix_in_current_ns() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    // Type a partial bare symbol and complete it
    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(add-an");
    let result = client.completion(&utils, last_line, 7);

    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"add-and-double"),
        "expected add-and-double in completions, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_completion_clojure_core_builtins() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(redu");
    let result = client.completion(&utils, last_line, 5);

    let items = result.as_array().expect("expected CompletionItem array");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(
        labels.contains(&"reduce") && labels.contains(&"reduce-kv"),
        "expected reduce/reduce-kv in completions, got {:?}",
        labels
    );
    let reduce = items.iter().find(|i| i["label"] == "reduce").unwrap();
    let detail = reduce["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("clojure.core"),
        "expected clojure.core detail, got {}",
        detail
    );
}

#[test]
fn test_e2e_completion_from_jar_library() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn helper\n  \"Does helping.\"\n  [x]\n  x)\n\n(defn helper-two [x] x)\n")
        .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    client.did_change_insert(&consumer, 2, 0, "(u/hel");
    let result = client.completion(&consumer, 2, 6);

    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"u/helper") && labels.contains(&"u/helper-two"),
        "expected u/helper completions from JAR lib, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_completion_from_directory_library() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let libdir = tempfile::TempDir::new().unwrap();
    let lib_src = libdir.path().join("src");
    std::fs::create_dir_all(lib_src.join("gitlib")).unwrap();
    std::fs::write(
        lib_src.join("gitlib/util.clj"),
        "(ns gitlib.util)\n\n(defn helper\n  \"From a git dep.\"\n  [x]\n  x)\n",
    )
    .unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), lib_src.display().to_string()).unwrap();

    let consumer = root.join("src/uses_gitlib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-gitlib\n  (:require [gitlib.util :as u]))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    client.did_change_insert(&consumer, 2, 0, "(u/hel");
    let result = client.completion(&consumer, 2, 6);

    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"u/helper"),
        "expected u/helper completion from directory lib, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_completion_namespaces_and_aliases() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let libdir = tempfile::TempDir::new().unwrap();
    let lib_src = libdir.path().join("src");
    std::fs::create_dir_all(lib_src.join("gitlib")).unwrap();
    std::fs::write(
        lib_src.join("gitlib/util.clj"),
        "(ns gitlib.util)\n\n(defn helper [x] x)\n",
    )
    .unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), lib_src.display().to_string()).unwrap();

    let consumer = root.join("src/uses_gitlib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-gitlib\n  (:require [gitlib.util :as u]))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    // Namespace completion, as when typing inside (:require [gitli…])
    client.did_change_insert(&consumer, 2, 0, "gitli\n");
    let result = client.completion(&consumer, 2, 5);
    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"gitlib.util"),
        "expected gitlib.util namespace completion, got {:?}",
        labels
    );

    // Alias completion: typing "u" offers the alias itself
    client.did_change_insert(&consumer, 3, 0, "(u");
    let result = client.completion(&consumer, 3, 2);
    let items = result.as_array().expect("expected CompletionItem array");
    let alias = items
        .iter()
        .find(|i| i["label"] == "u" && i["detail"] == "alias for gitlib.util");
    assert!(
        alias.is_some(),
        "expected alias completion for u, got {:?}",
        items
            .iter()
            .map(|i| i["label"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_e2e_signature_help_while_typing_call() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;

    // Project fn via alias: "(core/add " — first parameter active
    client.did_change_insert(&utils, last_line, 0, "(core/add \n");
    let result = client.signature_help(&utils, last_line, 10);
    assert!(!result.is_null(), "no signature help for core/add");
    let label = result["signatures"][0]["label"].as_str().unwrap();
    assert_eq!(label, "(add a b)");
    assert_eq!(result["activeParameter"], json!(0));
    assert_eq!(
        result["signatures"][0]["parameters"][0]["label"],
        json!("a")
    );

    // Second argument: "(core/add 1 " — second parameter active
    client.did_change_insert(&utils, last_line + 1, 0, "(core/add 1 \n");
    let result = client.signature_help(&utils, last_line + 1, 12);
    assert_eq!(result["activeParameter"], json!(1));

    // clojure.core builtin with multiple arities
    client.did_change_insert(&utils, last_line + 2, 0, "(reduce f init ");
    let result = client.signature_help(&utils, last_line + 2, 15);
    assert!(!result.is_null(), "no signature help for reduce");
    let labels: Vec<&str> = result["signatures"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["label"].as_str())
        .collect();
    assert!(
        labels.iter().any(|l| l.contains("coll")),
        "expected reduce arities, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_definition_on_require_alias_and_namespace() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    // Cursor on the alias in `[simple.core :as core]`
    let (line, ch) = position_of(&utils, "core]");
    let result = client.goto_definition(&utils, line, ch);
    assert!(!result.is_null(), "no definition for require alias");
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "alias should navigate to core.clj, got {}",
        uri
    );
    assert_eq!(result["range"]["start"]["line"], json!(0));

    // Cursor on the namespace symbol itself
    let (line, ch) = position_of(&utils, "simple.core");
    let result = client.goto_definition(&utils, line, ch);
    assert!(!result.is_null(), "no definition for required namespace");
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "namespace should navigate to core.clj, got {}",
        uri
    );
}

#[test]
fn test_e2e_definition_on_core_builtin_navigates_into_clojure_jar() {
    // `defn`, `or`, `cond`… are ordinary definitions inside the clojure JAR;
    // bare usages must navigate into it even though the static core list
    // answers hover/completion.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("clojure-x.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("clojure/core.clj", opts).unwrap();
    zip.write_all(
        b"(ns ^{:doc \"core\"} clojure.core)\n\n(defmacro or\n  \"Evaluates exprs one at a time.\"\n  ([] nil)\n  ([x] x)\n  ([x & next] nil))\n",
    )
    .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(or 1 2)\n");
    let result = client.goto_definition(&utils, last_line, 2);

    assert!(
        !result.is_null(),
        "goto-definition on core builtin returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.starts_with("jar:file://") && uri.ends_with("!/clojure/core.clj"),
        "expected jar URI into clojure core, got {}",
        uri
    );
    // name_range points at `or` on the defmacro line
    assert_eq!(result["range"]["start"]["line"], json!(2));
}

#[test]
fn test_e2e_definition_on_alias_shadowing_core_symbol() {
    // `[simple.core :as str]`: the alias shadows clojure.core/str. On the
    // alias declaration it must navigate to the namespace; on a body usage
    // of bare `str` (which is clojure.core/str) it must not.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let consumer = root.join("src/shadow.clj");
    std::fs::write(
        &consumer,
        "(ns shadow\n  (:require [simple.core :as str]))\n\n(defn f [x]\n  (str/add x 1)\n  (str \"x\"))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.did_open(&consumer);

    // Alias declaration → namespace file
    let (line, ch) = position_of(&consumer, ":as str");
    let result = client.goto_definition(&consumer, line, ch + 2);
    assert!(
        !result.is_null(),
        "alias shadowing a core symbol did not navigate"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "expected core.clj, got {}",
        uri
    );

    // Bare core-symbol usage in the body → no navigation (clojure.core/str)
    let (line, ch) = position_of(&consumer, "(str \"x\")");
    let result = client.goto_definition(&consumer, line, ch);
    assert!(
        result.is_null(),
        "bare core symbol in body must not navigate to the alias ns: {:?}",
        result
    );
}

#[test]
fn test_e2e_document_symbols_outline() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    client.did_open(&core);

    let result = client.document_symbols(&core);
    assert!(!result.is_null(), "documentSymbol returned null");
    let symbols = result.as_array().expect("expected DocumentSymbol array");

    let names: Vec<&str> = symbols.iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, vec!["VERSION", "add", "multiply"]);

    // SymbolKind: Variable = 13, Function = 12
    let version = &symbols[0];
    assert_eq!(version["kind"], json!(13));
    let add = &symbols[1];
    assert_eq!(add["kind"], json!(12));

    // selectionRange points at the name, range covers the whole form
    let (def_line, _) = position_of(&core, "defn add");
    assert_eq!(add["selectionRange"]["start"]["line"], json!(def_line));
    assert_eq!(add["range"]["start"]["line"], json!(def_line));
    assert!(add["range"]["end"]["line"].as_u64().unwrap() > def_line as u64);

    // Live (unsaved) edits are reflected in the outline
    let last_line = std::fs::read_to_string(&core).unwrap().lines().count() as u32;
    client.did_change_insert(&core, last_line, 0, "(defn fresh [] 1)\n");
    let result = client.document_symbols(&core);
    let names: Vec<String> = result
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    assert!(
        names.contains(&"fresh".to_string()),
        "outline missing unsaved defn: {:?}",
        names
    );

    // Non-file documents (jar: virtual sources opened by the editor) must
    // be outlined from their open text, not rejected with a server error.
    let jar_uri = "jar:file:///some/lib.jar!/mylib/util.clj";
    client.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": jar_uri,
                "languageId": "clojure",
                "version": 1,
                "text": "(ns mylib.util)\n\n(defn helper [x] x)\n"
            }
        }),
    );
    let result = client.request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": jar_uri } }),
    );
    let names: Vec<&str> = result
        .as_array()
        .expect("expected symbols for jar document")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(names, vec!["helper"]);
}

#[test]
fn test_e2e_workspace_symbols_ranked_and_project_only() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    // A library JAR defining `addition` — must NOT appear in results
    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn addition [x] x)\n")
        .unwrap();
    zip.finish().unwrap();
    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let result = client.workspace_symbols("add");
    let symbols = result.as_array().expect("expected SymbolInformation array");
    let names: Vec<&str> = symbols.iter().filter_map(|s| s["name"].as_str()).collect();

    // Exact match ranks before prefix match; library symbol excluded
    assert_eq!(names, vec!["add", "add-and-double"]);
    let add = &symbols[0];
    assert_eq!(add["containerName"], json!("simple.core"));
    assert!(add["location"]["uri"]
        .as_str()
        .unwrap()
        .ends_with("/src/core.clj"));

    // Subsequence matching: "aad" finds add-and-double
    let result = client.workspace_symbols("aad");
    let names: Vec<&str> = result
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"add-and-double"),
        "subsequence match failed: {:?}",
        names
    );
}

#[test]
fn test_e2e_add_missing_require() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // consumer.clj uses `helpers/greet` without requiring simple.helpers.
    let consumer = root.join("src/consumer.clj");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "helpers/greet");
    let result = client.code_action(&consumer, line, ch);

    let actions = result.as_array().expect("expected code action array");
    let action = actions
        .iter()
        .find(|a| {
            a["title"]
                .as_str()
                .map(|t| t.contains("[simple.helpers :as helpers]"))
                .unwrap_or(false)
        })
        .expect("expected add-require action for simple.helpers");

    assert_eq!(action["kind"], json!("quickfix"));

    // The edit inserts the require spec into consumer.clj.
    let edits = action["edit"]["changes"]
        .as_object()
        .expect("expected WorkspaceEdit.changes")
        .values()
        .next()
        .expect("expected edits for the file")
        .as_array()
        .unwrap();
    let new_text = edits[0]["newText"].as_str().unwrap();
    assert!(
        new_text.contains("(:require [simple.helpers :as helpers])"),
        "unexpected edit text: {}",
        new_text
    );
}

#[test]
fn test_e2e_unresolved_namespace_diagnostic() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // consumer.clj uses `helpers/greet` without requiring simple.helpers.
    let consumer = root.join("src/consumer.clj");
    client.did_open(&consumer);

    let params = client.wait_for_diagnostics("/src/consumer.clj");
    let diags = params["diagnostics"].as_array().expect("diagnostics array");
    let unresolved = diags
        .iter()
        .find(|d| d["code"] == json!("unresolved-namespace"))
        .expect("expected an unresolved-namespace diagnostic");
    assert_eq!(unresolved["severity"], json!(2)); // WARNING
    assert!(unresolved["message"].as_str().unwrap().contains("helpers"));

    // VS Code requests code actions for the squiggle, passing the diagnostic.
    // The add-require fix is returned and carries that diagnostic so the
    // client binds them.
    let result = client.code_action_for_diagnostic(&consumer, unresolved);
    let actions = result.as_array().expect("code action array");
    let action = actions
        .iter()
        .find(|a| {
            a["title"]
                .as_str()
                .map(|t| t.contains("[simple.helpers :as helpers]"))
                .unwrap_or(false)
        })
        .expect("expected add-require action");
    assert_eq!(action["kind"], json!("quickfix"));
    assert_eq!(
        action["diagnostics"][0]["code"],
        json!("unresolved-namespace"),
        "fix should carry the diagnostic it resolves"
    );
}

#[test]
fn test_e2e_find_references() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    let utils = root.join("src/utils.clj");
    client.did_open(&core);

    // From the definition site, with declaration included
    let (line, ch) = position_of(&core, "add"); // `(defn add` is the first match
    let result = client.references(&core, line, ch, true);
    assert!(!result.is_null(), "references returned null");
    let locs = result.as_array().unwrap();
    let uris: Vec<&str> = locs.iter().filter_map(|l| l["uri"].as_str()).collect();
    assert_eq!(
        locs.len(),
        2,
        "expected declaration + usage, got {:?}",
        locs
    );
    assert!(uris.iter().any(|u| u.ends_with("/src/core.clj")));
    assert!(uris.iter().any(|u| u.ends_with("/src/utils.clj")));

    // Usage range covers only `add`, not the `core/` alias
    let usage = locs
        .iter()
        .find(|l| l["uri"].as_str().unwrap().ends_with("/src/utils.clj"))
        .unwrap();
    let utils_text = std::fs::read_to_string(&utils).unwrap();
    let usage_line = utils_text.lines().nth(6).unwrap(); // "  (* 2 (core/add x y)))"
    let name_col = usage_line.find("core/add").unwrap() + "core/".len();
    assert_eq!(usage["range"]["start"]["character"], json!(name_col));

    // Without declaration: only the usage
    let result = client.references(&core, line, ch, false);
    let locs = result.as_array().unwrap();
    assert_eq!(locs.len(), 1);
    assert!(locs[0]["uri"].as_str().unwrap().ends_with("/src/utils.clj"));

    // From the usage site, resolution gives the same answer
    let (uline, uch) = position_of(&utils, "core/add");
    client.did_open(&utils);
    let result = client.references(&utils, uline, uch, true);
    assert_eq!(result.as_array().unwrap().len(), 2);
}

#[test]
fn test_e2e_references_work_without_indexed_definition() {
    // References of an alias-qualified usage must work even when the target
    // library isn't indexed (yet) — the fqn is derivable from the alias.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let consumer = root.join("src/uses_unknown.clj");
    std::fs::write(
        &consumer,
        "(ns uses-unknown\n  (:require [unknown.lib :as ul]))\n\n(ul/go 1)\n(ul/go 2)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "ul/go");
    let result = client.references(&consumer, line, ch, true);
    assert!(!result.is_null(), "references returned null");
    assert_eq!(
        result.as_array().unwrap().len(),
        2,
        "both usages must be found: {:?}",
        result
    );
}

#[test]
fn test_e2e_rename_rejects_local_shadowing_global() {
    // `(defn f2 [add] add)` — the param shadows simple.core/add; rename and
    // references on it must NOT touch the global var.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    client.did_open(&core);
    let last_line = std::fs::read_to_string(&core).unwrap().lines().count() as u32;
    client.did_change_insert(&core, last_line, 0, "(defn f2 [add] add)\n");

    // Cursor on the param `add` (col 10)
    let error = client.request_expect_error(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": format!("file://{}", core.display()) },
            "position": { "line": last_line, "character": 11 },
            "newName": "plus"
        }),
    );
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("nothing to rename"),
        "got: {}",
        error
    );

    let refs = client.references(&core, last_line, 11, true);
    assert!(
        refs.is_null(),
        "local must have no global references: {:?}",
        refs
    );
}

#[test]
fn test_e2e_classpath_change_drops_stale_libs() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let make_jar = |name: &str, entry: &str, content: &[u8]| {
        let jar_path = root.join(name);
        let jar_file = std::fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(jar_file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file(entry, opts).unwrap();
        zip.write_all(content).unwrap();
        zip.finish().unwrap();
        jar_path
    };
    let jar_a = make_jar(
        "liba.jar",
        "mylib/util.clj",
        b"(ns mylib.util)\n(defn helper [x] x)\n",
    );
    let jar_b = make_jar(
        "libb.jar",
        "otherlib/core.clj",
        b"(ns otherlib.core)\n(defn other-fn [x] x)\n",
    );

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    let cp_file = cpcache.join("1.cp");
    std::fs::write(&cp_file, jar_a.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n\n(u/helper 42)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "u/helper");
    assert!(
        !client.goto_definition(&consumer, line, ch).is_null(),
        "lib A must resolve before the classpath change"
    );

    // Dependency swapped: A removed, B added
    std::fs::write(&cp_file, jar_b.display().to_string()).unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", cp_file.display()), "type": 2 }] }),
    );

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let result = client.goto_definition(&consumer, line, ch);
        if result.is_null() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stale lib A symbol still resolves after classpath change"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_e2e_deps_edn_change_reindexes_project_paths() {
    // Adding a source root to :paths (e.g. via git pull) must index its
    // files without a restart.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    std::fs::create_dir_all(root.join("extra")).unwrap();
    std::fs::write(
        root.join("extra/more.clj"),
        "(ns extra.more)\n\n(defn extra-fn [x] x)\n",
    )
    .unwrap();
    let consumer = root.join("src/uses_extra.clj");
    std::fs::write(
        &consumer,
        "(ns uses-extra\n  (:require [extra.more :as em]))\n\n(em/extra-fn 1)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.did_open(&consumer);
    let (line, ch) = position_of(&consumer, "em/extra-fn");
    assert!(
        client.goto_definition(&consumer, line, ch).is_null(),
        "extra/ must not be indexed while outside :paths"
    );

    // :paths gains the new root
    let deps = root.join("deps.edn");
    std::fs::write(
        &deps,
        "{:paths [\"src\" \"extra\"]\n :deps {org.clojure/clojure {:mvn/version \"1.11.1\"}}}\n",
    )
    .unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", deps.display()), "type": 2 }] }),
    );

    let deadline = Instant::now() + TIMEOUT;
    let mut result = Value::Null;
    while Instant::now() < deadline {
        result = client.goto_definition(&consumer, line, ch);
        if !result.is_null() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let uri = result["uri"]
        .as_str()
        .expect("definition after :paths change");
    assert!(uri.ends_with("/extra/more.clj"), "got {}", uri);
}

#[test]
fn test_e2e_rename_across_files() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    // Third file referring to `add` via :refer — rename must fix the
    // refer vector and the bare usage too
    std::fs::write(
        root.join("src/refers.clj"),
        "(ns simple.refers\n  (:require [simple.core :refer [add]]))\n\n(add 1 2)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    client.did_open(&core);

    let (line, ch) = position_of(&core, "add");
    let result = client.rename(&core, line, ch, "plus");
    assert!(!result.is_null(), "rename returned null");
    let changes = result["changes"]
        .as_object()
        .expect("expected WorkspaceEdit.changes");

    assert_eq!(
        changes.len(),
        3,
        "expected 3 files edited: {:?}",
        changes.keys().collect::<Vec<_>>()
    );

    let edits_for = |suffix: &str| -> Vec<Value> {
        changes
            .iter()
            .find(|(uri, _)| uri.ends_with(suffix))
            .unwrap_or_else(|| panic!("no edits for {}", suffix))
            .1
            .as_array()
            .unwrap()
            .clone()
    };

    // Declaration edit
    let core_edits = edits_for("/src/core.clj");
    assert_eq!(core_edits.len(), 1);
    assert_eq!(core_edits[0]["newText"], json!("plus"));
    assert_eq!(core_edits[0]["range"]["start"]["line"], json!(line));

    // Alias-qualified usage: edit covers only the name part
    let utils_edits = edits_for("/src/utils.clj");
    assert_eq!(utils_edits.len(), 1);
    let utils_text = std::fs::read_to_string(root.join("src/utils.clj")).unwrap();
    let usage_line = utils_text.lines().nth(6).unwrap();
    let name_col = usage_line.find("core/add").unwrap() + "core/".len();
    assert_eq!(
        utils_edits[0]["range"]["start"]["character"],
        json!(name_col)
    );
    assert_eq!(
        utils_edits[0]["range"]["end"]["character"],
        json!(name_col + 3)
    );

    // :refer vector entry + bare usage
    let refers_edits = edits_for("/src/refers.clj");
    assert_eq!(
        refers_edits.len(),
        2,
        "refer vector + usage: {:?}",
        refers_edits
    );
}

#[test]
fn test_e2e_rename_rejects_library_symbols() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    // `str` in the body is clojure.core/str
    let (line, ch) = position_of(&utils, "(str \"Hello");
    let error = client.request_expect_error(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": format!("file://{}", utils.display()) },
            "position": { "line": line, "character": ch + 1 },
            "newName": "my-str"
        }),
    );
    let msg = error["message"].as_str().unwrap();
    assert!(
        msg.contains("rename"),
        "expected a rename rejection message, got: {}",
        msg
    );
}

#[test]
fn test_e2e_rename_uses_unsaved_edits() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    let utils = root.join("src/utils.clj");
    client.did_open(&core);
    client.did_open(&utils);

    // Add an unsaved usage of core/add in utils.clj
    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(core/add 9 9)\n");

    let (line, ch) = position_of(&core, "add");
    let result = client.rename(&core, line, ch, "plus");
    let changes = result["changes"].as_object().unwrap();
    let utils_edits = changes
        .iter()
        .find(|(uri, _)| uri.ends_with("/src/utils.clj"))
        .unwrap()
        .1
        .as_array()
        .unwrap();

    assert_eq!(
        utils_edits.len(),
        2,
        "saved + unsaved usage must both be edited: {:?}",
        utils_edits
    );
    assert!(
        utils_edits
            .iter()
            .any(|e| e["range"]["start"]["line"] == json!(last_line)),
        "unsaved usage line missing from edits"
    );
}

#[test]
fn test_e2e_watched_files_keep_index_fresh() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // Consumer of a namespace that doesn't exist yet
    let consumer = root.join("src/uses_fresh.clj");
    std::fs::write(
        &consumer,
        "(ns uses-fresh\n  (:require [simple.fresh :as fr]))\n\n(fr/fresh-fn 1)\n",
    )
    .unwrap();
    client.did_open(&consumer);
    let (line, ch) = position_of(&consumer, "fr/fresh-fn");

    assert!(
        client.goto_definition(&consumer, line, ch).is_null(),
        "definition should not resolve before the file exists"
    );

    // Simulate `git pull` creating the file (no editor save involved)
    let fresh = root.join("src/fresh.clj");
    std::fs::write(&fresh, "(ns simple.fresh)\n\n(defn fresh-fn [x] x)\n").unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", fresh.display()), "type": 1 }] }),
    );

    // Notifications are processed asynchronously — poll
    let deadline = Instant::now() + TIMEOUT;
    let mut result = Value::Null;
    while Instant::now() < deadline {
        result = client.goto_definition(&consumer, line, ch);
        if !result.is_null() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let uri = result["uri"]
        .as_str()
        .expect("definition after Created event");
    assert!(uri.ends_with("/src/fresh.clj"), "got {}", uri);

    // Simulate the file being deleted
    std::fs::remove_file(&fresh).unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", fresh.display()), "type": 3 }] }),
    );
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let result = client.goto_definition(&consumer, line, ch);
        if result.is_null() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "definition still resolves after Deleted event"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_e2e_definition_after_in_memory_edit() {
    // Type new code without saving: didChange must keep the in-memory
    // document in sync so navigation works from unsaved edits.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    let core = root.join("src/core.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(core/multiply 3 4)\n");

    let result = client.goto_definition(&utils, last_line, 8);

    assert!(
        !result.is_null(),
        "goto-definition on unsaved edit returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "expected core.clj, got {}",
        uri
    );
    let (def_line, _) = position_of(&core, "defn multiply");
    assert_eq!(result["range"]["start"]["line"], json!(def_line));
}

#[test]
fn test_e2e_jar_definition_and_content() {
    // Library symbol: definition must return a jar: URI, and
    // workspace/textDocumentContent must serve the source behind it.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn helper\n  \"Does helping.\"\n  [x]\n  x)\n")
        .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n\n(u/helper 42)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "u/helper");
    let result = client.goto_definition(&consumer, line, ch);

    assert!(!result.is_null(), "goto-definition into JAR returned null");
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(uri.starts_with("jar:file://"), "expected jar: URI: {}", uri);
    assert!(uri.ends_with("!/mylib/util.clj"), "wrong entry: {}", uri);

    let content = client.text_document_content(uri);
    let text = content["text"].as_str().expect("expected text");
    assert!(text.contains("(defn helper"), "wrong JAR content: {}", text);
}

#[test]
fn test_e2e_gitlib_directory_dependency() {
    // Git deps (and :local/root deps) appear on the classpath as source
    // *directories* (~/.gitlibs/libs/...), not JARs. They must be indexed
    // and navigable via plain file: URIs.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let libdir = tempfile::TempDir::new().unwrap();
    let lib_src = libdir.path().join("src");
    std::fs::create_dir_all(lib_src.join("gitlib")).unwrap();
    let lib_file = lib_src.join("gitlib/util.clj");
    std::fs::write(
        &lib_file,
        "(ns gitlib.util)\n\n(defn helper\n  \"From a git dep.\"\n  [x]\n  x)\n",
    )
    .unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), lib_src.display().to_string()).unwrap();

    let consumer = root.join("src/uses_gitlib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-gitlib\n  (:require [gitlib.util :as u]))\n\n(u/helper 42)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "u/helper");
    let result = client.goto_definition(&consumer, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition into a directory dep returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    let expected = format!("file://{}", lib_file.canonicalize().unwrap().display());
    assert_eq!(uri, expected, "expected file: URI into the lib directory");
    let (def_line, _) = position_of(&lib_file, "defn helper");
    assert_eq!(result["range"]["start"]["line"], json!(def_line));
}

/// Full realistic scenario against a real Maven classpath. Requires the
/// `clojure` CLI and network/m2 access, so it is ignored by default:
/// `cargo test --test test_e2e -- --ignored`
#[test]
#[ignore = "requires clojure CLI (downloads deps on first run)"]
fn test_e2e_real_classpath_navigation() {
    let project = tempfile::TempDir::new().unwrap();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("deps.edn"),
        r#"{:paths ["src"]
 :deps {org.clojure/data.json {:mvn/version "2.5.0"}}}
"#,
    )
    .unwrap();
    let app = root.join("src/app.clj");
    std::fs::write(
        &app,
        "(ns app\n  (:require [clojure.data.json :as json]))\n\n(json/write-str {:a 1})\n",
    )
    .unwrap();

    // Produce .cpcache the same way a real project gets one
    let out = std::process::Command::new("clojure")
        .args(["-Spath"])
        .current_dir(&root)
        .output()
        .expect("clojure CLI not available");
    assert!(out.status.success(), "clojure -Spath failed");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&app);

    let (line, ch) = position_of(&app, "json/write-str");
    let result = client.goto_definition(&app, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition into data.json returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.starts_with("jar:file://") && uri.ends_with("!/clojure/data/json.clj"),
        "unexpected URI: {}",
        uri
    );

    let content = client.text_document_content(uri);
    let text = content["text"].as_str().expect("expected text");
    assert!(text.contains("(defn write-str"), "wrong JAR content");
}

#[test]
fn test_e2e_paths_inside_alias_do_not_break_indexing() {
    // A deps.edn whose only `:paths` lives inside an alias (tools.build
    // convention) must not be mistaken for the project's source paths —
    // otherwise src/ is never indexed and all navigation breaks.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    std::fs::write(
        root.join("deps.edn"),
        r#"{:deps {org.clojure/clojure {:mvn/version "1.11.1"}}
 :aliases {:build {:paths ["build"]
                   :deps {io.github.clojure/tools.build {:mvn/version "0.9.6"}}}}}
"#,
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.goto_definition(&utils, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition returned null: src/ was not indexed (alias :paths hijacked source-path detection)"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "expected core.clj, got {}",
        uri
    );
}

#[test]
fn test_e2e_project_symbols_not_shadowed_by_jars() {
    // A classpath JAR containing the same namespace as the project (e.g. an
    // older version of the project installed in ~/.m2) must not hijack
    // navigation for project files.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("old-simple.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("simple/core.clj", opts).unwrap();
    zip.write_all(b"(ns simple.core)\n\n(defn add [a b] (+ a b))\n")
        .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.goto_definition(&utils, line, ch);

    assert!(!result.is_null(), "goto-definition returned null");
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "project symbol shadowed by JAR: {}",
        uri
    );
}
