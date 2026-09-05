//! Keeping the process alive when a request handler panics.
//!
//! tower-lsp does not spawn its handlers: `Server::serve` polls the futures
//! returned by `Service::call` inside its own loop, so a panic in any handler
//! unwinds through `serve`, through `main`, and the process exits. The editor
//! then restarts the server, the index is rebuilt from scratch, and every open
//! buffer's state is lost — over one bad request.
//!
//! [`PanicGuard`] is a `tower` middleware that catches that unwind and turns it
//! into a JSON-RPC internal error for the offending request alone.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::FutureExt;
use tower_lsp::jsonrpc::{Error, Id, Request, Response};
use tower_lsp::ExitedError;
use tower_service::Service;

/// Installs a panic hook that records the payload and location in `server.log`.
///
/// Panics in `tokio::spawn`ed background work (indexing, lint passes) are
/// already isolated from the process, but they vanish silently; this is how
/// they — and the ones [`PanicGuard`] catches — become visible.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        match info.location() {
            Some(loc) => tracing::error!("panicked at {}:{}: {}", loc.file(), loc.line(), message),
            None => tracing::error!("panicked at unknown location: {}", message),
        }
        previous(info);
    }));
}

/// A `tower` middleware that contains a panicking handler.
///
/// `AssertUnwindSafe` is justified: the `Backend` state is `DashMap`s and
/// `Arc`s, so a panic mid-update can at worst leave one map entry stale, which
/// the next `didChange` or save corrects. That is far better than losing the
/// process.
pub struct PanicGuard<S> {
    inner: S,
    /// Ids whose handler panicked and whose entry in tower-lsp's pending-request
    /// map therefore leaked (see [`PanicGuard::call`]).
    leaked: Arc<Mutex<Vec<Id>>>,
}

impl<S> PanicGuard<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            leaked: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// The response and error types are [`LspService`]'s own, so `Server::serve`'s
/// bounds are met unchanged.
impl<S> Service<Request> for PanicGuard<S>
where
    S: Service<Request, Response = Option<Response>, Error = ExitedError>,
    S::Future: Send + 'static,
{
    type Response = Option<Response>;
    type Error = ExitedError;
    type Future = Pin<Box<dyn Future<Output = Result<Option<Response>, ExitedError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        // tower-lsp tracks every request carrying an id in a pending map and
        // only removes the entry once the handler future *returns*. A panicking
        // one never does, so the entry — and its abort handle — would leak, and
        // a later request reusing that id would be answered `invalid request`
        // forever after. `$/cancelRequest` is the public way to drop the entry,
        // so each panicked id is cleared on the way into the next request.
        //
        // The clearing has to finish *before* `inner.call(req)` below, because
        // that is where the pending map is consulted — a client reusing the id
        // on its very next request would otherwise still be rejected. The
        // `$/cancelRequest` handler only removes a map entry, so it resolves on
        // the first poll; anything that somehow does not is queued again.
        let leaked: Vec<Id> = std::mem::take(&mut *self.leaked.lock().unwrap());
        for id in leaked {
            let cancel = self.inner.call(
                Request::build("$/cancelRequest")
                    .params(serde_json::json!({ "id": id.clone() }))
                    .finish(),
            );
            if cancel.now_or_never().is_none() {
                self.leaked.lock().unwrap().push(id);
            }
        }

        let id = req.id().cloned();
        let method = req.method().to_string();
        let future = self.inner.call(req);
        let leaked = self.leaked.clone();
        Box::pin(async move {
            match AssertUnwindSafe(future).catch_unwind().await {
                Ok(response) => response,
                Err(_) => {
                    // The hook above already logged the payload and location.
                    tracing::error!(
                        "handler for `{}` panicked; failing that request only",
                        method
                    );
                    // A notification has no id, so nothing was ever entered in
                    // the pending map and there is nothing to answer.
                    if let Some(id) = id.clone() {
                        leaked.lock().unwrap().push(id);
                    }
                    Ok(id.map(|id| Response::from_error(id, Error::internal_error())))
                }
            }
        })
    }
}
