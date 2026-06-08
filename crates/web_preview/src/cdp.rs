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
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{AsyncApp, BackgroundExecutor, Task};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
type EventSubscribers = Arc<Mutex<HashMap<String, Vec<mpsc::UnboundedSender<Value>>>>>;

/// A connected CDP session. Cloneable: all clones share the same underlying socket and dispatch
/// state, so the client can be handed to multiple feature modules (pick mode, CSS panel, …).
#[derive(Clone)]
pub struct CdpClient {
    inner: Arc<CdpInner>,
}

struct CdpInner {
    next_id: AtomicU64,
    pending: PendingMap,
    subscribers: EventSubscribers,
    outgoing: mpsc::UnboundedSender<WebSocketMessage>,
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
            async move {
                while let Some(message) = read.next().await {
                    match message {
                        Ok(WebSocketMessage::Text(text)) => {
                            dispatch(&text, &pending, &subscribers);
                        }
                        Ok(WebSocketMessage::Close(_)) => break,
                        Ok(_) => {}
                        Err(error) => {
                            log::warn!("CDP read failed, closing connection: {error:#}");
                            break;
                        }
                    }
                }
                // Connection ended: fail any in-flight requests so awaiters don't hang forever.
                let mut pending = pending.lock();
                for (_, sender) in pending.drain() {
                    let _ = sender.send(Err(anyhow!("CDP connection closed")));
                }
            }
        });

        Self {
            inner: Arc::new(CdpInner {
                next_id: AtomicU64::new(1),
                pending,
                subscribers,
                outgoing: outgoing_tx,
                _pump: Arc::new([read_task, write_task]),
            }),
        }
    }

    /// Send a CDP method call and await its result. `params` is the raw params object (use
    /// `serde_json::json!({...})`, or `Value::Null` for none).
    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(id, tx);

        let payload = json!({ "id": id, "method": method, "params": params });
        let text = serde_json::to_string(&payload).context("serializing CDP request")?;
        self.inner
            .outgoing
            .unbounded_send(WebSocketMessage::Text(text.into()))
            .map_err(|_| anyhow!("CDP connection is closed"))?;

        rx.await.context("CDP response channel dropped")?
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

fn dispatch(text: &str, pending: &PendingMap, subscribers: &EventSubscribers) {
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
