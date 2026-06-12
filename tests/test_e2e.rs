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
