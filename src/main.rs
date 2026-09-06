// Binary re-exports lib modules; many are used only by the lib target (tests) for now.
#![allow(dead_code)]

use tower_lsp::{LspService, Server};

mod classpath;
mod clojuredocs;
mod config;
mod diagnostics;
mod document;
mod edn;
mod handlers;
mod index;
mod jar_content;
mod kondo;
mod leiningen;
mod lgx;
mod libraries;
mod panic_guard;
mod projects;
mod server;
mod settings;
mod uri;

use server::Backend;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--version") {
        println!("clj-pulse {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let verbose = args.iter().any(|a| a == "--verbose");

    let log_dir = std::env::current_dir()
        .ok()
        .and_then(|cwd| config::find_project_root(&cwd))
        .map(|root| root.join(".clj-pulse"))
        .unwrap_or_else(|| std::env::temp_dir().join("clj-pulse"));
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

    panic_guard::install_panic_hook();

    tracing::info!("clj-pulse starting");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let builder = LspService::build(Backend::new)
        .custom_method(
            "workspace/textDocumentContent",
            Backend::text_document_content,
        )
        // clojure-lsp-compatible jar content provider — what Calva calls to open
        // `jar:` navigation targets (clojure.core and library sources).
        .custom_method("clojure/dependencyContents", Backend::dependency_contents)
        // clj-pulse custom: ranges of `#_`/`(comment …)` forms for the extension
        // to dim (Calva-style opacity decoration).
        .custom_method("clojurePulse/ignoredForms", Backend::ignored_forms)
        // clj-pulse custom: the External Libraries panel — the resolved library
        // list and, per jar, its browsable file entries.
        .custom_method(
            "clojurePulse/externalLibraries",
            Backend::external_libraries,
        )
        .custom_method("clojurePulse/libraryEntries", Backend::library_entries)
        // clj-pulse custom: the grouped per-project view (kind, classpath
        // status, per-project libraries) for multi-project workspaces.
        .custom_method("clojurePulse/projects", Backend::projects_info)
        // clj-pulse custom: force re-detection + re-resolution (retries
        // error projects, picks up new gitignored subprojects).
        .custom_method("clojurePulse/rescan", Backend::rescan)
        // clj-pulse custom: the ClojureDocs entry (examples, see-alsos) for the
        // symbol at a position or a given `ns/name`, from the export file the
        // editor pointed at in initializationOptions — never the network.
        .custom_method("clojurePulse/clojureDocs", Backend::clojure_docs);

    // Test-only: a request that panics on demand, so the e2e suite can prove a
    // panicking handler fails alone instead of killing the process.
    let builder = if std::env::var_os("CLJ_PULSE_TEST_PANIC").is_some_and(|v| !v.is_empty()) {
        builder.custom_method("clojurePulse/__testPanic", Backend::test_panic)
    } else {
        builder
    };

    let (service, socket) = builder.finish();
    // A panicking handler must fail that request alone: tower-lsp polls
    // handler futures inline, so an unguarded panic would exit the process and
    // cost the editor its whole index.
    Server::new(stdin, stdout, socket)
        .serve(panic_guard::PanicGuard::new(service))
        .await;
}
