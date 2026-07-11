//! A minimal Chrome DevTools Protocol (CDP) client.
//!
//! We hand-roll this rather than depend on a crate like `chromiumoxide` because we only need a
//! handful of CDP methods/events and those crates pull in their own Tokio runtime plus large
//! generated protocol bindings. The transport follows the same pattern as `crates/repl`'s Jupyter
//! kernel socket: connect with `async_tungstenite::tokio` (driven through `gpui_tokio` so the Tokio
//! reactor is available), split the stream, and pump messages on a background task.

use anyhow::{Context as _, Result, anyhow, bail};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tokio::ConnectStream;
use async_tungstenite::tungstenite::Message as WebSocketMessage;
use collections::HashMap;
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{AsyncApp, BackgroundExecutor, Task};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
type EventSubscribers = Arc<Mutex<HashMap<String, Vec<mpsc::UnboundedSender<Value>>>>>;

/// Per-request timeout. Guards against a wedged-but-alive Chrome (a hung renderer that neither
/// replies nor closes the socket) hanging an awaiter — and thus session setup — forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A connected CDP session. Cloneable: all clones share the same underlying socket and dispatch
/// state, so the client can be handed to multiple feature modules (pick mode, CSS panel, …).
#[derive(Clone)]
pub struct CdpClient {
    inner: Arc<CdpInner>,
}

/// The page-target session captured from `Target.attachedToTarget`. Shared so `send`/`dispatch`
/// can stamp and route page-scoped traffic, and so it can be updated when Chrome re-attaches the
/// page target after a reload — which is what lets the session survive a page refresh.
///
/// The session is PINNED to the first page target's `targetId`: `Target.setAutoAttach` at the
/// browser endpoint attaches to every new top-level page, so a `window.open()` / `target=_blank`
/// popup in the previewed app would otherwise capture the session and silently reroute all input,
/// screencast acks, and CSS commands to the invisible popup, freezing the preview. A reload keeps
/// the tab's `targetId`, so re-attach after refresh still updates the session id.
#[derive(Default)]
struct PageTargetState {
    session_id: Option<String>,
    target_id: Option<String>,
}
type PageSession = Arc<Mutex<PageTargetState>>;

/// Page-scoped CDP domains: commands for these must carry the page `sessionId` when we're attached at
/// the browser endpoint (flatten mode). `Target` itself is browser-scoped and must NOT be stamped.
const PAGE_DOMAINS: &[&str] = &[
    "Page",
    "DOM",
    "CSS",
    "Overlay",
    "Runtime",
    "Input",
    "Emulation",
];

fn is_page_scoped(method: &str) -> bool {
    method
        .split_once('.')
        .map(|(domain, _)| PAGE_DOMAINS.contains(&domain))
        .unwrap_or(false)
}

struct CdpInner {
    next_id: AtomicU64,
    pending: PendingMap,
    subscribers: EventSubscribers,
    outgoing: mpsc::UnboundedSender<WebSocketMessage>,
    executor: BackgroundExecutor,
    /// The current page-target session id (browser-endpoint flatten mode). `None` when attached
    /// directly to a page endpoint (legacy) or before the first attach.
    page_session: PageSession,
    /// Set once the read pump terminates (socket closed / errored). After this, `send` fails fast
    /// and subscriber streams have been closed so their `.next()` yields `None`.
    closed: Arc<AtomicBool>,
    /// Fires once when the connection closes, for disconnect detection by the view.
    on_closed: Mutex<Option<oneshot::Receiver<()>>>,
    // Keeps the read/write pump tasks alive for the lifetime of the client. Dropping the client
    // drops these, which tears down the connection.
    _pump: Arc<[Task<()>; 2]>,
}

impl CdpClient {
    /// Connect to a CDP target by its `webSocketDebuggerUrl` (a `ws://…` endpoint). The connection
    /// is established on the Tokio reactor via `gpui_tokio`, matching the approach in `crates/repl`.
    pub async fn connect(ws_url: String, cx: &AsyncApp) -> Result<Self> {
        let executor = cx.background_executor().clone();
        let stream = gpui_tokio::Tokio::spawn(cx, async move {
            let (stream, _response) = async_tungstenite::tokio::connect_async(ws_url)
                .await
                .context("connecting to CDP websocket")?;
            anyhow::Ok(stream)
        })
        .await
        .context("joining CDP connect task")??;

        Ok(Self::from_stream(stream, executor))
    }

