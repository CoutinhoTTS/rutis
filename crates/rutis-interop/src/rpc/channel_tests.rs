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
