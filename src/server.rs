use std::sync::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::classpath;
use crate::config;
use crate::document::DocumentStore;
use crate::handlers;
use crate::index::extractor;
use crate::index::scanner;
use crate::index::Index;
use crate::jar_content;
use crate::leiningen;
use crate::lgx;
use crate::libraries;
use crate::settings;

/// What [`resolve_and_index_libs`] indexed: the classpath entries / dep dirs,
/// plus library sources indexed outside them (let-go's pinned core stdlib —
/// a pinned project with no deps of its own must still count as indexed).
struct ResolvedLibs {
    entries: Vec<std::path::PathBuf>,
    extra: usize,
}

impl ResolvedLibs {
    fn indexed_any(&self) -> bool {
        !self.entries.is_empty() || self.extra > 0
    }
}

/// Resolves and indexes a project's libraries: lgx git/local deps (indexed as
/// source dirs, including in-workspace `:local/root` deps) for let-go projects,
/// or the `.cpcache` classpath (JARs + dirs) for Clojure projects. When there
/// is no usable `.cpcache` but a Leiningen `project.clj` is present, falls back
/// to resolving its direct deps to `~/.m2` JARs.
fn resolve_and_index_libs(root: &std::path::Path, index: &Index) -> ResolvedLibs {
    match config::project_kind(root) {
        config::ProjectKind::LetGo => {
            let dirs = lgx::resolve(root);
            scanner::index_dir_libs(&dirs, index);
            // Also index let-go's built-in core/stdlib from the source `lgx
            // install` fetched (only when `:lg-version` is pinned).
            let extra = lgx::index_letgo_core(root, index);
            ResolvedLibs {
                entries: dirs,
                extra,
            }
        }
        config::ProjectKind::Clojure => {
            let classpath = clojure_classpath(root);
            if !classpath.is_empty() {
                scanner::index_classpath_libs(root, classpath.clone(), index);
            }
            ResolvedLibs {
                entries: classpath,
                extra: 0,
            }
        }
    }
}

/// Shared record of the classpath entries the library index was last built
/// from. Stage 3 and the config-watcher rerun compare against it to skip
/// no-op re-indexing; re-reading `.cpcache` instead would observe the `.cp`
/// file `clojure -Spath` itself just wrote and always conclude "no change".
type LibEntries = Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>;

/// Serializes stage-3 runs. Startup resolution and a config-watcher rerun can
/// overlap (a cold resolve may download for minutes); without the lock the
/// slower run — possibly carrying obsolete aliases — would clear and rebuild
/// the index last, overwriting the newer result.
type ClasspathCliLock = Arc<tokio::sync::Mutex<()>>;

