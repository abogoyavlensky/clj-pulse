//! The shared LSP test client: spawns the real `clj-pulse` binary and speaks
//! LSP over stdio with Content-Length framing, the same way VS Code/Calva
//! drives it. Used by `test_e2e.rs` and by the ignored bench in `test_bench.rs`.
//!
//! Both test binaries compile this module, and neither uses all of it, so
//! `dead_code` is off here rather than per item.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const TIMEOUT: Duration = Duration::from_secs(20);

/// Which `clj-kondo`, if any, the server under test may find.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kondo {
    /// Kill-switch on: the bridge is inert, as for every non-kondo test.
    Off,
    /// The committed fake, first on PATH.
    Fake,
    /// Whatever the host has installed (ignored tests only).
    Real,
}

pub struct LspClient {
    pub child: Child,
    pub stdin: ChildStdin,
    pub incoming: Receiver<Value>,
    pub notifications: Vec<Value>,
    pub next_id: i64,
}

impl LspClient {
    /// Spawns the server binary with `cwd` set to the project root,
    /// mirroring how an editor launches it.
    pub fn start(project_root: &Path) -> Self {
        Self::start_with_env(project_root, &[])
    }

    /// Like [`start`] but with stage-3 classpath resolution left enabled —
    /// only for tests that exercise the `clojure -Spath` flow.
    pub fn start_with_classpath_cli(project_root: &Path) -> Self {
        Self::spawn(project_root, &[], false, Kondo::Off)
    }

    /// Like [`start`] but sets extra environment variables on the server
    /// process (e.g. `LGX_HOME` for hermetic lgx dep resolution).
    pub fn start_with_env(project_root: &Path, envs: &[(&str, &Path)]) -> Self {
        Self::spawn(project_root, envs, true, Kondo::Off)
    }

    /// [`start_with_env`] for variables whose value is a plain string rather
    /// than a path (`CLJ_PULSE_TEST_PANIC`).
    pub fn start_with_str_env(project_root: &Path, envs: &[(&str, &str)]) -> Self {
        let owned: Vec<(&str, std::path::PathBuf)> = envs
            .iter()
            .map(|(k, v)| (*k, std::path::PathBuf::from(v)))
            .collect();
        let borrowed: Vec<(&str, &Path)> = owned.iter().map(|(k, v)| (*k, v.as_path())).collect();
        Self::spawn(project_root, &borrowed, true, Kondo::Off)
    }

    /// Like [`start`] but with the clj-kondo bridge live, answered by the
    /// committed fake binary rather than whatever the host happens to have
    /// installed — so these tests assert on fixed findings and never wait on
    /// a JVM.
    pub fn start_with_kondo(project_root: &Path) -> Self {
        Self::spawn(project_root, &[], true, Kondo::Fake)
    }

    /// [`start_with_kondo`] with extra environment variables — `FAKE_KONDO_LOG`,
    /// the file the fake records its cache-warming invocations in.
    pub fn start_with_kondo_env(project_root: &Path, envs: &[(&str, &Path)]) -> Self {
        Self::spawn(project_root, envs, true, Kondo::Fake)
    }

    /// Production settings: neither `CLJ_PULSE_DISABLE_CLASSPATH_CLI` nor
    /// `CLJ_PULSE_DISABLE_KONDO` is set, so stage-3 classpath resolution runs
    /// and clj-kondo is spawned when the host has it. **For the bench only** —
    /// a regular e2e test using this would spawn `clojure` and behave
    /// differently on a machine with clj-kondo installed than on one without.
    pub fn start_production(project_root: &Path) -> Self {
        Self::spawn(project_root, &[], false, Kondo::Real)
    }

    /// Like [`start_with_kondo`] but resolving `clj-kondo` from the host's own
    /// PATH — the real binary, for the ignored smoke test.
    pub fn start_with_real_kondo(project_root: &Path) -> Self {
        Self::spawn(project_root, &[], true, Kondo::Real)
    }