    fn from_stream(stream: WebSocketStream<ConnectStream>, executor: BackgroundExecutor) -> Self {
        let (mut write, mut read) = stream.split();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::default()));
        let subscribers: EventSubscribers = Arc::new(Mutex::new(HashMap::default()));
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<WebSocketMessage>();
        let closed = Arc::new(AtomicBool::new(false));
        let page_session: PageSession = Arc::new(Mutex::new(PageTargetState::default()));
        let (closed_tx, closed_rx) = oneshot::channel();

        let write_task = executor.spawn(async move {
            while let Some(message) = outgoing_rx.next().await {
                if let Err(error) = write.send(message).await {
                    log::warn!("CDP write failed, closing connection: {error:#}");
                    break;
                }
            }
        });

        let read_task = executor.spawn({
            let pending = pending.clone();
            let subscribers = subscribers.clone();
            let closed = closed.clone();
            let page_session = page_session.clone();
            async move {
                while let Some(message) = read.next().await {
                    match message {
                        Ok(WebSocketMessage::Text(text)) => {
                            dispatch(&text, &pending, &subscribers, &page_session);
                        }
                        Ok(WebSocketMessage::Close(_)) => break,
                        Ok(_) => {}
                        Err(error) => {
                            log::warn!("CDP read failed, closing connection: {error:#}");
                            break;
                        }
                    }
                }
                // Connection ended. Mark closed, fail in-flight requests, and drop all subscriber
                // senders so their receivers' `.next()` yields `None` (stopping the frame/pick loops
                // from hanging forever), then signal disconnection.
                closed.store(true, Ordering::SeqCst);
                for (_, sender) in pending.lock().drain() {
                    let _ = sender.send(Err(anyhow!("CDP connection closed")));
                }
                subscribers.lock().clear();
                let _ = closed_tx.send(());
            }
        });

        Self {
            inner: Arc::new(CdpInner {
                next_id: AtomicU64::new(1),
                pending,
                subscribers,
                outgoing: outgoing_tx,
                executor,
                page_session,
                closed,
                on_closed: Mutex::new(Some(closed_rx)),
                _pump: Arc::new([read_task, write_task]),
            }),
        }
    }

    /// Whether the connection has closed (socket dropped, Chrome exited, etc.).
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Take the one-shot that fires when the connection closes. Returns `None` if already taken.
    /// The view awaits this to flip into a disconnected state and offer reconnect.
    pub fn take_closed_signal(&self) -> Option<oneshot::Receiver<()>> {
        self.inner.on_closed.lock().take()
    }

    /// Fire-and-forget a CDP method call: serialize, send, and never register a pending-response
    /// slot. Use for high-frequency commands whose result we don't need (`mouseMoved`,
    /// `screencastFrameAck`) so a fast stream of them can't accumulate in-flight requests or leak
    /// pending slots. Errors are swallowed (the connection-closed path is handled elsewhere).
    pub fn send_no_reply(&self, method: &str, params: Value) {
        if self.is_closed() {
            return;
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let payload = self.build_payload(id, method, params);
        if let Ok(text) = serde_json::to_string(&payload) {
            let _ = self
                .inner
                .outgoing
                .unbounded_send(WebSocketMessage::Text(text.into()));
        }
    }

    /// Build an outgoing CDP message, stamping the page `sessionId` when one is set and the method is
    /// page-scoped (flatten mode at the browser endpoint). Browser-scoped methods (`Target.*`) and the
    /// legacy page-endpoint case (no session) are sent unstamped.
    fn build_payload(&self, id: u64, method: &str, params: Value) -> Value {
        let mut payload = json!({ "id": id, "method": method, "params": params });
        if is_page_scoped(method) {
            if let Some(session_id) = self.inner.page_session.lock().session_id.clone() {
                payload["sessionId"] = Value::String(session_id);
            }
        }
        payload
    }

    /// The current page session id, if attached via the browser endpoint.
    pub fn page_session(&self) -> Option<String> {
        self.inner.page_session.lock().session_id.clone()
    }

    /// Send a CDP method call and await its result. `params` is the raw params object (use
    /// `serde_json::json!({...})`, or `Value::Null` for none).
    ///
    /// Fails fast if the connection is already closed, and times out after [`REQUEST_TIMEOUT`] so a
    /// wedged-but-alive browser can't hang the caller forever.
    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        if self.is_closed() {
            bail!("CDP connection is closed");
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(id, tx);

        let payload = self.build_payload(id, method, params);
        let text = serde_json::to_string(&payload).context("serializing CDP request")?;
        self.inner
            .outgoing
            .unbounded_send(WebSocketMessage::Text(text.into()))
            .map_err(|_| anyhow!("CDP connection is closed"))?;

        let mut timer = self.inner.executor.timer(REQUEST_TIMEOUT).fuse();
        let mut response = rx.fuse();
        futures::select! {
            result = response => result.context("CDP response channel dropped")?,
            _ = timer => {
                // Reclaim the pending slot so it doesn't leak, then report the timeout.
                self.inner.pending.lock().remove(&id);
                bail!("CDP request '{method}' timed out after {REQUEST_TIMEOUT:?}");
            }
        }
    }

    /// Subscribe to a CDP event by method name (e.g. `"Overlay.inspectNodeRequested"`). Returns a
    /// receiver yielding each event's `params` object. The subscription ends when the receiver is
    /// dropped.
    pub fn subscribe(&self, method: impl Into<String>) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded();
        self.inner
            .subscribers
            .lock()
            .entry(method.into())
            .or_default()
            .push(tx);
        rx
    }
}

