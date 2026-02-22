use std::sync::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::config;
use crate::document::DocumentStore;
use crate::index::scanner;
use crate::index::Index;

pub struct Backend {
    pub client: Client,
    pub index: Arc<Index>,
    pub documents: DocumentStore,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            index: Arc::new(Index::new()),
            documents: DocumentStore::new(),
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(root_uri) = params.root_uri {
            if let Ok(root_path) = root_uri.to_file_path() {
                let index = self.index.clone();
                let client = self.client.clone();
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
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                completion_provider: Some(CompletionOptions::default()),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
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

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Err(e) = self.documents.apply_changes(&uri, params.content_changes) {
            tracing::warn!("failed to apply changes to {}: {}", uri, e);
        }
    }

    async fn goto_definition(
        &self,
        _params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        Ok(None)
    }

    async fn completion(&self, _params: CompletionParams) -> Result<Option<CompletionResponse>> {
        Ok(None)
    }

    async fn hover(&self, _params: HoverParams) -> Result<Option<Hover>> {
        Ok(None)
    }
}
