//! One WebSocket connection as a [`Channel`]: a task on the transport's
//! runtime moves messages between the socket and two pipes, pings, watches
//! for silence, and closes with the code the situation calls for.
//!
//! One physical connection carries one logical channel, so close codes and
//! heartbeats act on the connection.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::channel::{Channel, ChannelError, ChannelInfo, Closer, Receiver, Sender};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::WebSocketStream;

use crate::transport::websocket::pipe::{Ending, Pipe};
use crate::transport::websocket::Limits;

/// Orderly close.
pub(crate) const GOING_AWAY: u16 = 1001;
/// A message over the size limit.
pub(crate) const TOO_BIG: u16 = 1009;
/// A newer connection of the same endpoint took over.
pub(crate) const REPLACED: u16 = 4002;
/// Close frame reasons are at most 123 bytes.
const REASON_LIMIT: usize = 123;

/// Any byte stream a WebSocket can run on: TCP, or TLS over TCP.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}

/// A close the local side asked for.
#[derive(Default)]
struct Control {
    request: Mutex<Option<(u16, String)>>,
    notify: Notify,
}

impl Control {
    fn request(&self, code: u16, reason: &str) {
        self.request
            .lock()
            .unwrap()
            .get_or_insert_with(|| (code, reason.to_owned()));
        self.notify.notify_one();
    }

    async fn requested(&self) -> (u16, String) {
        loop {
            let notified = self.notify.notified();
            if let Some(request) = self.request.lock().unwrap().clone() {
                return request;
            }
            notified.await;
        }
    }
}

struct Shared {
    outgoing: Pipe,
    incoming: Pipe,
    control: Control,
    max_message: usize,
}

impl Shared {
    /// Close locally: both pipes end at once, so blocked callers wake even
    /// before the task sends the close frame.
    fn close(&self, code: u16, reason: &str) {
        self.control.request(code, reason);
        let ending = Ending::Failed(reason.to_owned());
        self.outgoing.close(ending.clone());
        self.incoming.close(ending);
    }
}

struct WsSender(Arc<Shared>);
impl Sender for WsSender {
    fn send(&mut self, message: &[u8]) -> Result<(), ChannelError> {
        if message.len() > self.0.max_message {
            let reason = format!(
                "message of {} bytes exceeds the limit of {}",
                message.len(),
                self.0.max_message
            );
            self.0.close(TOO_BIG, &reason);
            return Err(ChannelError::Closed { reason });
        }
        self.0.outgoing.push_blocking(message)
    }
}

struct WsReceiver(Arc<Shared>);
impl Receiver for WsReceiver {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        self.0.incoming.pop_blocking()
    }
}

struct WsCloser(Arc<Shared>);
impl Closer for WsCloser {
    fn close(&self, reason: &str) {
        self.0.close(GOING_AWAY, reason);
    }

    fn replaced(&self) {
        self.0.close(REPLACED, "replaced by a new connection");
    }
}

/// Counts a running connection task for the transport's shutdown.
pub(crate) struct Live(pub Arc<crate::transport::websocket::Shared>);
impl Live {
    pub(crate) fn new(transport: Arc<crate::transport::websocket::Shared>) -> Self {
        transport
            .live
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(transport)
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        self.0
            .live
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        self.0.ended.notify_waiters();
    }
}

/// Run `socket` as a channel on `runtime`.
pub(crate) fn channel<S: Io>(
    socket: WebSocketStream<S>,
    info: ChannelInfo,
    limits: &Limits,
    runtime: &tokio::runtime::Handle,
    live: Live,
) -> Channel {
    let shared = Arc::new(Shared {
        outgoing: Pipe::new(limits.buffer),
        incoming: Pipe::new(limits.buffer),
        control: Control::default(),
        max_message: limits.max_message,
    });
    let task = run(socket, shared.clone(), limits.ping, limits.timeout);
    runtime.spawn(async move {
        task.await;
        drop(live);
    });
    Channel {
        sender: Box::new(WsSender(shared.clone())),
        receiver: Box::new(WsReceiver(shared.clone())),
        closer: Arc::new(WsCloser(shared)),
        info,
    }
}

fn truncate(reason: &str) -> String {
    let mut end = reason.len().min(REASON_LIMIT);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

fn close_frame(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: truncate(reason).into(),
    }))
}

/// How the far end's close reads to the receiving side.
fn far_close(frame: Option<CloseFrame>) -> Ending {
    match frame {
        None => Ending::Finished,
        Some(frame) => match u16::from(frame.code) {
            1000 | GOING_AWAY => Ending::Finished,
            REPLACED => Ending::Failed("replaced by a new connection".into()),
            code => Ending::Failed(format!("closed by the far end ({code}): {}", frame.reason)),
        },
    }
}

async fn run<S: Io>(
    socket: WebSocketStream<S>,
    shared: Arc<Shared>,
    ping: Duration,
    timeout: Duration,
) {
    let (mut sink, mut stream) = socket.split();
    let mut ticker = tokio::time::interval_at(Instant::now() + ping, ping);
    let mut last_seen = Instant::now();
    let ending = loop {
        tokio::select! {
            (code, reason) = shared.control.requested() => {
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    sink.send(close_frame(code, &reason)),
                ).await;
                break Ending::Failed(reason);
            }
            message = shared.outgoing.pop() => {
                let Some(message) = message else { continue };
                let text = match String::from_utf8(message) {
                    Ok(text) => text,
                    Err(_) => {
                        let reason = "a message is not UTF-8 text";
                        let _ = sink.send(close_frame(1007, reason)).await;
                        break Ending::Failed(reason.into());
                    }
                };
                if let Err(error) = sink.send(Message::Text(text.into())).await {
                    break Ending::Failed(format!("send failed: {error}"));
                }
            }
            frame = stream.next() => {
                last_seen = Instant::now();
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        // Waits while the receiver lags: reading stops, and
                        // TCP pushes back on the far end.
                        if !shared.incoming.push(text.as_bytes().to_vec()).await {
                            continue;
                        }
                        last_seen = Instant::now();
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let reason = "binary messages are reserved for a binary encoding";
                        let _ = sink.send(close_frame(1003, reason)).await;
                        break Ending::Failed(reason.into());
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
                        // Answers to pings are queued by the protocol; flush them.
                        let _ = sink.flush().await;
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let _ = sink.close().await;
                        break far_close(frame);
                    }
                    Some(Err(WsError::Capacity(error))) => {
                        let reason = format!("received a message over the limit: {error}");
                        let _ = sink.send(close_frame(TOO_BIG, &reason)).await;
                        break Ending::Failed(reason);
                    }
                    Some(Err(error)) => break Ending::Failed(format!("connection failed: {error}")),
                    None => break Ending::Failed("connection lost".into()),
                }
            }
            _ = ticker.tick() => {
                if last_seen.elapsed() > timeout {
                    break Ending::Failed(format!(
                        "no message from the far end for {} s: heartbeat timeout",
                        timeout.as_secs_f32()
                    ));
                }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break Ending::Failed("connection lost".into());
                }
            }
        }
    };
    shared.outgoing.close(ending.clone());
    shared.incoming.close(ending);
}
