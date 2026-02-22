// Binary re-exports lib modules; many are used only by the lib target (tests) for now.
#![allow(dead_code)]

use tower_lsp::{LspService, Server};

mod config;
mod document;
mod index;
mod server;

use server::Backend;

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("clj-lsp {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let log_dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("clj-lsp");
    std::fs::create_dir_all(&log_dir).ok();
    let file_appender = tracing_appender::rolling::daily(log_dir, "server.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt().with_writer(non_blocking).init();

    tracing::info!("clj-lsp starting");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