fn dispatch(
    text: &str,
    pending: &PendingMap,
    subscribers: &EventSubscribers,
    page_session: &PageSession,
) {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        log::warn!("ignoring non-JSON CDP message");
        return;
    };

    // A response carries an `id`; an event carries a `method` and no `id`.
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        if let Some(sender) = pending.lock().remove(&id) {
            let result = response_result(&message);
            let _ = sender.send(result);
        }
        return;
    }

    if let Some(method) = message.get("method").and_then(Value::as_str) {
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        // Capture the page-target session id so subsequent page-scoped commands can be stamped, and
        // update it whenever Chrome re-attaches the SAME page target (e.g. after a reload, which
        // keeps the tab's targetId). This is what lets the session survive a page refresh. Attaches
        // to OTHER page targets (window.open / target=_blank popups, which browser-level auto-attach
        // also delivers) are ignored — adopting them would reroute all input and screencast acks to
        // the invisible popup and freeze the preview.
        if method == "Target.attachedToTarget" {
            let target_info = params.get("targetInfo");
            let is_page = target_info
                .and_then(|info| info.get("type"))
                .and_then(Value::as_str)
                == Some("page");
            if is_page {
                let session_id = params.get("sessionId").and_then(Value::as_str);
                let target_id = target_info
                    .and_then(|info| info.get("targetId"))
                    .and_then(Value::as_str);
                if let (Some(session_id), Some(target_id)) = (session_id, target_id) {
                    let mut state = page_session.lock();
                    match state.target_id.as_deref() {
                        None => {
                            state.target_id = Some(target_id.to_string());
                            state.session_id = Some(session_id.to_string());
                            log::info!("web preview: attached to page target (session captured)");
                        }
                        Some(pinned) if pinned == target_id => {
                            state.session_id = Some(session_id.to_string());
                            log::info!("web preview: re-attached to page target (session updated)");
                        }
                        Some(_) => {
                            log::info!(
                                "web preview: ignoring attach to other page target {target_id} \
                                 (popup/new tab)"
                            );
                        }
                    }
                }
            }
        }

        let mut subscribers = subscribers.lock();
        if let Some(senders) = subscribers.get_mut(method) {
            // Drop any closed subscriber channels as we go.
            senders.retain(|sender| sender.unbounded_send(params.clone()).is_ok());
        }
    }
}

fn response_result(message: &Value) -> Result<Value> {
    if let Some(error) = message.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let cdp_message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown CDP error");
        bail!("CDP error {code}: {cdp_message}");
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch_attached(page_session: &PageSession, target_id: &str, session_id: &str) {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::default()));
        let subscribers: EventSubscribers = Arc::new(Mutex::new(HashMap::default()));
        let event = json!({
            "method": "Target.attachedToTarget",
            "params": {
                "sessionId": session_id,
                "targetInfo": { "targetId": target_id, "type": "page" },
            },
        })
        .to_string();
        dispatch(&event, &pending, &subscribers, page_session);
    }

    #[test]
    fn first_page_attach_captures_session_and_pins_target() {
        let page_session: PageSession = Default::default();
        dispatch_attached(&page_session, "tab-1", "session-a");
        let state = page_session.lock();
        assert_eq!(state.session_id.as_deref(), Some("session-a"));
        assert_eq!(state.target_id.as_deref(), Some("tab-1"));
    }

    #[test]
    fn popup_attach_does_not_hijack_the_session() {
        // window.open / target=_blank creates a new page target which browser-level auto-attach
        // also delivers; adopting it would reroute all input/acks to the invisible popup.
        let page_session: PageSession = Default::default();
        dispatch_attached(&page_session, "tab-1", "session-a");
        dispatch_attached(&page_session, "popup-2", "session-b");
        let state = page_session.lock();
        assert_eq!(state.session_id.as_deref(), Some("session-a"));
        assert_eq!(state.target_id.as_deref(), Some("tab-1"));
    }

    #[test]
    fn reload_reattach_to_same_target_updates_the_session() {
        // A reload keeps the tab's targetId but issues a fresh sessionId — that one must be adopted
        // or the preview dies after refresh.
        let page_session: PageSession = Default::default();
        dispatch_attached(&page_session, "tab-1", "session-a");
        dispatch_attached(&page_session, "tab-1", "session-c");
        let state = page_session.lock();
        assert_eq!(state.session_id.as_deref(), Some("session-c"));
        assert_eq!(state.target_id.as_deref(), Some("tab-1"));
    }
}
