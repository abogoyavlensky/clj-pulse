//! End-to-end tests: spawn the real `clj-pulse` binary and speak LSP over
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clj-pulse"));
        cmd.current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().expect("failed to spawn clj-pulse");

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

    /// Zed-shaped startup: only `workspaceFolders` (no deprecated `rootUri`),
    /// offering UTF-8 then UTF-16 position encodings — what Zed's LSP client
    /// sends. Exercises the same indexing path real Zed users hit.
    fn initialize_zed(&mut self, root: &Path) -> Value {
        let root_uri = format!("file://{}", root.display());
        let result = self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "workspaceFolders": [{ "uri": root_uri, "name": "fixture" }],
                "capabilities": {
                    "textDocument": { "definition": { "linkSupport": true } },
                    "general": { "positionEncodings": ["utf-8", "utf-16"] }
                }
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

    /// clojure-lsp's custom jar content request (what Calva calls). Returns the
    /// raw content string.
    fn dependency_contents(&mut self, uri: &str) -> Value {
        self.request("clojure/dependencyContents", json!({ "uri": uri }))
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

    /// Code action request restricted to specific kinds, as VS Code sends for
    /// "Organize Imports" / code-actions-on-save (`context.only`).
    fn code_action_only(&mut self, path: &Path, only: &[&str]) -> Value {
        self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "context": { "diagnostics": [], "only": only }
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

    // URI-addressed variants: a JAR entry the editor displays is identified by
    // its `jar:` URI, not a filesystem path, so these drive navigation/inspection
    // from inside a library file.

    fn did_open_uri(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "clojure",
                    "version": 1,
                    "text": text
                }
            }),
        );
    }

    fn goto_definition_uri(&mut self, uri: &str, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
    }

    fn references_uri(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        include_decl: bool,
    ) -> Value {
        self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": include_decl }
            }),
        )
    }

    fn hover_uri(&mut self, uri: &str, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
    }

    /// Rename by URI, returning the raw JSON-RPC message so the caller can
    /// assert on either `result` or `error`.
    fn rename_uri(&mut self, uri: &str, line: u32, character: u32, new_name: &str) -> Value {
        self.request_full(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": uri },
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
/// (and so `.clj-pulse/` artifacts don't pollute the repo).
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

/// Like [`position_of`] but over an in-memory string — JAR content is served
/// from the archive, not from a file on disk.
fn position_in_text(text: &str, needle: &str) -> (u32, u32) {
    for (i, line) in text.lines().enumerate() {
        if let Some(col) = line.find(needle) {
            return (i as u32, (col + needle.len() / 2) as u32);
        }
    }
    panic!("{:?} not found in text", needle);
}

/// Applies LSP `TextEdit` JSON values to `source` (highest position first, so
/// earlier offsets stay valid) and returns the result.
fn apply_edits(source: &str, edits: &[Value]) -> String {
    let pos = |e: &Value| {
        (
            e["range"]["start"]["line"].as_u64().unwrap(),
            e["range"]["start"]["character"].as_u64().unwrap(),
        )
    };
    let mut ordered: Vec<&Value> = edits.iter().collect();
    ordered.sort_by_key(|e| std::cmp::Reverse(pos(e)));

    let mut text = source.to_string();
    for e in ordered {
        let start = offset_of(&text, &e["range"]["start"]);
        let end = offset_of(&text, &e["range"]["end"]);
        let new_text = e["newText"].as_str().unwrap();
        text = format!("{}{}{}", &text[..start], new_text, &text[end..]);
    }
    text
}

/// Byte offset of an LSP position (`{line, character}`) in `source`.
fn offset_of(source: &str, pos: &Value) -> usize {
    let line = pos["line"].as_u64().unwrap() as u32;
    let character = pos["character"].as_u64().unwrap() as u32;
    let (mut l, mut c) = (0u32, 0u32);
    for (i, ch) in source.char_indices() {
        if l == line && c == character {
            return i;
        }
        if ch == '\n' {
            l += 1;
            c = 0;
        } else {
            c += ch.len_utf16() as u32;
        }
    }
    source.len()
}

/// A deps.edn project whose classpath holds a JAR with two namespaces where
/// `mylib.core` requires `mylib.util` (the transitive-dependency shape). The
/// project consumer requires and uses both. Returns the tempdir (keep it alive)
/// and the canonicalized project root.
fn two_ns_jar_project() -> (tempfile::TempDir, std::path::PathBuf) {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn helper [x] x)\n")
        .unwrap();
    zip.start_file("mylib/core.clj", opts).unwrap();
    zip.write_all(
        b"(ns mylib.core\n  (:require [mylib.util :as util]))\n\n(defn run [x] (util/helper x))\n",
    )
    .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.core :as core]\n            [mylib.util :as util]))\n\n(core/run 1)\n(util/helper 2)\n",
    )
    .unwrap();

    (project, root)
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
fn test_e2e_letgo_core_navigation() {
    // A pinned let-go project (`:lg-version`) with no deps of its own: bare
    // builtins and clojure.*-aliased stdlib must navigate into the let-go core
    // source that `lgx install` fetched under LGX_HOME.
    let project = setup_named("letgo_core_project");
    let root = project.path().canonicalize().unwrap();
    let lgx_home = root.join("lgxhome");

    // Stand in for what `lgx install` fetched for version 0.0.1.
    let core = lgx_home.join("let-go/source/0.0.1/pkg/rt/core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("core.lg"), "(ns core)\n(defn map [f c] c)\n").unwrap();
    std::fs::write(
        core.join("string.lg"),
        "(ns string)\n(defn join [sep c] sep)\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    // Fires even though the project has no lgx deps — core indexing counts.
    client.wait_for_log("library indexing complete");

    let app = root.join("src/app.lg");
    client.did_open(&app);

    // Bare `map` is auto-referred from let-go's built-in core → core.lg.
    let (line, ch) = position_of(&app, "map");
    let m = client.goto_definition(&app, line, ch);
    let m_uri = m["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for bare map: {}", m));
    assert!(
        m_uri.ends_with("/pkg/rt/core/core.lg"),
        "expected let-go core.lg, got {}",
        m_uri
    );

    // `str/join` through the `clojure.string` alias → the stdlib string.lg.
    let (line, ch) = position_of(&app, "str/join");
    let j = client.goto_definition(&app, line, ch);
    let j_uri = j["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for str/join: {}", j));
    assert!(
        j_uri.ends_with("/pkg/rt/core/string.lg"),
        "expected let-go string.lg, got {}",
        j_uri
    );
}

