//! An interop session needs nothing of Unix: a session runs on one end of
//! a memory channel pair, with the far end scripted as a Node runtime.
use std::sync::Arc;

use rutis_bridge::channel::Channel;
use rutis_bridge::session::{Connection, Dispatch, Reply, Value};
use rutis_bridge::session::{Error, PROTOCOL};
use serde_json::{json, Value as Json};

struct Echo;
impl Dispatch for Echo {
    fn invoke(&self, _: &Connection, target: &str, method: &str, args: Value) -> Reply {
        Ok(Value::Data(
            json!({ "target": target, "method": method, "args": args.json()? }),
        ))
    }
}

fn read(channel: &mut Channel) -> Json {
    serde_json::from_slice(&channel.receiver.recv().unwrap().unwrap()).unwrap()
}
fn send(channel: &mut Channel, frame: Json) {
    channel
        .sender
        .send(&serde_json::to_vec(&frame).unwrap())
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_runs_on_a_memory_channel() {
    let (local, mut remote) = rutis_bridge::transport::memory::pair();
    let session = Connection::open(local, Arc::new(Echo)).unwrap();
    let far = std::thread::spawn(move || {
        assert_eq!(
            read(&mut remote),
            json!({ "op": "hello", "version": PROTOCOL })
        );
        send(&mut remote, json!({ "op": "hello", "version": PROTOCOL }));
        // A call from here reaches the far end.
        let call = read(&mut remote);
        assert_eq!(
            (call["op"].as_str(), call["method"].as_str()),
            (Some("invoke"), Some("back"))
        );
        send(
            &mut remote,
            json!({ "op": "return", "id": call["id"], "value": { "type": "data", "value": 7 } }),
        );
        // A call from the far end is dispatched here.
        send(
            &mut remote,
            json!({ "op": "invoke", "id": "node:1", "path": [], "target": "svc",
                    "method": "ping", "args": { "type": "data", "value": [1] } }),
        );
        let reply = read(&mut remote);
        assert_eq!(reply["op"], "return");
        assert_eq!(reply["value"]["value"]["method"], "ping");
        remote
    });
    session.ready().await.unwrap();
    let reply = session
        .invoke_async("", "back", Value::Undefined)
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(reply, json!(7));

    // Closing the session closes the channel: the far end sees its end.
    let mut remote = far.join().unwrap();
    session.close(Error::Transport("done".into()));
    session.closed().await;
    assert!(matches!(remote.receiver.recv(), Ok(None)));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_far_end_closing_ends_the_session() {
    let (local, mut remote) = rutis_bridge::transport::memory::pair();
    let session = Connection::open(local, Arc::new(Echo)).unwrap();
    read(&mut remote);
    send(&mut remote, json!({ "op": "hello", "version": PROTOCOL }));
    session.ready().await.unwrap();
    remote.closer.close("bye");
    session.closed().await;
    let error = session
        .invoke_async("", "x", Value::Undefined)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "peer disconnected");
}
