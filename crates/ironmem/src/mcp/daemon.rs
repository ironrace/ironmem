//! Single-owner dispatcher actor for shared-daemon mode.
//!
//! `App` is `!Sync`, so `Arc<App>` is `!Send` and cannot cross a `tokio::spawn`
//! boundary. To share one `App` across many concurrent connections, a single
//! owner task holds the `Arc<App>` and is the SOLE caller of `dispatch`.
//! Per-connection handlers send their request plus a oneshot reply channel over
//! an mpsc; the owner serially dispatches and replies. This confines `App` to
//! one task so it is never required to be `Send`.

use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use super::app::App;
use super::protocol::{JsonRpcRequest, JsonRpcResponse};
use super::server::dispatch;

/// A request routed to the dispatcher owner, paired with the reply channel the
/// owner uses to return the response to the originating connection handler.
///
/// The type is `pub` (fields stay private) because it appears in the return
/// type of the public [`dispatcher_channel`] function via
/// `mpsc::Receiver<DispatchMessage>`; keeping it private would trip the
/// `private_interfaces` lint.
pub struct DispatchMessage {
    request: JsonRpcRequest,
    respond_to: oneshot::Sender<Option<JsonRpcResponse>>,
}

/// Cloneable handle used by connection handlers to send requests to the single
/// dispatcher owner. Cloning is cheap (clones the mpsc sender).
#[derive(Clone)]
pub struct DispatcherHandle {
    tx: mpsc::Sender<DispatchMessage>,
}

impl DispatcherHandle {
    /// Async round-trip: send `request` to the owner and await its response.
    /// Returns `None` if the dispatcher owner has shut down (channel closed) or
    /// produced no response (e.g. a notification).
    pub async fn dispatch(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let (respond_to, rx) = oneshot::channel();
        if self
            .tx
            .send(DispatchMessage {
                request,
                respond_to,
            })
            .await
            .is_err()
        {
            return None;
        }
        rx.await.ok().flatten()
    }

    /// Blocking round-trip for use INSIDE `tokio::task::block_in_place` from the
    /// synchronous `run_framing_loop` dispatch backend (Task 6). Must NOT be
    /// called on the dispatcher owner's own task (would deadlock) — only from a
    /// distinct per-connection handler task.
    pub fn blocking_dispatch(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let (respond_to, rx) = oneshot::channel();
        if self
            .tx
            .blocking_send(DispatchMessage {
                request,
                respond_to,
            })
            .is_err()
        {
            return None;
        }
        rx.blocking_recv().ok().flatten()
    }
}

/// Create a dispatcher channel with the given mpsc buffer size, returning the
/// cloneable handle and the receiver the owner loop consumes.
pub fn dispatcher_channel(buffer: usize) -> (DispatcherHandle, mpsc::Receiver<DispatchMessage>) {
    let (tx, rx) = mpsc::channel(buffer);
    (DispatcherHandle { tx }, rx)
}

/// The single-owner dispatcher loop. Owns `Arc<App>` and is the sole caller of
/// `dispatch`. This future is `!Send` (holds `Arc<App>`); it MUST be driven on a
/// `LocalSet`/`spawn_local` or a dedicated current-thread runtime, NEVER
/// `tokio::spawn`ed on the multi-thread runtime.
///
/// `dispatch` is called directly (synchronously) rather than via
/// `block_in_place`: `block_in_place` is only valid on a multi-thread-runtime
/// worker and panics inside a `LocalSet`/current-thread runtime — precisely the
/// contexts this `!Send` owner must run on. Because the owner is the sole task
/// on its dedicated execution context, a blocking `dispatch` starves nothing
/// here. Concurrency is preserved on the OTHER side of the channel: connection
/// handlers (Task 6) live on the multi-thread runtime and wrap their
/// [`DispatcherHandle::blocking_dispatch`] round-trip in `block_in_place`, so
/// their runtime worker keeps serving peers while this owner works.
pub async fn run_dispatcher(app: Arc<App>, mut rx: mpsc::Receiver<DispatchMessage>) {
    while let Some(DispatchMessage {
        request,
        respond_to,
    }) = rx.recv().await
    {
        let response = dispatch(&app, &request);
        // Ignore send errors: the connection handler may have dropped (client
        // disconnected) before the reply was ready.
        let _ = respond_to.send(response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_request(line: &str) -> JsonRpcRequest {
        serde_json::from_str(line).expect("valid JSON-RPC request")
    }

    /// Drives two concurrent in-memory "connections" (cloned handles) through a
    /// single dispatcher owned by a `spawn_local`'d future. Asserts each reply
    /// carries the id of ITS request (no cross-talk), which proves correct
    /// routing of concurrent in-flight requests. That the `Arc<App>`-owning
    /// future is `spawn_local`'d (never `tokio::spawn`'d) proves `App` is never
    /// required to be `Send`.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_connections_route_to_correct_responses() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (handle, rx) = dispatcher_channel(16);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let dispatcher = tokio::task::spawn_local(run_dispatcher(app, rx));

                let h1 = handle.clone();
                let h2 = handle.clone();

                let req1 =
                    parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#);
                let req2 =
                    parse_request(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}"#);

                let (r1, r2) = tokio::join!(h1.dispatch(req1), h2.dispatch(req2));

                let r1 = r1.expect("tools/list returns a response");
                let r2 = r2.expect("initialize returns a response");

                // Correctly-routed: each response carries the id of its own
                // request, so concurrent in-flight requests did not swap replies.
                assert_eq!(r1.id, Some(serde_json::json!(1)));
                assert_eq!(r2.id, Some(serde_json::json!(2)));

                // Sanity: the responses are the ones we expect for each method.
                assert!(r1.result.is_some(), "tools/list is a success response");
                assert!(r2.result.is_some(), "initialize is a success response");

                // Drop every sender (the original plus both per-connection
                // clones) so the mpsc closes and the dispatcher loop exits.
                drop(handle);
                drop(h1);
                drop(h2);
                dispatcher.await.unwrap();
            })
            .await;
    }

    /// Verifies the blocking round-trip used by Task 6's synchronous framing
    /// backend: a `spawn_blocking` task calls `blocking_dispatch` while the
    /// dispatcher runs on a `LocalSet`, and receives the correctly-routed reply.
    #[tokio::test(flavor = "multi_thread")]
    async fn blocking_dispatch_round_trips() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (handle, rx) = dispatcher_channel(16);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let dispatcher = tokio::task::spawn_local(run_dispatcher(app, rx));

                let h = handle.clone();
                let response = tokio::task::spawn_blocking(move || {
                    let req = parse_request(
                        r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#,
                    );
                    h.blocking_dispatch(req)
                })
                .await
                .unwrap();

                let response = response.expect("tools/list returns a response");
                assert_eq!(response.id, Some(serde_json::json!(7)));
                assert!(response.result.is_some());

                drop(handle);
                dispatcher.await.unwrap();
            })
            .await;
    }
}