#[test]
fn test_e2e_letgo_builtins_hover() {
    // let-go built-ins with no `.lg` source — special forms (`if`) and native
    // core fns (`count`) — describe themselves on hover but never navigate.
    let project = setup_named("letgo_core_project");
    let root = project.path().canonicalize().unwrap();
    let lgx_home = root.join("lgxhome");
    let core = lgx_home.join("let-go/source/0.0.1/pkg/rt/core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("core.lg"), "(ns core)\n(defn map [f c] c)\n").unwrap();
    std::fs::write(
        core.join("string.lg"),
        "(ns string)\n(defn join [sep c] sep)\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let app = root.join("src/app.lg");
    client.did_open(&app);

    // Special form `if`: hover describes it; goto-def is a no-op.
    let (line, ch) = position_of(&app, "if ");
    let h = client.hover(&app, line, ch);
    let val = h["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover for if: {}", h));
    assert!(val.contains("special form"), "if hover: {}", val);
    let def = client.goto_definition(&app, line, ch);
    assert!(def.is_null(), "if must not navigate, got {}", def);

    // Native core fn `count`: hover labels it native, doc borrowed from clojure.core.
    let (line, ch) = position_of(&app, "count");
    let h = client.hover(&app, line, ch);
    let val = h["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover for count: {}", h));
    assert!(val.contains("let-go core (native)"), "count hover: {}", val);

    // Regression: goto-def on the `str` alias declaration still resolves to the
    // stdlib namespace, even though `str` is also a native core fn name.
    let (line, ch) = position_of(&app, "str]");
    let d = client.goto_definition(&app, line, ch);
    let uri = d["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for str alias: {}", d));
    assert!(
        uri.ends_with("/pkg/rt/core/string.lg"),
        "str alias should navigate to string.lg, got {}",
        uri
    );
}

