//! Sessions on channels other than a Unix socket.

use super::*;
use rutis_channel::{ChannelInfo, Receiver};
use serde_json::json;

/// Returns the arguments it is called with.
struct Echo;
impl Dispatch for Echo {
    fn invoke(&self, _: &Connection, _: &str, _: &str, args: Value) -> Reply {
        Ok(args)
    }
}

/// The far end of a channel, speaking frames by hand.
struct Remote {
    sender: Box<dyn Sender>,
    receiver: Box<dyn Receiver>,
    closer: Arc<dyn Closer>,
}
impl Remote {
    fn new(channel: Channel) -> Self {
        Self {
            sender: channel.sender,
            receiver: channel.receiver,
            closer: channel.closer,
        }
    }
    fn read(&mut self) -> Frame {
        serde_json::from_slice(&self.receiver.recv().unwrap().unwrap()).unwrap()
    }
    fn send(&mut self, frame: Frame) {
        self.sender
            .send(serde_json::to_vec(&frame).unwrap())
            .unwrap();
    }
    fn handshake(&mut self) {
        assert!(matches!(self.read(), Frame::Hello { version: VERSION }));
        self.send(Frame::Hello { version: VERSION });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_runs_over_an_in_memory_channel() {
    let (local, remote) = rutis_channel::pair(16);
    let peer = Connection::open(local, Arc::new(Echo)).unwrap();
    let remote = std::thread::spawn(move || {
        let mut remote = Remote::new(remote);
        remote.handshake();
        // This side answers a call from the session.
        let Frame::Invoke {
            id, target, method, ..
        } = remote.read()
        else {
            panic!("invoke expected")
        };
        assert_eq!((target.as_str(), method.as_str()), ("svc", "ping"));
        remote.send(Frame::Return {
            id,
            value: WireValue::Data(json!("pong")),
        });
        // And calls the session.
        remote.send(Frame::Invoke {
            id: "node:1".into(),
            path: vec![],
            target: "svc".into(),
            method: "echo".into(),
            args: WireValue::Data(json!([1, 2])),
        });
        let Frame::Return { id, value } = remote.read() else {
            panic!("return expected")
        };
        assert_eq!(id, "node:1");
        assert!(matches!(value, WireValue::Data(value) if value == json!([1, 2])));
    });
    peer.ready().await.unwrap();
    let reply = peer
        .invoke_async("svc", "ping", Value::List(vec![]))
        .await
        .unwrap();
    assert_eq!(reply.json().unwrap(), json!("pong"));
    tokio::task::spawn_blocking(move || remote.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_ends_with_the_reason_its_channel_gives() {
    let (mut local, remote) = rutis_channel::pair(16);
    local.info = ChannelInfo::new("memory").with_label("peer mac");
    let peer = Connection::open(local, Arc::new(Echo)).unwrap();
    let mut remote = Remote::new(remote);
    remote.handshake();
    peer.ready().await.unwrap();
    remote.closer.close("the process exited");
    peer.closed().await;
    let error = peer
        .invoke_async("svc", "ping", Value::List(vec![]))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "peer mac: the process exited");
}

/// A sender that refuses messages above `limit` bytes and stays usable.
struct Limit {
    inner: Box<dyn Sender>,
    limit: usize,
}
impl Sender for Limit {
    fn send(&mut self, message: Vec<u8>) -> Result<(), ChannelError> {
        if message.len() > self.limit {
            return Err(ChannelError::TooLarge {
                limit: self.limit,
                size: message.len(),
            });
        }
        self.inner.send(message)
    }
}

/// A local end that refuses messages above 256 bytes, and its far end.
fn limited() -> (Channel, Remote) {
    let (local, remote) = rutis_channel::pair(16);
    let local = Channel {
        sender: Box::new(Limit {
            inner: local.sender,
            limit: 256,
        }),
        ..local
    };
    (local, Remote::new(remote))
}

fn big() -> Value {
    Value::List(vec![
        Value::Data(json!("x".repeat(512))),
        Value::callback(|_| Ok(Value::Undefined)),
    ])
}

fn refused(error: &Error) -> bool {
    matches!(error, Error::Value(message) if message.contains("exceeds the limit"))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_request_fails_alone() {
    let (local, mut remote) = limited();
    let peer = Connection::open(local, Arc::new(Echo)).unwrap();
    remote.handshake();
    peer.ready().await.unwrap();

    let error = peer.invoke_async("svc", "put", big()).await.unwrap_err();
    assert!(refused(&error), "{error}");
    let blocking = peer.clone();
    let error = tokio::task::spawn_blocking(move || blocking.invoke("svc", "put", big()))
        .await
        .unwrap()
        .unwrap_err();
    assert!(refused(&error), "{error}");
    {
        let calls = peer.0.calls.lock().unwrap();
        assert!(calls.closed.is_none(), "the session stays open");
        assert!(
            calls.waiting.is_empty(),
            "the refused calls are not waiting"
        );
    }
    assert!(
        peer.0.exports.lock().unwrap().entries.is_empty(),
        "the callbacks of the refused calls are not granted"
    );

    // The next request goes through; nothing of the refused ones did.
    let remote = std::thread::spawn(move || {
        let Frame::Invoke { id, method, .. } = remote.read() else {
            panic!("invoke expected")
        };
        assert_eq!(method, "ping");
        remote.send(Frame::Return {
            id,
            value: WireValue::Data(json!("pong")),
        });
    });
    let reply = peer
        .invoke_async("svc", "ping", Value::List(vec![]))
        .await
        .unwrap();
    assert_eq!(reply.json().unwrap(), json!("pong"));
    tokio::task::spawn_blocking(move || remote.join().unwrap())
        .await
        .unwrap();
}

/// Answers `big` with a value above the limit, anything else with its
/// arguments.
struct Answers;
impl Dispatch for Answers {
    fn invoke(&self, _: &Connection, _: &str, method: &str, args: Value) -> Reply {
        if method == "big" {
            Ok(big())
        } else {
            Ok(args)
        }
    }
}

// Current-thread: an answer, with the release of its grants, completes
// before the next call runs.
#[tokio::test(flavor = "current_thread")]
async fn an_oversized_answer_becomes_an_error_answer() {
    let (local, mut remote) = limited();
    let peer = Connection::open(local, Arc::new(Answers)).unwrap();
    remote.handshake();
    peer.ready().await.unwrap();
    let remote = std::thread::spawn(move || {
        remote.send(Frame::Invoke {
            id: "node:1".into(),
            path: vec![],
            target: "svc".into(),
            method: "big".into(),
            args: WireValue::Data(json!([])),
        });
        let Frame::Throw { id, error } = remote.read() else {
            panic!("an error answer expected")
        };
        assert_eq!(id, "node:1");
        assert!(
            error.message.contains("exceeds the limit"),
            "{}",
            error.message
        );
        // The session still answers.
        remote.send(Frame::Invoke {
            id: "node:2".into(),
            path: vec![],
            target: "svc".into(),
            method: "echo".into(),
            args: WireValue::Data(json!([1])),
        });
        let Frame::Return { id, .. } = remote.read() else {
            panic!("return expected")
        };
        assert_eq!(id, "node:2");
        // Kept open until the checks below: dropping it ends the session.
        remote
    });
    let _remote = tokio::task::spawn_blocking(move || remote.join().unwrap())
        .await
        .unwrap();
    assert!(peer.0.calls.lock().unwrap().closed.is_none());
    assert!(
        peer.0.exports.lock().unwrap().entries.is_empty(),
        "the callback of the refused answer is not granted"
    );
}

/// A sender whose channel ends after `left` messages.
struct BreaksAfter {
    inner: Box<dyn Sender>,
    left: usize,
}
impl Sender for BreaksAfter {
    fn send(&mut self, message: Vec<u8>) -> Result<(), ChannelError> {
        if self.left == 0 {
            return Err(ChannelError::closed("the channel broke"));
        }
        self.left -= 1;
        self.inner.send(message)
    }
}

#[test]
fn an_answer_on_an_ended_channel_closes_the_session() {
    // On a thread of its own, so a hang fails the test instead of the run.
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let (local, remote) = rutis_channel::pair(16);
            // The hello goes out; the answer below does not.
            let local = Channel {
                sender: Box::new(BreaksAfter {
                    inner: local.sender,
                    left: 1,
                }),
                ..local
            };
            let peer = Connection::open(local, Arc::new(Echo)).unwrap();
            let mut remote = Remote::new(remote);
            remote.handshake();
            peer.ready().await.unwrap();
            remote.send(Frame::Invoke {
                id: "node:1".into(),
                path: vec![],
                target: "svc".into(),
                method: "echo".into(),
                args: WireValue::Data(json!([])),
            });
            peer.closed().await;
            let error = peer
                .invoke_async("svc", "ping", Value::List(vec![]))
                .await
                .unwrap_err();
            done.send(error.to_string()).unwrap();
        });
    });
    let error = finished
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the session does not close after a failed answer");
    assert_eq!(error, "the channel broke");
}
