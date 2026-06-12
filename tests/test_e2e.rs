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
        let mut child = Command::new(env!("CARGO_BIN_EXE_clj-lsp"))
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn clj-lsp");

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
            if msg.get("id") == Some(&json!(id)) {
                if let Some(err) = msg.get("error") {
                    panic!("{} returned error: {}", method, err);
                }
                return msg["result"].clone();
            }
            self.notifications.push(msg);
        }
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
            self.notifications.push(msg);
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
    let tmp = tempfile::TempDir::new().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple_project");
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