#[test]
fn test_e2e_clojure_special_form_hover() {
    // In a Clojure project, special forms (`if`) describe themselves on hover
    // and never navigate; clojure.core fns (`map`) keep their existing behavior.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // A scratch file (not a committed fixture, so shared-fixture assertions in
    // other tests are untouched); did_open indexes it on the fly.
    let f = root.join("src/special_forms_demo.clj");
    std::fs::write(
        &f,
        "(ns special-forms-demo)\n\n(if true 1 2)\n(map inc [1 2])\n",
    )
    .unwrap();
    client.did_open(&f);

    // Special form `if`: hover labels it; goto-def is a no-op.
    let (line, ch) = position_of(&f, "if ");
    let h = client.hover(&f, line, ch);
    let val = h["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover for if: {}", h));
    assert!(val.contains("special form"), "if hover: {}", val);
    let def = client.goto_definition(&f, line, ch);
    assert!(def.is_null(), "if must not navigate, got {}", def);

    // A clojure.core fn still hovers as clojure.core (unchanged behavior).
    let (line, ch) = position_of(&f, "map ");
    let h = client.hover(&f, line, ch);
    let val = h["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover for map: {}", h));
    assert!(val.contains("clojure.core"), "map hover: {}", val);
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
fn test_e2e_definition_on_protocol_method() {
    // Go-to-definition on a protocol-method call lands on the method's
    // signature inside the defprotocol.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let proto = root.join("src/proto.clj");
    std::fs::write(
        &proto,
        "(ns proto)\n(defprotocol Storage\n  (fetch [this id]))\n\n(defn run [s] (fetch s 1))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&proto);

    let (uline, uch) = position_of(&proto, "fetch s");
    let result = client.goto_definition(&proto, uline, uch);
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(uri.ends_with("/src/proto.clj"), "got {}", uri);

    let (decl_line, _) = position_of(&proto, "fetch [this");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
}

#[test]
fn test_e2e_definition_on_record_factory() {
    // Go-to-definition on the auto-generated `map->DB` / `->DB` factory fns
    // lands on the `defrecord`.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let recs = root.join("src/recs.clj");
    std::fs::write(
        &recs,
        "(ns recs)\n(defrecord DB [conn])\n\n(defn make [c] (map->DB {:conn c}))\n(defn make2 [c] (->DB c))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&recs);

    let (decl_line, _) = position_of(&recs, "defrecord DB");

    for usage in ["map->DB {", "->DB c"] {
        let (l, c) = position_of(&recs, usage);
        let result = client.goto_definition(&recs, l, c);
        let uri = result["uri"]
            .as_str()
            .unwrap_or_else(|| panic!("no def for {}: {}", usage, result));
        assert!(uri.ends_with("/src/recs.clj"), "{} -> {}", usage, uri);
        assert_eq!(
            result["range"]["start"]["line"],
            json!(decl_line),
            "{} did not navigate to the defrecord",
            usage
        );
    }
}

#[test]
fn test_e2e_definition_on_protocol_method_impl() {
    // A protocol method *implementation* navigates to the protocol's
    // *declaration* in another namespace — exercises the occurrence fallback,
    // since `resolve_symbol` can't resolve the bare impl name across namespaces.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let proto = root.join("src/proto.clj");
    std::fs::write(
        &proto,
        "(ns app.proto)\n(defprotocol Worker\n  (run-task [this job]))\n",
    )
    .unwrap();
    let impl_file = root.join("src/impl.clj");
    std::fs::write(
        &impl_file,
        "(ns app.impl\n  (:require [app.proto :as p]))\n(defrecord Runner [id]\n  p/Worker\n  (run-task [this job] job))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&impl_file);

    let (line, ch) = position_of(&impl_file, "run-task");
    let result = client.goto_definition(&impl_file, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for run-task impl: {}", result));
    assert!(uri.ends_with("/src/proto.clj"), "got {}", uri);

    let (decl_line, _) = position_of(&proto, "run-task");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
}

#[test]
fn test_e2e_definition_on_defmethod() {
    // goto-def on a `defmethod` head navigates to the `defmulti` declaration in
    // another namespace — the multimethod analog of the protocol-impl case.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let def = root.join("src/multi_def.clj");
    std::fs::write(&def, "(ns app.multi)\n(defmulti area :kind)\n").unwrap();
    let impl_file = root.join("src/multi_impl.clj");
    std::fs::write(
        &impl_file,
        "(ns app.impl\n  (:require [app.multi :as m]))\n(defmethod m/area :circle [x] (:r x))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&impl_file);

    let (line, ch) = position_of(&impl_file, "m/area");
    let result = client.goto_definition(&impl_file, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for m/area: {}", result));
    assert!(uri.ends_with("/src/multi_def.clj"), "got {}", uri);

    let (decl_line, _) = position_of(&def, "area");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
}

#[test]
fn test_e2e_definition_on_defmethod_letgo() {
    // Same as above for a let-go (`.lg`) project — the fix is dialect-agnostic.
    let project = setup_named("letgo_core_project");
    let root = project.path().canonicalize().unwrap();
    // Hermetic: empty LGX_HOME so no real let-go core is indexed.
    let lgx_home = root.join("lgxhome");

    let def = root.join("src/mdef.lg");
    std::fs::write(&def, "(ns mdef)\n(defmulti area :kind)\n").unwrap();
    let impl_file = root.join("src/mimpl.lg");
    std::fs::write(
        &impl_file,
        "(ns mimpl\n  (:require [mdef :as m]))\n(defmethod m/area :circle [x] (:r x))\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&impl_file);

    let (line, ch) = position_of(&impl_file, "m/area");
    let result = client.goto_definition(&impl_file, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for m/area (lg): {}", result));
    assert!(uri.ends_with("/src/mdef.lg"), "got {}", uri);

    let (decl_line, _) = position_of(&def, "area");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
}

#[test]
fn test_e2e_protocol_impl_wins_over_colliding_def() {
    // A same-namespace defn shares the impl method's name. Go-to-definition on
    // the *impl* must reach the protocol declaration (the position-specific
    // occurrence), not the colliding local var.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let proto = root.join("src/proto.clj");
    std::fs::write(
        &proto,
        "(ns app.proto)\n(defprotocol Worker\n  (run-task [this job]))\n",
    )
    .unwrap();
    let impl_file = root.join("src/impl.clj");
    std::fs::write(
        &impl_file,
        "(ns app.impl\n  (:require [app.proto :as p]))\n(defn run-task [x] x)\n(defrecord Runner [id]\n  p/Worker\n  (run-task [this job] job))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&impl_file);

    // Target the impl head specifically (`run-task [this …]`), not the defn.
    let (line, ch) = position_of(&impl_file, "run-task [this");
    let result = client.goto_definition(&impl_file, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for colliding impl: {}", result));
    assert!(
        uri.ends_with("/src/proto.clj"),
        "impl should reach the protocol decl, not the local defn; got {}",
        uri
    );
    let (decl_line, _) = position_of(&proto, "run-task");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
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

/// Builds a hermetic JDK `src.zip` from `(entry, java-source)` pairs.
fn make_jdk_src_zip(entries: &[(&str, &str)]) -> tempfile::NamedTempFile {
    let tmp = tempfile::Builder::new().suffix(".zip").tempfile().unwrap();
    let file = std::fs::File::create(tmp.path()).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    for (name, content) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    tmp
}

#[test]
fn test_e2e_java_definition_and_hover() {
    // Hermetic JDK source pointed at by CLJ_PULSE_JDK_SRC, so the test never
    // depends on the box's JDK. `demo.lib.Greeter` is imported; `java.lang.Sample`
    // resolves without an import (auto-`java.lang`).
    let src_zip = make_jdk_src_zip(&[
        (
            "java.base/demo/lib/Greeter.java",
            "package demo.lib;\n/** A greeter. */\npublic class Greeter {\n  \
             /** Greet by name. */\n  public static String greet(String name) { return name; }\n}\n",
        ),
        (
            "java.base/java/lang/Sample.java",
            "package java.lang;\npublic class Sample {\n  \
             public static Sample of(long n) { return null; }\n}\n",
        ),
    ]);

    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let probe = root.join("src/javaprobe.clj");
    std::fs::write(
        &probe,
        "(ns simple.javaprobe\n  (:import [demo.lib Greeter]))\n\n\
         (defn g [n] (Greeter/greet n))\n\n(defn s [] (Sample/of 1))\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("CLJ_PULSE_JDK_SRC", src_zip.path())]);
    client.initialize(&root);
    client.wait_for_log("JDK source indexed");
    client.did_open(&probe);

    // Static-member navigation lands in the src.zip Greeter.java.
    let (line, ch) = position_of(&probe, "Greeter/greet");
    let def = client.goto_definition(&probe, line, ch);
    let uri = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("expected Location, got {def}"));
    assert!(
        uri.contains(".zip!/") && uri.ends_with("Greeter.java"),
        "expected src.zip Greeter.java, got {uri}"
    );

    // Hover shows the Java signature and Javadoc.
    let hov = client.hover(&probe, line, ch);
    let value = hov["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("hover null: {hov}"));
    assert!(
        value.contains("greet(String name)"),
        "signature missing: {value}"
    );
    assert!(value.contains("Greet by name"), "javadoc missing: {value}");

    // Auto-imported java.lang resolves without an explicit :import.
    let (sline, sch) = position_of(&probe, "Sample/of");
    let sdef = client.goto_definition(&probe, sline, sch);
    let suri = sdef["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("expected Location, got {sdef}"));
    assert!(
        suri.ends_with("Sample.java"),
        "expected Sample.java, got {suri}"
    );
}

#[test]
fn test_e2e_java_completion_and_signature() {
    let src_zip = make_jdk_src_zip(&[(
        "java.base/demo/lib/Greeter.java",
        "package demo.lib;\npublic class Greeter {\n  public Greeter(int seed) {}\n  \
         public static String greet(String name) { return name; }\n}\n",
    )]);

    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let probe = root.join("src/jcomp.clj");
    std::fs::write(
        &probe,
        "(ns simple.jcomp\n  (:import [demo.lib Greeter]))\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("CLJ_PULSE_JDK_SRC", src_zip.path())]);
    client.initialize(&root);
    client.wait_for_log("JDK source indexed");
    client.did_open(&probe);

    let base = std::fs::read_to_string(&probe).unwrap().lines().count() as u32;

    // Static-member completion: `Greeter/g` → greet (no paren needed).
    client.did_change_insert(&probe, base, 0, "Greeter/g\n");
    let comp = client.completion(&probe, base, 9);
    let labels: Vec<&str> = comp
        .as_array()
        .expect("completion array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"greet"),
        "static-member completion: {labels:?}"
    );

    // Class-name completion: PascalCase `Gr` → Greeter.
    client.did_change_insert(&probe, base + 1, 0, "Gr\n");
    let comp2 = client.completion(&probe, base + 1, 2);
    let labels2: Vec<&str> = comp2
        .as_array()
        .expect("completion array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels2.contains(&"Greeter"),
        "class-name completion: {labels2:?}"
    );

    // Signature help: `(Greeter/greet ` → greet(String name).
    client.did_change_insert(&probe, base + 2, 0, "(Greeter/greet \n");
    let sig = client.signature_help(&probe, base + 2, 15);
    assert!(!sig.is_null(), "no signature help");
    let label = sig["signatures"][0]["label"].as_str().unwrap_or("");
    assert!(label.contains("greet(String name)"), "signature: {label}");
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
fn test_e2e_no_diagnostics_on_project_clj() {
    // Opening project.clj must not flag dependency coordinates
    // (`org.clojure/clojure`, `ring/ring-defaults`) as unresolved namespaces —
    // it is a build manifest, not source (like deps.edn / lgx.edn).
    let project = setup_named("lein_project");
    let root = project.path().canonicalize().unwrap();

    let project_clj = root.join("project.clj");
    std::fs::write(
        &project_clj,
        "(defproject app \"0.1.0\"\n  :dependencies [[org.clojure/clojure \"1.11.1\"]\n                 [ring/ring-defaults \"0.3.2\"]])\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.did_open(&project_clj);

    let diags = client.wait_for_diagnostics("/project.clj");
    let list = diags["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        list.is_empty(),
        "expected no diagnostics on project.clj, got {}",
        diags["diagnostics"]
    );
}

#[test]
fn test_e2e_leiningen_navigation_into_m2_jar() {
    // A Leiningen project with no .cpcache: deps are read from project.clj and
    // mapped to JARs under its :local-repo Maven tree. The fixture's
    // project.clj also carries `^{:protect false}` metadata and a `#"user"`
    // regex, proving the masked parser resolves :dependencies regardless.
    let project = setup_named("lein_project");
    let root = project.path().canonicalize().unwrap();

    // Lay down the declared dep [mylib "1.0.0"] at its Maven coordinate inside
    // the hermetic :local-repo (<root>/m2).
    let jar_path = root.join("m2/mylib/mylib/1.0.0/mylib-1.0.0.jar");
    std::fs::create_dir_all(jar_path.parent().unwrap()).unwrap();
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn helper\n  \"Does helping.\"\n  [x]\n  x)\n\n(defn helper-two [x] x)\n")
        .unwrap();
    zip.finish().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let consumer = root.join("src/uses_lib.clj");
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
        "expected u/helper completions from project.clj-resolved JAR, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_project_clj_change_indexes_new_deps() {
    // Editing project.clj while the server runs must re-resolve Leiningen deps
    // (it is a manifest, like deps.edn), not merely re-index it as source.
    let project = setup_named("lein_project");
    let root = project.path().canonicalize().unwrap();

    // The dep JAR exists on disk, but project.clj initially declares nothing.
    let jar_path = root.join("m2/mylib/mylib/1.0.0/mylib-1.0.0.jar");
    std::fs::create_dir_all(jar_path.parent().unwrap()).unwrap();
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n(defn helper [x] x)\n")
        .unwrap();
    zip.finish().unwrap();

    let project_clj = root.join("project.clj");
    std::fs::write(
        &project_clj,
        "(defproject lein-app \"0.1.0\" :local-repo \"m2\" :source-paths [\"src\"])\n",
    )
    .unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n\n(u/helper 42)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    // No deps yet, so the library task logs a warning rather than completion;
    // sync on the project index instead.
    client.wait_for_log("Indexed");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "u/helper");
    assert!(
        client.goto_definition(&consumer, line, ch).is_null(),
        "lib resolved before being declared in project.clj"
    );

    // Declare the dependency and signal the manifest change.
    std::fs::write(
        &project_clj,
        "(defproject lein-app \"0.1.0\" :local-repo \"m2\"\n  :dependencies [[mylib \"1.0.0\"]]\n  :source-paths [\"src\"])\n",
    )
    .unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", project_clj.display()), "type": 2 }] }),
    );

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let result = client.goto_definition(&consumer, line, ch);
        if let Some(uri) = result["uri"].as_str() {
            assert!(
                uri.starts_with("jar:file://") && uri.ends_with("!/mylib/util.clj"),
                "expected jar navigation after project.clj edit, got {}",
                uri
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "new dep not indexed after project.clj change"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
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
fn test_e2e_clean_ns_removes_unused_require() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // scratch.clj requires clojure.string (unused) and simple.helpers (used).
    let scratch = root.join("src/scratch.clj");
    let source = "(ns simple.scratch\n  (:require [clojure.string :as str]\n            \
                  [simple.helpers :as helpers]))\n\n(defn run []\n  (helpers/greet \"hi\"))\n";
    std::fs::write(&scratch, source).unwrap();
    client.did_open(&scratch);

    // VS Code's "Organize Imports" path: a code-action request restricted to
    // the source.organizeImports kind.
    let result = client.code_action_only(&scratch, &["source.organizeImports"]);
    let actions = result.as_array().expect("expected code action array");
    let action = actions
        .iter()
        .find(|a| a["kind"] == json!("source.organizeImports"))
        .expect("expected a clean-namespace source action");
    assert!(
        action["title"]
            .as_str()
            .map(|t| t.contains("Clean"))
            .unwrap_or(false),
        "unexpected title: {}",
        action["title"]
    );

    let edits = action["edit"]["changes"]
        .as_object()
        .expect("expected WorkspaceEdit.changes")
        .values()
        .next()
        .expect("expected edits for the file")
        .as_array()
        .unwrap();
    let cleaned = apply_edits(source, edits);
    assert!(
        !cleaned.contains("clojure.string"),
        "unused require not removed:\n{}",
        cleaned
    );
    assert!(
        cleaned.contains("[simple.helpers :as helpers]"),
        "used require dropped:\n{}",
        cleaned
    );
    assert!(
        cleaned.contains("(helpers/greet \"hi\")"),
        "body changed:\n{}",
        cleaned
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
fn test_e2e_references_find_usage_in_unopened_alias_test_dir() {
    // A usage in test/ declared via an alias :extra-paths must be found at
    // startup, without opening the test file.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    std::fs::write(
        root.join("deps.edn"),
        "{:paths [\"src\"]\n :aliases {:test {:extra-paths [\"test\"]}}\n \
         :deps {org.clojure/clojure {:mvn/version \"1.11.1\"}}}\n",
    )
    .unwrap();
    let test_file = root.join("test/core_test.clj");
    std::fs::create_dir_all(test_file.parent().unwrap()).unwrap();
    std::fs::write(
        &test_file,
        "(ns simple.core-test\n  (:require [simple.core :as core]))\n(core/add 1 2)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let core = root.join("src/core.clj");
    client.did_open(&core); // open ONLY the definition file
    let (line, ch) = position_of(&core, "add");
    let result = client.references(&core, line, ch, false);
    let locs = result.as_array().cloned().unwrap_or_default();
    assert!(
        locs.iter().any(|l| l["uri"]
            .as_str()
            .map(|u| u.ends_with("/test/core_test.clj"))
            .unwrap_or(false)),
        "test/ usage (alias :extra-paths) not found without opening it: {:?}",
        locs
    );
}

#[test]
fn test_e2e_references_find_usage_in_unopened_default_test_dir() {
    // Even when test/ is declared nowhere, the default src/test scan roots
    // index it at startup so its usages are found without opening the file.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    // deps.edn keeps the default :paths ["src"] (no :test alias).
    let test_file = root.join("test/core_test.clj");
    std::fs::create_dir_all(test_file.parent().unwrap()).unwrap();
    std::fs::write(
        &test_file,
        "(ns simple.core-test\n  (:require [simple.core :as core]))\n(core/add 1 2)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let core = root.join("src/core.clj");
    client.did_open(&core);
    let (line, ch) = position_of(&core, "add");
    let result = client.references(&core, line, ch, false);
    let locs = result.as_array().cloned().unwrap_or_default();
    assert!(
        locs.iter().any(|l| l["uri"]
            .as_str()
            .map(|u| u.ends_with("/test/core_test.clj"))
            .unwrap_or(false)),
        "test/ usage (default scan root) not found without opening it: {:?}",
        locs
    );
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

#[test]
fn test_e2e_transitive_definition_jar_to_jar() {
    // Navigate project → mylib.core (a JAR entry), then from *inside* that JAR
    // entry on into its own dependency mylib.util — the transitive hop.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "core/run");
    let core_loc = client.goto_definition(&consumer, line, ch);
    let core_uri = core_loc["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no jar nav into mylib.core: {}", core_loc))
        .to_string();
    assert!(
        core_uri.ends_with("!/mylib/core.clj"),
        "expected jar nav into mylib.core, got {}",
        core_uri
    );

    // Open the JAR entry the way the editor would, then navigate from within it.
    let core_src = client.text_document_content(&core_uri)["text"]
        .as_str()
        .expect("jar content")
        .to_string();
    client.did_open_uri(&core_uri, &core_src);

    let (line, ch) = position_in_text(&core_src, "util/helper");
    let util_loc = client.goto_definition_uri(&core_uri, line, ch);
    let util_uri = util_loc["uri"].as_str().unwrap_or_default();
    assert!(
        util_uri.ends_with("!/mylib/util.clj"),
        "expected transitive nav into mylib.util, got {}",
        util_loc
    );
}

#[test]
fn test_e2e_references_from_inside_library_file() {
    // Find-references invoked from the `helper` declaration inside the JAR file
    // returns the project usage, the lib→lib usage in mylib.core, and the
    // declaration itself.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    // Open both JAR entries as editor docs.
    let (l, c) = position_of(&consumer, "util/helper");
    let util_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let util_src = client.text_document_content(&util_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&util_uri, &util_src);

    let (l, c) = position_of(&consumer, "core/run");
    let core_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let core_src = client.text_document_content(&core_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&core_uri, &core_src);

    let (line, ch) = position_in_text(&util_src, "helper");
    let result = client.references_uri(&util_uri, line, ch, true);
    let uris: Vec<&str> = result
        .as_array()
        .expect("references array")
        .iter()
        .filter_map(|loc| loc["uri"].as_str())
        .collect();

    assert!(
        uris.iter().any(|u| u.ends_with("/src/uses_lib.clj")),
        "expected the project usage, got {:?}",
        uris
    );
    assert!(
        uris.iter().any(|u| u.ends_with("!/mylib/core.clj")),
        "expected the lib→lib usage in mylib.core, got {:?}",
        uris
    );
    assert!(
        uris.iter().any(|u| u.ends_with("!/mylib/util.clj")),
        "expected the declaration in mylib.util, got {:?}",
        uris
    );
}

#[test]
fn test_e2e_hover_from_inside_library_file() {
    // Hovering a symbol inside a JAR file resolves it through that file's own
    // requires and shows its docs.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "core/run");
    let core_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let core_src = client.text_document_content(&core_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&core_uri, &core_src);

    let (line, ch) = position_in_text(&core_src, "util/helper");
    let hover = client.hover_uri(&core_uri, line, ch);
    let value = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        value.contains("helper") && value.contains("mylib.util"),
        "expected hover for mylib.util/helper from inside the JAR, got {}",
        hover
    );
}

#[test]
fn test_e2e_dependency_contents_serves_jar_source() {
    // clojure-lsp's `clojure/dependencyContents` returns the raw entry text for
    // a jar: URI — the request Calva issues to open a navigation target.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "core/run");
    let core_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();

    let contents = client.dependency_contents(&core_uri);
    let text = contents.as_str().unwrap_or_default();
    assert!(
        text.contains("(ns mylib.core") && text.contains("util/helper"),
        "expected raw mylib.core source from dependencyContents, got {:?}",
        contents
    );
}

#[test]
fn test_e2e_definition_from_percent_encoded_jar_uri() {
    // VS Code / Calva re-encode the jar URI before sending didOpen / definition
    // (`file:`→`file%3A`, `!`→`%21`). The server must still resolve from it —
    // this is the real-world Calva failure mode.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "core/run");
    let clean = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let encoded = clean.replace("file:", "file%3A").replace("!/", "%21/");
    assert!(
        encoded.contains("file%3A") && encoded.contains("%21/"),
        "encoding sanity check failed: {}",
        encoded
    );

    let src = client.text_document_content(&clean)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&encoded, &src);

    let (line, ch) = position_in_text(&src, "util/helper");
    let loc = client.goto_definition_uri(&encoded, line, ch);
    assert!(
        loc["uri"]
            .as_str()
            .unwrap_or_default()
            .ends_with("!/mylib/util.clj"),
        "expected nav from a percent-encoded jar buffer, got {}",
        loc
    );
}

#[test]
fn test_e2e_navigate_to_private_fn_in_jar() {
    // Private (`defn-`) functions in library files are navigable from inside the
    // library source. `caller` calls the private `secret` unqualified.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn- secret [x] x)\n\n(defn caller [x] (secret x))\n")
        .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n\n(u/caller 1)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "u/caller");
    let util_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let util_src = client.text_document_content(&util_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&util_uri, &util_src);

    let (line, ch) = position_in_text(&util_src, "(secret x)");
    let loc = client.goto_definition_uri(&util_uri, line, ch);
    assert!(
        loc["uri"]
            .as_str()
            .unwrap_or_default()
            .ends_with("!/mylib/util.clj"),
        "expected nav to a private fn in the lib, got {}",
        loc
    );
    assert_eq!(
        loc["range"]["start"]["line"].as_u64(),
        Some(2),
        "expected the `(defn- secret ...)` line, got {}",
        loc
    );
}

#[test]
fn test_e2e_navigate_into_impl_namespace_from_jar() {
    // Library-internal `.impl` namespaces are indexed, so navigating from one
    // library file into the lib's `.impl` namespace works (the claypoole
    // `impl/validate-future-pool` case).
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/impl.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.impl)\n\n(defn validate [x] x)\n")
        .unwrap();
    zip.start_file("mylib/core.clj", opts).unwrap();
    zip.write_all(
        b"(ns mylib.core\n  (:require [mylib.impl :as impl]))\n\n(defn run [x] (impl/validate x))\n",
    )
    .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.core :as core]))\n\n(core/run 1)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "core/run");
    let core_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let core_src = client.text_document_content(&core_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&core_uri, &core_src);

    let (line, ch) = position_in_text(&core_src, "impl/validate");
    let loc = client.goto_definition_uri(&core_uri, line, ch);
    assert!(
        loc["uri"]
            .as_str()
            .unwrap_or_default()
            .ends_with("!/mylib/impl.clj"),
        "expected nav into the .impl namespace, got {}",
        loc
    );
}

#[test]
fn test_e2e_definition_bare_same_ns_symbol_in_jar() {
    // The reported claypoole case: inside a JAR file, go-to-definition on a
    // BARE, same-namespace symbol (`completable-future-call`), not a qualified
    // cross-ns ref. `caller` calls `helper` unqualified, both in mylib.util.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn helper [x] x)\n\n(defn caller [x] (helper x))\n")
        .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n\n(u/caller 1)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    // Into the JAR, then open it the way the editor would.
    let (l, c) = position_of(&consumer, "u/caller");
    let util_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let util_src = client.text_document_content(&util_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&util_uri, &util_src);

    // Cursor on the bare `helper` inside `(helper x)` → its definition above.
    let (line, ch) = position_in_text(&util_src, "(helper x)");
    let loc = client.goto_definition_uri(&util_uri, line, ch);
    let uri = loc["uri"].as_str().unwrap_or_default();
    assert!(
        uri.ends_with("!/mylib/util.clj"),
        "expected same-file nav for a bare same-ns symbol, got {}",
        loc
    );
    assert_eq!(
        loc["range"]["start"]["line"].as_u64(),
        Some(2),
        "expected the `(defn helper ...)` line, got {}",
        loc
    );
}

#[test]
fn test_e2e_rename_rejected_from_library_file() {
    // Navigation/inspection work from a JAR buffer, but rename must not: a
    // library file is read-only, and the fqn-only resolver could otherwise edit
    // a project symbol that shadows the library one.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "util/helper");
    let util_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let util_src = client.text_document_content(&util_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&util_uri, &util_src);

    let (line, ch) = position_in_text(&util_src, "helper");
    let msg = client.rename_uri(&util_uri, line, ch, "renamed");
    let err = msg
        .get("error")
        .unwrap_or_else(|| panic!("rename from a JAR buffer should be rejected, got {}", msg));
    assert!(
        err["message"]
            .as_str()
            .unwrap_or_default()
            .contains("library file"),
        "expected a library-file rejection, got {}",
        err
    );
}

#[test]
fn test_e2e_integrant_goto_definition_from_config() {
    // The headline feature: from a namespaced keyword in an Integrant
    // `config.edn` system map, navigate to its `(defmethod ig/init-key ::db …)`.
    let project = setup_named("integrant_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let config = root.join("resources/config.edn");
    let db = root.join("src/readx/db.clj");
    client.did_open(&config);

    // Cursor on the `:readx.db/db` map key.
    let (line, ch) = position_of(&config, ":readx.db/db");
    let result = client.goto_definition(&config, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for :readx.db/db: {}", result));
    assert!(uri.ends_with("/src/readx/db.clj"), "got {}", uri);

    // Lands on the ig/init-key defmethod line — not assert-key or halt-key!.
    let (init_line, _) = position_of(&db, "ig/init-key");
    assert_eq!(result["range"]["start"]["line"], json!(init_line));
}

#[test]
fn test_e2e_integrant_references_span_defmethods_and_config() {
    // References on the component keyword reach every lifecycle defmethod plus
    // the config-map key and the `#ig/ref`.
    let project = setup_named("integrant_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let db = root.join("src/readx/db.clj");
    client.did_open(&db);

    // First `::db` in db.clj is the assert-key dispatch (an occurrence); it
    // resolves to the same `:readx.db/db` as the init-key definition.
    let (line, ch) = position_of(&db, "::db");
    let result = client.references(&db, line, ch, true);
    let locs = result
        .as_array()
        .unwrap_or_else(|| panic!("references returned null: {}", result));

    // 3 in db.clj (init-key declaration + assert-key + halt-key! occurrences),
    // 2 in resources/config.edn (the map key + the `#ig/ref` value).
    assert_eq!(locs.len(), 5, "locs: {:?}", locs);
    let uris: Vec<&str> = locs.iter().filter_map(|l| l["uri"].as_str()).collect();
    assert_eq!(
        uris.iter()
            .filter(|u| u.ends_with("/src/readx/db.clj"))
            .count(),
        3,
        "db.clj locations: {:?}",
        locs
    );
    assert_eq!(
        uris.iter()
            .filter(|u| u.ends_with("/resources/config.edn"))
            .count(),
        2,
        "config.edn locations: {:?}",
        locs
    );
}

#[test]
fn test_e2e_keyword_does_not_navigate_to_same_named_var() {
    // A namespaced keyword must never goto-def to a same-named var. With no
    // keyword definition, goto-def yields nothing rather than a wrong jump
    // (regression: `::counter` used to land on `(defn- counter …)`).
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let f = root.join("src/metrics.clj");
    std::fs::write(
        &f,
        "(ns app.metrics)\n(defn- counter [] 1)\n(def m {::counter (counter)})\n(::counter m)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&f);

    // Cursor on the `::counter` keyword (the map key on line 2).
    let (line, ch) = position_of(&f, "::counter");
    let result = client.goto_definition(&f, line, ch);
    assert!(
        result.is_null(),
        "keyword must not navigate to the same-named var, got {}",
        result
    );
}

#[test]
fn test_e2e_unqualified_keyword_does_not_navigate_to_var() {
    // Same guard for an *unqualified* keyword: `:counter` must not goto-def to
    // a same-named var. (resolve_fqn_at returns nothing for unqualified
    // keywords, so this relies on the keyword-token check, not the fqn.)
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let f = root.join("src/unq.clj");
    std::fs::write(
        &f,
        "(ns app.unq)\n(defn- counter [] 1)\n(def m {:counter (counter)})\n(:counter m)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&f);

    // Cursor on an unqualified `:counter` keyword (the first is the map key).
    let (line, ch) = position_of(&f, ":counter");
    let result = client.goto_definition(&f, line, ch);
    assert!(
        result.is_null(),
        "unqualified keyword must not navigate to the same-named var, got {}",
        result
    );
}

#[test]
fn test_e2e_keyword_navigation_with_cursor_on_colon() {
    // The whole-keyword nav must work even with the cursor on the leading `:`
    // of the keyword, not just on the name part.
    let project = setup_named("integrant_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let config = root.join("resources/config.edn");
    client.did_open(&config);

    // Column of the leading ':' of `:readx.db/db` (its first appearance).
    let text = std::fs::read_to_string(&config).unwrap();
    let (line, col) = text
        .lines()
        .enumerate()
        .find_map(|(i, l)| l.find(":readx.db/db").map(|c| (i as u32, c as u32)))
        .expect(":readx.db/db not found");

    let result = client.goto_definition(&config, line, col);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("cursor on the ':' should still resolve, got {}", result));
    assert!(uri.ends_with("/src/readx/db.clj"), "got {}", uri);
}

#[test]
fn test_e2e_integrant_refless_config_navigates() {
    // A ref-less Integrant config (no #ig/ref anywhere) must still navigate from
    // a component key to its `ig/init-key` defmethod.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let db = root.join("src/sys/db.clj");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(
        &db,
        "(ns sys.db\n  (:require [integrant.core :as ig]))\n(defmethod ig/init-key ::conn [_ o] o)\n",
    )
    .unwrap();
    let cfg = root.join("resources/sys.edn");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, "{:sys.db/conn {:url \"x\"}}\n").unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&cfg);

    let (line, ch) = position_of(&cfg, ":sys.db/conn");
    let result = client.goto_definition(&cfg, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("ref-less config should navigate, got {}", result));
    assert!(uri.ends_with("/src/sys/db.clj"), "got {}", uri);
}

#[test]
fn test_e2e_watched_edn_config_keeps_references_fresh() {
    // An Integrant config edited outside the editor (git pull / branch switch)
    // is re-indexed via the file watcher, so references reflect the new keys.
    let project = setup_named("integrant_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let db = root.join("src/readx/db.clj");
    let config = root.join("resources/config.edn");
    client.did_open(&db);

    // Baseline: 5 references (3 in db.clj + 2 in config.edn).
    let (line, ch) = position_of(&db, "::db");
    assert_eq!(
        client
            .references(&db, line, ch, true)
            .as_array()
            .unwrap()
            .len(),
        5,
        "baseline references"
    );

    // Rewrite config.edn (no editor save) to drop every :readx.db/db use.
    std::fs::write(&config, "{:other/x {:v 1}\n :sys {:y #ig/ref :other/x}}\n").unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", config.display()), "type": 2 }] }),
    );

    // Poll until the stale config.edn occurrences are gone: only the 3 db.clj
    // locations remain.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let n = client
            .references(&db, line, ch, true)
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        if n == 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "references did not refresh after watched EDN change (still {})",
            n
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_e2e_references_spec_keyword_def_and_usage() {
    // find-references on a clojure.spec keyword spans its `s/def` site and every
    // `:req-un`/usage — mirrors tickets/handlers.clj.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let f = root.join("src/handlers.clj");
    std::fs::write(
        &f,
        "(ns app.handlers\n  (:require [clojure.spec.alpha :as s]))\n\
         (s/def :ticket/id integer?)\n\
         (s/def ::ticket-out\n  (s/keys :req-un [:ticket/id]))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&f);

    // From the `s/def` site, and again from the `:req-un` usage — both resolve
    // to the same two locations.
    for needle in [":ticket/id", "[:ticket/id]"] {
        let (line, ch) = position_of(&f, needle);
        let result = client.references(&f, line, ch, true);
        let locs = result
            .as_array()
            .unwrap_or_else(|| panic!("references null from {:?}: {}", needle, result));
        assert_eq!(locs.len(), 2, "from {:?}: {:?}", needle, locs);
    }
}

#[test]
fn test_e2e_zed_client_cross_file_definition() {
    // Under a Zed-shaped init (workspaceFolders, no rootUri), cross-file
    // goto-definition must resolve — the regression that broke Zed navigation.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize_zed(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let def = client.goto_definition(&utils, line, ch);
    let uri = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("zed cross-file definition failed: {}", def));
    assert!(uri.ends_with("/src/core.clj"), "got {}", uri);
}

#[test]
fn test_e2e_zed_client_hover_and_completion() {
    // Hover and completion must work under a Zed-shaped client — both need the
    // project (not just the open file) indexed.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize_zed(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);
    let (line, ch) = position_of(&utils, "core/add");

    // Cross-file docstring on hover.
    let hov = client.hover(&utils, line, ch);
    let val = hov["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("zed hover returned null: {}", hov));
    assert!(val.contains("Adds two numbers."), "zed hover doc: {}", val);

    // Alias-prefixed completion.
    let comp = client.completion(&utils, line, ch);
    let labels: Vec<&str> = comp
        .as_array()
        .unwrap_or_else(|| panic!("zed completion returned null: {}", comp))
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(labels.contains(&"core/add"), "zed completion: {:?}", labels);
}

#[test]
fn test_e2e_zed_client_cross_file_references() {
    // find-references across files must work under a Zed-shaped client.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize_zed(&root);

    let core = root.join("src/core.clj");
    client.did_open(&core);

    // `add` is defined in core.clj and used as `core/add` in utils.clj.
    let (line, ch) = position_of(&core, "add");
    let refs = client.references(&core, line, ch, true);
    let locs = refs
        .as_array()
        .unwrap_or_else(|| panic!("zed references returned null: {}", refs));
    assert_eq!(locs.len(), 2, "decl + cross-file usage: {:?}", locs);
    let uris: Vec<&str> = locs.iter().filter_map(|l| l["uri"].as_str()).collect();
    assert!(uris.iter().any(|u| u.ends_with("/src/core.clj")));
    assert!(uris.iter().any(|u| u.ends_with("/src/utils.clj")));
}
