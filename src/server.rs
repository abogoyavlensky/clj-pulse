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

#[derive(serde::Deserialize)]
struct TextDocumentContentParams {
    uri: String,
}

#[derive(serde::Serialize)]
struct TextDocumentContentResult {
    text: String,
}

pub struct Backend {
    pub client: Client,
    pub index: Arc<Index>,
    pub documents: DocumentStore,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            index: Arc::new(Index::new_with_core()),
            documents: DocumentStore::new(),
        }
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
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(root_uri) = params.root_uri {
            if let Ok(root_path) = root_uri.to_file_path() {
                let index = self.index.clone();
                let client = self.client.clone();
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

                            for entry in new_index.symbols.iter() {
                                index
                                    .symbols
                                    .insert(entry.key().clone(), entry.value().clone());
                            }
                            for entry in new_index.namespaces.iter() {
                                index
                                    .namespaces
                                    .insert(entry.key().clone(), entry.value().clone());
                            }
                            for entry in new_index.ns_symbols.iter() {
                                index
                                    .ns_symbols
                                    .insert(entry.key().clone(), entry.value().clone());
                            }
                            for entry in new_index.file_to_ns.iter() {
                                index
                                    .file_to_ns
                                    .insert(entry.key().clone(), entry.value().clone());
                            }

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
                    let classpath = classpath::discover(&root_path_jars);
                    if classpath.is_empty() {
                        return;
                    }
                    scanner::index_classpath_jars(&root_path_jars, classpath, &index_jars);
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
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("clj-lsp shutting down");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        self.documents.open(uri, text);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.close(&params.text_document.uri);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return;
        };
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to read {}: {}", path.display(), e);
                return;
            }
        };
        self.index.remove_file(&path);
        match extractor::extract(&source, &path) {
            Ok((meta, symbols)) => {
                let count = symbols.len();
                self.index.insert_file(meta, symbols);
                tracing::info!("re-indexed {} ({} symbols)", path.display(), count);
            }
            Err(e) => tracing::warn!("failed to re-index {}: {}", path.display(), e),
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Err(e) = self.documents.apply_changes(&uri, params.content_changes) {
            tracing::warn!("failed to apply changes to {}: {}", uri, e);
        }
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
}