    /// The directory holding the fake `clj-kondo`, prepended to the server's
    /// PATH by [`start_with_kondo`].
    pub fn fake_kondo_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-clj-kondo")
    }

    pub fn spawn(
        project_root: &Path,
        envs: &[(&str, &Path)],
        disable_classpath_cli: bool,
        kondo: Kondo,
    ) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clj-pulse"));
        cmd.current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if disable_classpath_cli {
            // Fixtures carry a deps.edn; without this every test would spawn
            // `clojure` for stage-3 classpath resolution.
            cmd.env("CLJ_PULSE_DISABLE_CLASSPATH_CLI", "1");
        } else {
            // Strip an inherited kill-switch too — the stage-3 tests must not
            // be silently neutered by the parent environment.
            cmd.env_remove("CLJ_PULSE_DISABLE_CLASSPATH_CLI");
        }
        match kondo {
            // No fixture may depend on a clj-kondo installed on the host: the
            // whole suite would then behave differently per machine.
            Kondo::Off => {
                cmd.env("CLJ_PULSE_DISABLE_KONDO", "1");
            }
            // Discovery goes through PATH, so putting the fake first is all it
            // takes — the server has no test-only code path.
            Kondo::Fake => {
                cmd.env_remove("CLJ_PULSE_DISABLE_KONDO");
                let inherited = std::env::var_os("PATH").unwrap_or_default();
                let mut dirs = vec![Self::fake_kondo_dir()];
                dirs.extend(std::env::split_paths(&inherited));
                cmd.env("PATH", std::env::join_paths(dirs).unwrap());
            }
            // The host's own binary, inherited PATH untouched.
            Kondo::Real => {
                cmd.env_remove("CLJ_PULSE_DISABLE_KONDO");
            }
        }
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

    pub fn send(&mut self, msg: Value) {
        let body = serde_json::to_string(&msg).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Sends a request and blocks until its response arrives.
    /// Server-initiated messages received in the meantime are stashed.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let msg = self.request_full(method, params);
        if let Some(err) = msg.get("error") {
            panic!("{} returned error: {}", method, err);
        }
        msg["result"].clone()
    }

    /// Like `request` but expects a JSON-RPC error and returns it.
    pub fn request_expect_error(&mut self, method: &str, params: Value) -> Value {
        let msg = self.request_full(method, params);
        msg.get("error")
            .unwrap_or_else(|| panic!("{} unexpectedly succeeded: {}", method, msg))
            .clone()
    }

    /// Sends a request under a caller-chosen id, so a test can reuse one.
    /// Returns the raw JSON-RPC message.
    pub fn request_with_id(&mut self, id: i64, method: &str, params: Value) -> Value {
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

    pub fn request_full(&mut self, method: &str, params: Value) -> Value {
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
    pub fn stash(&mut self, msg: Value) {
        if let (Some(id), Some(_)) = (msg.get("id").cloned(), msg.get("method")) {
            self.send(json!({ "jsonrpc": "2.0", "id": id, "result": null }));
        }
        self.notifications.push(msg);
    }

    /// Waits until a `window/logMessage` whose text contains `needle` has
    /// been received (checks already-stashed notifications first).
    pub fn wait_for_log(&mut self, needle: &str) {
        self.wait_for_log_within(needle, TIMEOUT);
    }

    /// [`wait_for_log`] with a custom deadline — for waits that legitimately
    /// exceed the harness default, like a cold-network dependency download.
    pub fn wait_for_log_within(&mut self, needle: &str, timeout: Duration) {
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
        let deadline = Instant::now() + timeout;
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

    /// [`initialize`] without the trailing wait, so a caller can time the
    /// indexing stages itself (the bench).
    pub fn initialize_no_wait(&mut self, root: &Path) -> Value {
        let root_uri = format!("file://{}", root.display());
        let result = self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": "bench" }],
                "capabilities": {
                    "textDocument": { "definition": { "linkSupport": true } },
                    "general": { "positionEncodings": ["utf-16"] }
                }
            }),
        );
        self.notify("initialized", json!({}));
        result
    }

    /// The text of the first `window/logMessage` containing any of `needles`,
    /// waiting up to `timeout` for it. `None` on timeout — the bench reports a
    /// missing stage rather than failing the run.
    pub fn log_line_within(&mut self, needles: &[&str], timeout: Duration) -> Option<String> {
        let text = |m: &Value| {
            m["params"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };
        let matches = |m: &Value| {
            m["method"] == "window/logMessage" && needles.iter().any(|n| text(m).contains(n))
        };
        if let Some(m) = self.notifications.iter().find(|m| matches(m)) {
            return Some(text(m));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let msg = self.incoming.recv_timeout(remaining).ok()?;
            let found = matches(&msg);
            let line = text(&msg);
            self.stash(msg);
            if found {
                return Some(line);
            }
        }
    }

    /// Full editor-style startup: initialize (with rootUri), initialized,
    /// then wait for project indexing to finish.
    pub fn initialize(&mut self, root: &Path) -> Value {
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

    /// Like [`initialize`] but with the given `initializationOptions` — what
    /// Clojure Pulse sends (`{"projects": …, "kondo": …, "clojuredocs": …}`).
    pub fn initialize_with_options(&mut self, root: &Path, options: Value) -> Value {
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
                "initializationOptions": options
            }),
        );
        self.notify("initialized", json!({}));
        self.wait_for_log("Indexed");
        result
    }

    /// `clojurePulse/clojureDocs`, returning the raw JSON-RPC message so a
    /// test can assert on either `result` or `error`.
    pub fn clojure_docs(&mut self, params: Value) -> Value {
        self.request_full("clojurePulse/clojureDocs", params)
    }

    /// Like [`initialize`] but advertising `window.workDoneProgress`, so the
    /// server may report `$/progress`.
    pub fn initialize_with_progress(&mut self, root: &Path) -> Value {
        let root_uri = format!("file://{}", root.display());
        let result = self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": "fixture" }],
                "capabilities": {
                    "textDocument": { "definition": { "linkSupport": true } },
                    "general": { "positionEncodings": ["utf-16"] },
                    "window": { "workDoneProgress": true }
                }
            }),
        );
        self.notify("initialized", json!({}));
        self.wait_for_log("Indexed");
        result
    }

    /// Zed-shaped startup: only `workspaceFolders` (no deprecated `rootUri`),
    /// offering UTF-8 then UTF-16 position encodings — what Zed's LSP client
    /// sends. Exercises the same indexing path real Zed users hit.
    pub fn initialize_zed(&mut self, root: &Path) -> Value {
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

    pub fn did_open(&mut self, path: &Path) {
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

    pub fn goto_definition(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character }
            }),
        )
    }

    pub fn hover(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character }
            }),
        )
    }

    pub fn completion(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character }
            }),
        )
    }

    /// The `items` of a completion response. The server answers with a
    /// `CompletionList` (`isIncomplete: true`), so tests read through `items`;
    /// a null response stays null.
    pub fn completion_items(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.completion(path, line, character)["items"].clone()
    }

    /// `completionItem/resolve` for one item of a completion response.
    pub fn completion_resolve(&mut self, item: Value) -> Value {
        self.request("completionItem/resolve", item)
    }

    /// Incremental edit: inserts `text` at (line, character), version bump.
    pub fn did_change_insert(&mut self, path: &Path, line: u32, character: u32, text: &str) {
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

    /// Incremental edit over an arbitrary range, so a test can send one the
    /// document does not have. [`did_change_insert`] can only express a
    /// zero-width range at a position.
    pub fn did_change_range(
        &mut self,
        path: &Path,
        version: i64,
        start: (u32, u32),
        end: (u32, u32),
        text: &str,
    ) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {
                    "uri": format!("file://{}", path.display()),
                    "version": version
                },
                "contentChanges": [{
                    "range": {
                        "start": { "line": start.0, "character": start.1 },
                        "end": { "line": end.0, "character": end.1 }
                    },
                    "text": text
                }]
            }),
        )
    }

    pub fn text_document_content(&mut self, uri: &str) -> Value {
        self.request("workspace/textDocumentContent", json!({ "uri": uri }))
    }

    /// clojure-lsp's custom jar content request (what Calva calls). Returns the
    /// raw content string.
    pub fn dependency_contents(&mut self, uri: &str) -> Value {
        self.request("clojure/dependencyContents", json!({ "uri": uri }))
    }

    pub fn signature_help(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/signatureHelp",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character }
            }),
        )
    }

    pub fn document_symbols(&mut self, path: &Path) -> Value {
        self.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": format!("file://{}", path.display()) } }),
        )
    }

    pub fn ignored_forms(&mut self, path: &Path) -> Value {
        self.request(
            "clojurePulse/ignoredForms",
            json!({ "uri": format!("file://{}", path.display()) }),
        )
    }

    pub fn on_type_formatting(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
        ch: &str,
    ) -> Value {
        self.request(
            "textDocument/onTypeFormatting",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character },
                "ch": ch,
                "options": { "tabSize": 2, "insertSpaces": true }
            }),
        )
    }

    pub fn workspace_symbols(&mut self, query: &str) -> Value {
        self.request("workspace/symbol", json!({ "query": query }))
    }

    pub fn code_action(&mut self, path: &Path, line: u32, character: u32) -> Value {
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
    pub fn code_action_only(&mut self, path: &Path, only: &[&str]) -> Value {
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
    pub fn code_action_for_diagnostic(&mut self, path: &Path, diagnostic: &Value) -> Value {
        self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "range": diagnostic["range"].clone(),
                "context": { "diagnostics": [diagnostic.clone()] }
            }),
        )
    }

    /// Drops every stashed server message, so a following `wait_for_*` can
    /// only be satisfied by something that arrives afterwards. Needed when a
    /// test asserts on a *second* publish for a document it already saw one
    /// for — otherwise the stash answers instantly with the stale one.
    pub fn clear_notifications(&mut self) {
        // Drain what is already in flight before dropping the stash: a message
        // sitting unread in the channel is exactly as stale as one already
        // stashed, and would otherwise satisfy the next `wait_for_*`. Draining
        // through `stash` keeps answering server→client requests.
        while let Ok(msg) = self.incoming.try_recv() {
            self.stash(msg);
        }
        self.notifications.clear();
    }

    /// Waits for a `textDocument/publishDiagnostics` whose uri ends with
    /// `uri_suffix` and returns its `params` (checks already-stashed first).
    pub fn wait_for_diagnostics(&mut self, uri_suffix: &str) -> Value {
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

    pub fn references(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
        include_decl: bool,
    ) -> Value {
        self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": include_decl }
            }),
        )
    }

    pub fn prepare_rename(&mut self, path: &Path, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/prepareRename",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character }
            }),
        )
    }

    pub fn prepare_rename_error(&mut self, path: &Path, line: u32, character: u32) -> String {
        let error = self.request_expect_error(
            "textDocument/prepareRename",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": character }
            }),
        );
        error["message"].as_str().unwrap().to_string()
    }

    pub fn rename(&mut self, path: &Path, line: u32, character: u32, new_name: &str) -> Value {
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

    pub fn did_open_uri(&mut self, uri: &str, text: &str) {
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

    pub fn goto_definition_uri(&mut self, uri: &str, line: u32, character: u32) -> Value {
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
    }

    pub fn references_uri(
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

    pub fn hover_uri(&mut self, uri: &str, line: u32, character: u32) -> Value {
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
    pub fn rename_uri(&mut self, uri: &str, line: u32, character: u32, new_name: &str) -> Value {
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
pub fn setup_project() -> tempfile::TempDir {
    setup_named("simple_project")
}

pub fn setup_named(name: &str) -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    copy_dir(&src, tmp.path());
    tmp
}

pub fn copy_dir(from: &Path, to: &Path) {
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
pub fn position_of(path: &Path, needle: &str) -> (u32, u32) {
    let text = std::fs::read_to_string(path).unwrap();
    for (i, line) in text.lines().enumerate() {
        if let Some(col) = line.find(needle) {
            return (i as u32, (col + needle.len() / 2) as u32);
        }
    }
    panic!("{:?} not found in {}", needle, path.display());
}

/// The (line, character) of the *start* of the first occurrence of `needle`
/// (ASCII), for asserting a binding-site range exactly.
pub fn start_of(text: &str, needle: &str) -> (u32, u32) {
    for (i, line) in text.lines().enumerate() {
        if let Some(col) = line.find(needle) {
            return (i as u32, col as u32);
        }
    }
    panic!("{:?} not found in text", needle);
}

/// Like [`position_of`] but over an in-memory string — JAR content is served
/// from the archive, not from a file on disk.
pub fn position_in_text(text: &str, needle: &str) -> (u32, u32) {
    for (i, line) in text.lines().enumerate() {
        if let Some(col) = line.find(needle) {
            return (i as u32, (col + needle.len() / 2) as u32);
        }
    }
    panic!("{:?} not found in text", needle);
}
