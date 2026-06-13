// Binary re-exports lib modules; many are used only by the lib target (tests) for now.
#![allow(dead_code)]

use tower_lsp::{LspService, Server};

mod classpath;
mod config;
mod diagnostics;
mod document;
mod handlers;
mod index;
mod jar_content;
mod server;

use server::Backend;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--version") {
        println!("clj-lsp {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let verbose = args.iter().any(|a| a == "--verbose");

    let log_dir = std::env::current_dir()
        .ok()
        .and_then(|cwd| config::find_project_root(&cwd))
        .map(|root| root.join(".clj-lsp"))
        .unwrap_or_else(|| std::env::temp_dir().join("clj-lsp"));
    std::fs::create_dir_all(&log_dir).ok();

    let log_path = log_dir.join("server.log");
    let log_file = std::fs::File::create(&log_path).expect("cannot create log file");

    let (non_blocking, _guard) = tracing_appender::non_blocking(log_file);
    let level = if verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::WARN
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(non_blocking)
        .init();

    tracing::info!("clj-lsp starting");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::build(Backend::new)
        .custom_method(
            "workspace/textDocumentContent",
            Backend::text_document_content,
        )
        .finish();
    Server::new(stdin, stdout, socket).serve(service).await;
}