/// Stage 3: resolves the authoritative classpath by running the clojure CLI
/// (`clojure -A:dev:test -Spath` by default) and re-indexes libraries when the
/// result differs from what is already indexed. Returns whether resolution
/// succeeded; every failure — including `:enabled false` — only logs at most,
/// keeping the stage-2 index intact.
///
/// The `:classpath` config is read *under the lock*, so a run queued behind a
/// slow one always applies the freshest alias selection.
async fn run_classpath_cli(
    root: &std::path::Path,
    index: &Index,
    client: &Client,
    lib_entries: &LibEntries,
    cli_lock: &ClasspathCliLock,
) -> bool {
    let _serial = cli_lock.lock().await;
    // Interim single-project read of the `:projects` config: resolve just the
    // root project's `:enabled`/`:cmd`. The staged multi-project startup
    // replaces this whole flow.
    let file_cfg = std::fs::read_to_string(root.join(".clj-pulse").join("config.edn"))
        .map(|src| crate::projects::parse_edn(&src))
        .unwrap_or_default();
    let root_project = crate::projects::resolve(root, &[], &file_cfg, &[])
        .into_iter()
        .next();
    let Some(project) = root_project else {
        return false;
    };
    if !project.classpath_enabled {
        return false;
    }
    let Some(cmd) = project.classpath_cmd else {
        return false;
    };
    let msg = format!("clj-pulse: resolving classpath via '{cmd}' (may download dependencies)...");
    tracing::info!("{}", msg);
    client.log_message(MessageType::INFO, msg).await;

    match classpath::resolve_via_cmd(&cmd, root, classpath::CMD_TIMEOUT).await {
        Ok(entries) => {
            let set: std::collections::HashSet<std::path::PathBuf> =
                entries.iter().cloned().collect();
            let changed = *lib_entries.lock().unwrap() != set;
            if changed {
                index.clear_libs();
                scanner::index_classpath_libs(root, entries.clone(), index);
                *lib_entries.lock().unwrap() = set;
            }
            let msg = format!(
                "clj-pulse: full classpath indexed ({} entries)",
                entries.len()
            );
            tracing::info!("{}", msg);
            client.log_message(MessageType::INFO, msg).await;
            if changed {
                client.send_notification::<LibrariesChanged>(()).await;
            }
            true
        }
        Err(reason) => {
            let msg = format!("clj-pulse: classpath resolution failed: {reason}");
            tracing::warn!("{}", msg);
            client.log_message(MessageType::WARNING, msg).await;
            false
        }
    }
}

/// A Clojure project's classpath: deps.edn's `.cpcache` is authoritative (full
/// transitive classpath); only when it is empty do we consult a Leiningen
/// `project.clj` for its direct dependencies.
fn clojure_classpath(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut classpath = classpath::discover(root);
    if classpath.is_empty() && root.join("project.clj").exists() {
        classpath = leiningen::resolve(root);
    }
    classpath
}

