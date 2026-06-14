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

/// Resolves and indexes a project's libraries: lgx git/local deps (indexed as
/// source dirs, including in-workspace `:local/root` deps) for let-go projects,
/// or the `.cpcache` classpath (JARs + dirs) for Clojure projects. When there
/// is no usable `.cpcache` but a Leiningen `project.clj` is present, falls back
/// to resolving its direct deps to `~/.m2` JARs. Returns the number of resolved
/// entries; 0 means nothing was found to index.
fn resolve_and_index_libs(root: &std::path::Path, index: &Index) -> usize {
    match config::project_kind(root) {
        config::ProjectKind::LetGo => {
            let dirs = lgx::resolve(root);
            scanner::index_dir_libs(&dirs, index);
            dirs.len()
        }
        config::ProjectKind::Clojure => {
            // deps.edn's `.cpcache` is authoritative (full transitive
            // classpath). Only when it is empty do we consult a Leiningen
            // `project.clj` for its direct dependencies.
            let mut classpath = classpath::discover(root);
            if classpath.is_empty() && root.join("project.clj").exists() {
                classpath = leiningen::resolve(root);
            }
            let n = classpath.len();
            if n > 0 {
                scanner::index_classpath_libs(root, classpath, index);
            }
            n
        }
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

pub struct Backend {
    pub client: Client,
    pub index: Arc<Index>,
    pub documents: Arc<DocumentStore>,
    root: std::sync::Mutex<Option<std::path::PathBuf>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            index: Arc::new(Index::new_with_core()),
            documents: Arc::new(DocumentStore::new()),
            root: std::sync::Mutex::new(None),
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

    pub async fn text_document_content(
        &self,
        params: TextDocumentContentParams,
    ) -> tower_lsp::jsonrpc::Result<TextDocumentContentResult> {
        let (jar_path, entry_path) = jar_content::parse_jar_uri(&params.uri).map_err(|e| {
            tracing::warn!("text_document_content: bad URI {}: {}", params.uri, e);
            tower_lsp::jsonrpc::Error::invalid_params(e.to_string())
        })?;

        if !jar_path.exists() {
            return Err(tower_lsp::jsonrpc::Error {
                code: tower_lsp::jsonrpc::ErrorCode::ServerError(-32801),
                message: std::borrow::Cow::Owned(format!("JAR not found: {}", jar_path.display())),
                data: None,
            });
        }

        let text = jar_content::extract_content(&jar_path, &entry_path).map_err(|e| {
            tracing::warn!(
                "text_document_content: failed to extract {}: {}",
                params.uri,
                e
            );
            let msg = e.to_string();
            if msg.contains("not found") {
                tower_lsp::jsonrpc::Error::invalid_params(msg)
            } else {
                tower_lsp::jsonrpc::Error::internal_error()
            }
        })?;

        Ok(TextDocumentContentResult { text })
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

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(root_uri) = params.root_uri {
            if let Ok(root_path) = root_uri.to_file_path() {
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

                    match scanner::build_index(&root_path, &source_paths) {
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
                                    format!("clj-lsp: index build failed: {}", e),
                                )
                                .await;
                        }
                    }
                });

                // Background task: index library JARs from the classpath
                let index_jars = self.index.clone();
                let client_jars = self.client.clone();
                tokio::spawn(async move {
                    if resolve_and_index_libs(&root_path_jars, &index_jars) == 0 {
                        let msg = match config::project_kind(&root_path_jars) {
                            config::ProjectKind::LetGo => {
                                "clj-lsp: no lgx deps resolved (no ~/.lgx/gitlibs, or deps not \
                                 fetched — run `lgx run`/`lgx build` once) — library symbols \
                                 will not be indexed."
                            }
                            config::ProjectKind::Clojure => {
                                "clj-lsp: no classpath found (no .cpcache/ in project root?) \
                                 — library symbols will not be indexed. Run `clojure -Spath` \
                                 or start a REPL once to generate it."
                            }
                        };
                        tracing::warn!("{}", msg);
                        client_jars.log_message(MessageType::WARNING, msg).await;
                        return;
                    }
                    let sym_count = index_jars.symbols.len();
                    let msg = format!(
                        "clj-lsp: library indexing complete ({} total symbols)",
                        sym_count
                    );
                    tracing::info!("{}", msg);
                    client_jars.log_message(MessageType::INFO, msg).await;
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
                experimental: Some(serde_json::json!({
                    "textDocumentContentProvider": { "schemes": ["jar"] }
                })),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "clj-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        tracing::info!("clj-lsp initialized");

        // Watch source files so git pulls / branch switches keep the index
        // fresh without editor saves. Clients without dynamic registration
        // simply reject this; everything else still works.
        let watchers = vec![
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.{clj,cljs,cljc,lg}".to_string()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/deps.edn".to_string()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/lgx.edn".to_string()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.cpcache/*.cp".to_string()),
                kind: None,
            },
        ];
        let registration = Registration {
            id: "clj-lsp-watched-files".to_string(),
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
        tracing::info!("clj-lsp shutting down");
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
                match extractor::extract_full(&text, &path) {
                    Ok((meta, symbols, occurrences)) => {
                        tracing::info!("indexed opened file {}", path.display());
                        self.index.insert_file(meta, symbols, occurrences);
                    }
                    Err(e) => {
                        tracing::debug!("failed to index opened {}: {}", path.display(), e)
                    }
                }
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
                    match extractor::extract_full(&source, &path) {
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
        }

        let version = self.documents.current_version(&uri).unwrap_or(0);
        self.lint_and_publish(uri, version).await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let mut classpath_changed = false;
        let mut source_paths_changed = false;
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
                    match extractor::extract_full(&source, &path) {
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

        if classpath_changed {
            let root = self.root.lock().unwrap().clone();
            if let Some(root) = root {
                let index = self.index.clone();
                let client = self.client.clone();
                let documents = self.documents.clone();
                tokio::spawn(async move {
                    if source_paths_changed {
                        // :paths may have changed — rebuild project sources,
                        // dropping files from removed roots.
                        let source_paths = config::source_paths(&root);
                        match scanner::build_index(&root, &source_paths) {
                            Ok(new_index) => {
                                index.merge_project_from(new_index, &Self::open_paths(&documents))
                            }
                            Err(e) => tracing::error!("project re-index failed: {}", e),
                        }
                    }

                    // Drop symbols of removed/replaced dependencies first
                    index.clear_libs();
                    if resolve_and_index_libs(&root, &index) == 0 {
                        return;
                    }
                    let msg = "clj-lsp: library re-indexing complete";
                    tracing::info!("{}", msg);
                    client.log_message(MessageType::INFO, msg).await;
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
}