/// The library classpath entries for the External Libraries panel, derived the
/// same way `resolve_and_index_libs` does but without indexing. For let-go this
/// is the resolved dependency source dirs; the built-in core stdlib is
/// intentionally excluded (mirroring JDK being out of the panel's scope).
fn resolve_lib_entries(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    match config::project_kind(root) {
        config::ProjectKind::LetGo => lgx::resolve(root),
        config::ProjectKind::Clojure => clojure_classpath(root),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct TextDocumentContentParams {
    uri: String,
}

#[derive(serde::Serialize)]
pub(crate) struct TextDocumentContentResult {
    text: String,
}

#[derive(serde::Deserialize)]
pub(crate) struct IgnoredFormsParams {
    uri: String,
}

#[derive(serde::Deserialize)]
pub(crate) struct LibraryEntriesParams {
    path: String,
}

/// clj-pulse custom notification, pushed whenever library (re)indexing
/// completes — including the zero-entries case, so the External Libraries panel
/// refreshes (and clears when deps disappear) without polling. Zero params.
enum LibrariesChanged {}

impl tower_lsp::lsp_types::notification::Notification for LibrariesChanged {
    type Params = ();
    const METHOD: &'static str = "clojurePulse/librariesChanged";
}

pub struct Backend {
    pub client: Client,
    pub index: Arc<Index>,
    pub documents: Arc<DocumentStore>,
    root: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// See [`LibEntries`].
    lib_entries: LibEntries,
    /// See [`ClasspathCliLock`].
    classpath_cli_lock: ClasspathCliLock,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            index: Arc::new(Index::new_with_core()),
            documents: Arc::new(DocumentStore::new()),
            root: std::sync::Mutex::new(None),
            lib_entries: LibEntries::default(),
            classpath_cli_lock: ClasspathCliLock::default(),
        }
    }

    /// Paths of currently open documents — kept indexed even when they live
    /// outside deps.edn `:paths`.
    fn open_paths(documents: &DocumentStore) -> std::collections::HashSet<std::path::PathBuf> {
        documents
            .open_uris()
            .into_iter()
            .filter_map(|uri| uri.to_file_path().ok())
            .collect()
    }

    /// Reads the text of a `jar:` URI entry. Shared by the LSP 3.17
    /// `workspace/textDocumentContent` method and clojure-lsp's
    /// `clojure/dependencyContents`.
    fn read_jar_uri(uri: &str) -> tower_lsp::jsonrpc::Result<String> {
        let (jar_path, entry_path) = jar_content::parse_jar_uri(uri).map_err(|e| {
            tracing::warn!("jar content: bad URI {}: {}", uri, e);
            tower_lsp::jsonrpc::Error::invalid_params(e.to_string())
        })?;

        if !jar_path.exists() {
            return Err(tower_lsp::jsonrpc::Error {
                code: tower_lsp::jsonrpc::ErrorCode::ServerError(-32801),
                message: std::borrow::Cow::Owned(format!("JAR not found: {}", jar_path.display())),
                data: None,
            });
        }

        jar_content::extract_content(&jar_path, &entry_path).map_err(|e| {
            tracing::warn!("jar content: failed to extract {}: {}", uri, e);
            let msg = e.to_string();
            if msg.contains("not found") {
                tower_lsp::jsonrpc::Error::invalid_params(msg)
            } else {
                tower_lsp::jsonrpc::Error::internal_error()
            }
        })
    }

    /// LSP 3.17 `workspace/textDocumentContent` — used by clients that support
    /// the standardized content-provider method.
    pub async fn text_document_content(
        &self,
        params: TextDocumentContentParams,
    ) -> tower_lsp::jsonrpc::Result<TextDocumentContentResult> {
        Self::read_jar_uri(&params.uri).map(|text| TextDocumentContentResult { text })
    }

    /// clojure-lsp-compatible `clojure/dependencyContents`: returns the raw
    /// content string for a `jar:` URI. Calva (and other clojure-lsp clients)
    /// register a `jar`-scheme content provider that calls this; without it they
    /// cannot open any library or clojure.core navigation target, since plain
    /// vscode-languageclient does not support `workspace/textDocumentContent`.
    pub async fn dependency_contents(
        &self,
        params: TextDocumentContentParams,
    ) -> tower_lsp::jsonrpc::Result<String> {
        Self::read_jar_uri(&params.uri)
    }

    /// clj-pulse custom `clojurePulse/ignoredForms`: the whole-form ranges of
    /// `#_` discard forms and `(comment …)` blocks in the live buffer, for the
    /// editor to dim. An unparseable or unopened URI yields an empty list. Never
    /// errors — dimming is best-effort.
    pub async fn ignored_forms(
        &self,
        params: IgnoredFormsParams,
    ) -> tower_lsp::jsonrpc::Result<Vec<Range>> {
        let Ok(uri) = Url::parse(&params.uri) else {
            return Ok(Vec::new());
        };
        let Some(text) = self.documents.text(&uri) else {
            return Ok(Vec::new());
        };
        Ok(handlers::ignored_forms::ignored_form_ranges(&text))
    }

    /// clj-pulse custom `clojurePulse/externalLibraries`: the resolved external
    /// libraries for the panel. Re-derives from disk per request (reading
    /// `.cpcache`/lgx is cheap) so it survives server restarts without new
    /// state. No project root yet → empty list, never an error.
    pub async fn external_libraries(
        &self,
        _params: Option<serde_json::Value>,
    ) -> tower_lsp::jsonrpc::Result<Vec<libraries::Library>> {
        let Some(root) = self.root.lock().unwrap().clone() else {
            return Ok(Vec::new());
        };
        // Re-derivation reads `.cpcache`/lgx/source-paths from disk; keep it off
        // the LSP executor so it can't stall hover/completion/definition.
        tokio::task::spawn_blocking(move || {
            let entries = resolve_lib_entries(&root);
            let own_paths = config::source_paths(&root);
            libraries::from_entries(&own_paths, &entries)
        })
        .await
        .map_err(|e| {
            tracing::error!("externalLibraries task panicked: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    /// clj-pulse custom `clojurePulse/libraryEntries`: the file entries of a jar
    /// library, for the panel to fold into a browsable tree. Rejects anything
    /// that is not an existing `.jar` file with `invalid_params`.
    pub async fn library_entries(
        &self,
        params: LibraryEntriesParams,
    ) -> tower_lsp::jsonrpc::Result<Vec<String>> {
        let path = std::path::PathBuf::from(&params.path);
        if path.extension().and_then(|e| e.to_str()) != Some("jar") || !path.is_file() {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "not an existing .jar file: {}",
                params.path
            )));
        }
        // Reading a jar's central directory can be slow for large or
        // network-mounted archives; run it off the LSP executor.
        tokio::task::spawn_blocking(move || jar_content::list_entries(&path))
            .await
            .map_err(|e| {
                tracing::error!("libraryEntries task panicked: {}", e);
                tower_lsp::jsonrpc::Error::internal_error()
            })?
            .map_err(|e| {
                tracing::warn!("libraryEntries: failed to list {}: {}", params.path, e);
                tower_lsp::jsonrpc::Error::internal_error()
            })
    }

    /// Computes unresolved-namespace diagnostics from the live buffer and
    /// publishes them for `uri`.
    async fn lint_and_publish(&self, uri: Url, version: i32) {
        let Some(text) = self.documents.text(&uri) else {
            return;
        };
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        let diags = crate::diagnostics::compute(&text, &path);
        self.client
            .publish_diagnostics(uri, diags, Some(version))
            .await;
    }
}

/// Idle time after the last edit before re-linting a changed document.
const DIAGNOSTIC_DEBOUNCE_MS: u64 = 300;

/// The project root from an `initialize` request. Prefers the modern
/// `workspaceFolders` over the deprecated `rootUri`: newer clients (Zed and
/// others) send only `workspaceFolders`, and reading just `rootUri` left the
/// project unindexed — so same-file navigation worked but cross-file silently
/// failed.
fn initialize_root(params: &InitializeParams) -> Option<std::path::PathBuf> {
    params
        .workspace_folders
        .as_ref()
        .and_then(|folders| folders.first())
        .and_then(|folder| folder.uri.to_file_path().ok())
        .or_else(|| {
            params
                .root_uri
                .as_ref()
                .and_then(|uri| uri.to_file_path().ok())
        })
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(root_path) = initialize_root(&params) {
            {
                *self.root.lock().unwrap() = Some(root_path.clone());
                let index = self.index.clone();
                let client = self.client.clone();
                let documents = self.documents.clone();
                let root_path_jars = root_path.clone();
                tokio::spawn(async move {
                    let start = std::time::Instant::now();
                    let source_paths = config::source_paths(&root_path);
                    tracing::info!(
                        "project root: {}, source paths: {:?}",
                        root_path.display(),
                        source_paths
                    );
                    index.set_extract_config(settings::load(&root_path));

                    match scanner::build_index(&root_path, &source_paths, &index.extract_config()) {
                        Ok(new_index) => {
                            let sym_count = new_index.symbols.len();
                            let ns_count = new_index.namespaces.len();

                            index.merge_project_from(new_index, &Self::open_paths(&documents));

                            let elapsed = start.elapsed();
                            let msg = format!(
                                "Indexed {} symbols in {} namespaces in {:?}",
                                sym_count, ns_count, elapsed
                            );
                            tracing::info!("{}", msg);
                            client.log_message(MessageType::INFO, msg).await;
                        }
                        Err(e) => {
                            tracing::error!("failed to build index: {}", e);
                            client
                                .log_message(
                                    MessageType::ERROR,
                                    format!("clj-pulse: index build failed: {}", e),
                                )
                                .await;
                        }
                    }
                });

                // Background task: index library JARs from the classpath.
                // Graduated: stage 2 indexes whatever `.cpcache` already holds
                // (instant), then stage 3 resolves the authoritative classpath
                // via the clojure CLI and re-indexes on change.
                let index_jars = self.index.clone();
                let client_jars = self.client.clone();
                let lib_entries = self.lib_entries.clone();
                let cli_lock = self.classpath_cli_lock.clone();
                tokio::spawn(async move {
                    let resolved = resolve_and_index_libs(&root_path_jars, &index_jars);
                    let stage2_ok = resolved.indexed_any();
                    *lib_entries.lock().unwrap() = resolved.entries.into_iter().collect();
                    if stage2_ok {
                        let msg = format!(
                            "clj-pulse: library indexing complete ({} total symbols)",
                            index_jars.symbols.len()
                        );
                        tracing::info!("{}", msg);
                        client_jars.log_message(MessageType::INFO, msg).await;
                        client_jars.send_notification::<LibrariesChanged>(()).await;
                    }

                    // Stage 3 applies only to deps.edn projects; let-go and
                    // Leiningen resolution has no CLI-backed refinement.
                    let is_deps_project = config::project_kind(&root_path_jars)
                        == config::ProjectKind::Clojure
                        && root_path_jars.join("deps.edn").exists();
                    let stage3_ok = is_deps_project
                        && run_classpath_cli(
                            &root_path_jars,
                            &index_jars,
                            &client_jars,
                            &lib_entries,
                            &cli_lock,
                        )
                        .await;

                    if !stage2_ok && !stage3_ok {
                        let msg = match config::project_kind(&root_path_jars) {
                            config::ProjectKind::LetGo => {
                                "clj-pulse: no lgx deps resolved (no ~/.lgx/gitlibs, or deps not \
                                 fetched — run `lgx run`/`lgx build` once) — library symbols \
                                 will not be indexed."
                            }
                            config::ProjectKind::Clojure => {
                                "clj-pulse: no classpath found (no .cpcache/ in project root?) \
                                 — library symbols will not be indexed. Run \
                                 `clojure -A:dev:test -Spath` or start a REPL once to generate it."
                            }
                        };
                        tracing::warn!("{}", msg);
                        client_jars.log_message(MessageType::WARNING, msg).await;
                        client_jars.send_notification::<LibrariesChanged>(()).await;
                    }
                });

                // Background task: discover and index the JDK's bundled Java
                // source (`src.zip`) for built-in Java navigation/completion.
                let index_jdk = self.index.clone();
                let client_jdk = self.client.clone();
                tokio::spawn(async move {
                    match crate::index::jdk::JdkIndex::discover() {
                        Some(jdk) => {
                            let count = jdk.class_count();
                            index_jdk.set_jdk(jdk);
                            let msg = format!("JDK source indexed: {} classes", count);
                            tracing::info!("{}", msg);
                            client_jdk.log_message(MessageType::INFO, msg).await;
                        }
                        None => {
                            tracing::debug!(
                                "no JDK source (src.zip) found — built-in Java navigation disabled"
                            );
                        }
                    }
                });
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions::default()),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), " ".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: Default::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: "\n".to_string(),
                    more_trigger_character: None,
                }),
                experimental: Some(serde_json::json!({
                    "textDocumentContentProvider": { "schemes": ["jar"] }
                })),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "clj-pulse".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        tracing::info!("clj-pulse initialized");

        // Watch source files so git pulls / branch switches keep the index
        // fresh without editor saves. Clients without dynamic registration
        // simply reject this; everything else still works.
        let watchers = vec![
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.{clj,cljs,cljc,lg}".to_string()),
                kind: None,
            },
            FileSystemWatcher {
                // All EDN files: manifests (deps.edn / lgx.edn → classpath) and
                // Integrant system configs (→ keyword occurrences). The handler
                // routes each by name/content.
                glob_pattern: GlobPattern::String("**/*.edn".to_string()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.cpcache/*.cp".to_string()),
                kind: None,
            },
            // clj-pulse and clj-kondo config: reload `:lint-as` on change. Named
            // explicitly so clients that skip dotfiles under `**/*.edn` still
            // watch them.
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.clj-kondo/config.edn".to_string()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.clj-pulse/config.edn".to_string()),
                kind: None,
            },
        ];
        let registration = Registration {
            id: "clj-pulse-watched-files".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                watchers,
            })
            .ok(),
        };
        if let Err(e) = self.client.register_capability(vec![registration]).await {
            tracing::info!("watched-files registration not supported: {}", e);
        }
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("clj-pulse shutting down");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        // Files outside deps.edn :paths (dev/, scratch files, test dirs that
        // only appear in alias :extra-paths) are not indexed at startup;
        // index them on open so navigation from them works.
        if let Ok(path) = uri.to_file_path() {
            if config::is_clojure_source(&path) && self.index.file_ns(&path).is_none() {
                match extractor::extract_full_with(&text, &path, &self.index.extract_config()) {
                    Ok((meta, symbols, occurrences)) => {
                        tracing::info!("indexed opened file {}", path.display());
                        self.index.insert_file(meta, symbols, occurrences);
                    }
                    Err(e) => {
                        tracing::debug!("failed to index opened {}: {}", path.display(), e)
                    }
                }
            } else if extractor::is_integrant_edn(&path, &text)
                && self.index.file_ns(&path).is_none()
            {
                // Integrant config opened from outside the scanned paths.
                tracing::info!("indexed opened EDN config {}", path.display());
                self.index
                    .insert_edn_file(path.clone(), extractor::extract_edn(&text));
            }
        }

        self.documents.open(uri.clone(), text);
        self.documents.set_version(&uri, version);
        self.lint_and_publish(uri, version).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.close(&uri);
        // Clear diagnostics for the closed document.
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        // Only re-index Clojure source files; saving an EDN config file
        // (deps.edn / lgx.edn) must not insert a junk empty namespace.
        if config::is_clojure_source(&path) {
            match std::fs::read_to_string(&path) {
                Ok(source) => {
                    self.index.remove_file(&path);
                    match extractor::extract_full_with(&source, &path, &self.index.extract_config())
                    {
                        Ok((meta, symbols, occurrences)) => {
                            let count = symbols.len();
                            self.index.insert_file(meta, symbols, occurrences);
                            tracing::info!("re-indexed {} ({} symbols)", path.display(), count);
                        }
                        Err(e) => tracing::warn!("failed to re-index {}: {}", path.display(), e),
                    }
                }
                Err(e) => tracing::warn!("failed to read {}: {}", path.display(), e),
            }
        } else if config::is_edn(&path) {
            // Re-index Integrant EDN configs. The file is always removed first
            // (so an edit that drops `#ig/ref` de-indexes it) and re-inserted
            // only when it still looks like an Integrant system.
            match std::fs::read_to_string(&path) {
                Ok(source) => {
                    self.index.remove_file(&path);
                    if extractor::is_integrant_edn(&path, &source) {
                        self.index
                            .insert_edn_file(path.clone(), extractor::extract_edn(&source));
                        tracing::info!("re-indexed EDN config {}", path.display());
                    }
                }
                Err(e) => tracing::warn!("failed to read {}: {}", path.display(), e),
            }
        }

        let version = self.documents.current_version(&uri).unwrap_or(0);
        self.lint_and_publish(uri, version).await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let mut classpath_changed = false;
        let mut source_paths_changed = false;
        let mut config_changed = false;
        // `.clj-pulse/config.edn` specifically: only it carries `:classpath`,
        // so only it triggers a CLI re-resolution (`.clj-kondo` does not).
        let mut pulse_config_changed = false;
        for event in params.changes {
            let Ok(path) = event.uri.to_file_path() else {
                continue;
            };

            // deps.edn / lgx.edn / project.clj affect both the classpath/deps
            // and the project's own :paths; .cpcache only the classpath.
            let manifest = path
                .file_name()
                .map(|n| n == "deps.edn" || n == "lgx.edn" || n == "project.clj")
                .unwrap_or(false);
            if manifest {
                classpath_changed = true;
                source_paths_changed = true;
                continue;
            }
            if path.components().any(|c| c.as_os_str() == ".cpcache") {
                classpath_changed = true;
                continue;
            }

            // clj-pulse / clj-kondo config: reload `:lint-as` and re-index the
            // project. Intercept before the EDN branch below, since `config.edn`
            // is itself `.edn` but is not an Integrant config.
            let config_dir = path
                .file_name()
                .map(|n| n == "config.edn")
                .unwrap_or(false)
                .then(|| path.parent().and_then(|p| p.file_name()))
                .flatten();
            if config_dir.is_some_and(|d| d == ".clj-kondo" || d == ".clj-pulse") {
                config_changed = true;
                pulse_config_changed |= config_dir.is_some_and(|d| d == ".clj-pulse");
                continue;
            }

            // Integrant EDN configs: keep keyword occurrences fresh on external
            // edits (git pull / branch switch). Remove first so dropping
            // `#ig/ref` — or the file — de-indexes it; re-insert only when it
            // still looks like an Integrant system. Manifests (deps.edn/lgx.edn)
            // already `continue`d above as classpath changes.
            if config::is_edn(&path) {
                self.index.remove_file(&path);
                if event.typ != FileChangeType::DELETED {
                    if let Ok(source) = std::fs::read_to_string(&path) {
                        if extractor::is_integrant_edn(&path, &source) {
                            self.index
                                .insert_edn_file(path.clone(), extractor::extract_edn(&source));
                            tracing::info!("watched re-index EDN config: {}", path.display());
                        }
                    }
                }
                continue;
            }

            if !config::is_clojure_source(&path) {
                continue;
            }

            if event.typ == FileChangeType::DELETED {
                tracing::info!("watched delete: {}", path.display());
                self.index.remove_file(&path);
                continue;
            }

            // CREATED or CHANGED
            match std::fs::read_to_string(&path) {
                Ok(source) => {
                    self.index.remove_file(&path);
                    match extractor::extract_full_with(&source, &path, &self.index.extract_config())
                    {
                        Ok((meta, symbols, occurrences)) => {
                            tracing::info!("watched re-index: {}", path.display());
                            self.index.insert_file(meta, symbols, occurrences);
                        }
                        Err(e) => {
                            tracing::warn!("failed to extract {}: {}", path.display(), e)
                        }
                    }
                }
                Err(e) => tracing::warn!("failed to read {}: {}", path.display(), e),
            }
        }

        if classpath_changed || config_changed {
            let root = self.root.lock().unwrap().clone();
            if let Some(root) = root {
                let index = self.index.clone();
                let client = self.client.clone();
                let documents = self.documents.clone();
                let lib_entries = self.lib_entries.clone();
                let cli_lock = self.classpath_cli_lock.clone();
                tokio::spawn(async move {
                    // A config change reloads `:lint-as` before re-indexing, so
                    // the rebuild extracts project files with the new mapping.
                    if config_changed {
                        index.set_extract_config(settings::load(&root));
                    }

                    // Rebuild project sources when :paths changed or the config
                    // changed (lint-as affects how every project file extracts).
                    if source_paths_changed || config_changed {
                        let source_paths = config::source_paths(&root);
                        match scanner::build_index(&root, &source_paths, &index.extract_config()) {
                            Ok(new_index) => {
                                index.merge_project_from(new_index, &Self::open_paths(&documents))
                            }
                            Err(e) => tracing::error!("project re-index failed: {}", e),
                        }
                    }

                    // Open buffers outside :paths were indexed on didOpen with
                    // the previous config; re-extract each open project buffer so
                    // the reload reaches them too. Jar/dir-lib files have no
                    // occurrences entry, so `is_project_path` skips them.
                    if config_changed {
                        let cfg = index.extract_config();
                        for uri in documents.open_uris() {
                            let Ok(path) = uri.to_file_path() else {
                                continue;
                            };
                            if !config::is_clojure_source(&path) || !index.is_project_path(&path) {
                                continue;
                            }
                            if let Some(text) = documents.text(&uri) {
                                if let Ok((meta, symbols, occ)) =
                                    extractor::extract_full_with(&text, &path, &cfg)
                                {
                                    index.remove_file(&path);
                                    index.insert_file(meta, symbols, occ);
                                }
                            }
                        }
                    }

                    // Log the reload before the (optional) library branch, whose
                    // early return could otherwise skip it.
                    if config_changed {
                        client
                            .log_message(MessageType::INFO, "clj-pulse: config reloaded")
                            .await;
                    }

                    if classpath_changed {
                        // Drop symbols of removed/replaced dependencies first
                        index.clear_libs();
                        let resolved = resolve_and_index_libs(&root, &index);
                        *lib_entries.lock().unwrap() = resolved.entries.iter().cloned().collect();
                        if resolved.indexed_any() {
                            let msg = "clj-pulse: library re-indexing complete";
                            tracing::info!("{}", msg);
                            client.log_message(MessageType::INFO, msg).await;
                        }
                        // Notify either way — on nothing resolved the panel
                        // must clear.
                        client.send_notification::<LibrariesChanged>(()).await;
                    }

                    // `:classpath` (aliases / enabled) may have changed — e.g.
                    // an editor UI writing the alias selection — so re-resolve
                    // via the CLI. Comparing against `lib_entries` skips the
                    // re-index when the classpath is unchanged.
                    if pulse_config_changed {
                        let is_deps_project = config::project_kind(&root)
                            == config::ProjectKind::Clojure
                            && root.join("deps.edn").exists();
                        if is_deps_project {
                            run_classpath_cli(&root, &index, &client, &lib_entries, &cli_lock)
                                .await;
                        }
                    }
                });
            }
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        if let Err(e) = self.documents.apply_changes(&uri, params.content_changes) {
            tracing::warn!("failed to apply changes to {}: {}", uri, e);
            return;
        }
        self.documents.set_version(&uri, version);

        // Debounced re-lint: only the latest edit (matching version) survives
        // the sleep, so bursts of keystrokes collapse to one diagnostic pass.
        let documents = self.documents.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(DIAGNOSTIC_DEBOUNCE_MS)).await;
            if documents.current_version(&uri) != Some(version) {
                return;
            }
            let Some(text) = documents.text(&uri) else {
                return;
            };
            let Ok(path) = uri.to_file_path() else {
                return;
            };
            let diags = crate::diagnostics::compute(&text, &path);
            client.publish_diagnostics(uri, diags, Some(version)).await;
        });
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        handlers::definition::handle(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("definition error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        handlers::completion::handle(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("completion error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        handlers::hover::handle(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("hover error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        handlers::signature::handle(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("signature help error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        // Rename errors are user-facing (invalid name, library symbol, …)
        handlers::references::rename(&self.index, &self.documents, params)
            .map_err(|e| tower_lsp::jsonrpc::Error::invalid_params(e.to_string()))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        handlers::references::references(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("references error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        handlers::symbols::document_symbols(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("document symbol error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        Ok(Some(handlers::symbols::workspace_symbols(
            &self.index,
            &params.query,
        )))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        handlers::code_action::handle(&self.index, &self.documents, params).map_err(|e| {
            tracing::error!("code action error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        handlers::indent::on_type_formatting(&self.documents, params).map_err(|e| {
            tracing::error!("on type formatting error: {}", e);
            tower_lsp::jsonrpc::Error::internal_error()
        })
    }
}
