use super::*;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct NoExports;
impl Dispatch for NoExports {
    fn invoke(&self, _: &Connection, _: &str, _: &str, _: Value) -> Reply {
        Err(Error::Value("no exports".into()))
    }
}
fn read(reader: &mut BufReader<UnixStream>) -> Frame {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}
fn send(writer: &mut UnixStream, frame: Frame) {
    serde_json::to_writer(&mut *writer, &frame).unwrap();
    writer.write_all(b"\n").unwrap();
}
fn pair() -> (Connection, UnixStream) {
    let (local, remote) = UnixStream::pair().unwrap();
    remote
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    (
        Connection::connect(local, Arc::new(NoExports)).unwrap(),
        remote,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn admitted_call_pins_its_target_before_a_following_counted_release() {
    let (peer, mut remote) = pair();
    let remote = std::thread::spawn(move || {
        let mut reader = BufReader::new(remote.try_clone().unwrap());
        assert!(matches!(
            read(&mut reader),
            Frame::Hello { version: VERSION }
        ));
        send(&mut remote, Frame::Hello { version: VERSION });
        let Frame::Invoke {
            id,
            args: WireValue::List(args),
            ..
        } = read(&mut reader)
        else {
            panic!("invoke expected")
        };
        let WireValue::Reference { id: reference, .. } = &args[0] else {
            panic!("ref expected")
        };
        assert!(matches!(&args[1], WireValue::Reference { id, .. } if id == reference));
        // Unrelated work stays queued while the current_thread caller blocks.
        send(
            &mut remote,
            Frame::Call {
                method: None,
                id: "node:1".into(),
                path: vec![],
                reference: *reference,
                args: WireValue::Data(json!([])),
            },
        );
        send(
            &mut remote,
            Frame::Release {
                reference: *reference,
                count: 2,
            },
        );
        send(
            &mut remote,
            Frame::Return {
                id,
                value: WireValue::Undefined,
            },
        );
        assert!(matches!(read(&mut reader), Frame::Return { id, .. } if id == "node:1"));
    });
    peer.ready().await.unwrap();
    let called = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    struct Guard(Arc<AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let guard = Guard(dropped.clone());
    let observed = called.clone();
    let callback = Value::callback(move |_| {
        assert!(!guard.0.load(Ordering::SeqCst));
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(Value::Undefined)
    });
    peer.invoke("", "test", Value::List(vec![callback.clone(), callback]))
        .unwrap();
    assert_eq!(called.load(Ordering::SeqCst), 0);
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(peer.0.exports.lock().unwrap().entries.is_empty());
    peer.drain().await;
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert!(dropped.load(Ordering::SeqCst));
    remote.join().unwrap();
    peer.closed().await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_old_release_does_not_remove_a_concurrent_new_grant() {
    let (peer, mut remote) = pair();
    let remote = std::thread::spawn(move || {
        let mut reader = BufReader::new(remote.try_clone().unwrap());
        read(&mut reader);
        send(&mut remote, Frame::Hello { version: VERSION });
        let Frame::Invoke {
            id,
            args: WireValue::Reference { id: reference, .. },
            ..
        } = read(&mut reader)
        else {
            panic!("reference expected")
        };
        send(
            &mut remote,
            Frame::Return {
                id,
                value: WireValue::Undefined,
            },
        );
        let Frame::Invoke {
            id,
            args: WireValue::Reference { id: again, .. },
            ..
        } = read(&mut reader)
        else {
            panic!("second grant expected")
        };
        assert_eq!(again, reference);
        send(
            &mut remote,
            Frame::Release {
                reference,
                count: 1,
            },
        );
        send(
            &mut remote,
            Frame::Call {
                method: None,
                id: "node:1".into(),
                path: vec![id.clone()],
                reference,
                args: WireValue::Data(json!([])),
            },
        );
        assert!(matches!(read(&mut reader), Frame::Return { id, .. } if id == "node:1"));
        send(
            &mut remote,
            Frame::Release {
                reference,
                count: 1,
            },
        );
        send(
            &mut remote,
            Frame::Return {
                id,
                value: WireValue::Undefined,
            },
        );
    });
    peer.ready().await.unwrap();
    let called = Arc::new(AtomicUsize::new(0));
    let observed = called.clone();
    let callback = Value::callback(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(Value::Undefined)
    });
    peer.invoke("", "test", callback.clone()).unwrap();
    peer.invoke("", "test", callback).unwrap();
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert!(peer.0.exports.lock().unwrap().entries.is_empty());
    remote.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn handshake_and_release_count_fail_closed() {
    let (peer, mut remote) = pair();
    send(
        &mut remote,
        Frame::Hello {
            version: VERSION + 1,
        },
    );
    assert!(peer.ready().await.is_err());
    peer.closed().await;

    let (peer, mut remote) = pair();
    send(&mut remote, Frame::Hello { version: VERSION });
    peer.ready().await.unwrap();
    let reference = peer
        .encode(&Value::callback(|_| Ok(Value::Undefined)))
        .unwrap();
    let WireValue::Reference { id, .. } = reference else {
        unreachable!()
    };
    send(
        &mut remote,
        Frame::Release {
            reference: id,
            count: 2,
        },
    );
    peer.closed().await;
    assert!(peer.0.exports.lock().unwrap().entries.is_empty());
    assert!(peer.invoke("", "test", Value::Undefined).is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_close_interrupts_a_writer_whose_peer_stopped_reading() {
    let (peer, mut remote) = pair();
    send(&mut remote, Frame::Hello { version: VERSION });
    peer.ready().await.unwrap();
    let (entered, blocked) = mpsc::channel();
    let writing = peer.clone();
    let writer = std::thread::spawn(move || {
        let mut stream = writing.0.writer.lock().unwrap();
        entered.send(()).unwrap();
        stream.send(&vec![b'x'; 8 * 1024 * 1024])
    });
    blocked.recv().unwrap();
    peer.close(Error::Transport("explicit close".into()));
    assert!(writer.join().unwrap().is_err());
    peer.closed().await;
}

/// A scripted peer on the far end of a connection, after the handshake.
struct Remote {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}
impl Remote {
    fn start(stream: UnixStream) -> Self {
        let mut remote = Self {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        };
        assert!(matches!(remote.read(), Frame::Hello { version: VERSION }));
        remote.send(Frame::Hello { version: VERSION });
        remote
    }
    fn read(&mut self) -> Frame {
        read(&mut self.reader)
    }
    fn send(&mut self, frame: Frame) {
        send(&mut self.writer, frame)
    }
    fn ret(&mut self, id: String, value: WireValue) {
        self.send(Frame::Return { id, value })
    }
    /// Make call `node:{n}` and read its return value.
    fn call(&mut self, n: u64, frame: Frame) -> Json {
        self.send(frame);
        match self.read() {
            Frame::Return { id, value } if id == format!("node:{n}") => data_of(value),
            _ => panic!("return of node:{n} expected"),
        }
    }
}
fn reference(id: u64, kind: Kind, home: bool) -> WireValue {
    WireValue::Reference {
        id,
        kind,
        home,
        origin: vec![],
    }
}
fn data(value: Json) -> WireValue {
    WireValue::Data(value)
}
fn data_of(value: WireValue) -> Json {
    match value {
        WireValue::Data(value) => value,
        _ => panic!("data expected"),
    }
}
fn granted(value: &WireValue) -> (u64, Kind) {
    match value {
        WireValue::Reference {
            id,
            kind,
            home: false,
            ..
        } => (*id, *kind),
        _ => panic!("granted reference expected"),
    }
}
fn ids(path: &[&str]) -> Vec<String> {
    path.iter().map(|id| id.to_string()).collect()
}
fn connected(dispatch: Arc<dyn Dispatch>) -> (Connection, UnixStream) {
    let (local, remote) = UnixStream::pair().unwrap();
    remote
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    (Connection::connect(local, dispatch).unwrap(), remote)
}

#[test]
fn rebase_tags_the_source_restores_the_target_and_keeps_third_sessions() {
    let path = ids(&["node:3", "rust:5", "s2/node:1", "s7/rust:2"]);
    assert_eq!(
        rebase(&path, "s1", "s2"),
        ids(&["s1/node:3", "s1/rust:5", "node:1", "s7/rust:2"])
    );
    // Forwarding back restores the original chain.
    assert_eq!(rebase(&rebase(&path, "s1", "s2"), "s2", "s1"), path);
    assert!(rebase(&[], "s1", "s2").is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn sessions_have_distinct_tags_and_forward_rebases_the_current_chain() {
    let (a, _ra) = pair();
    let (b, _rb) = pair();
    assert_ne!(a.tag(), b.tag());
    assert!(a.tag().starts_with('s') && !a.tag().contains('/'));
    let _path = PathGuard::enter(ids(&["node:1"]));
    let expected = vec![format!("{}/node:1", a.tag())];
    assert_eq!(b.forward(&a, current_path), expected);
    assert_eq!(
        b.forward_async(&a, async { current_path() }).await,
        expected
    );
    assert_eq!(current_path(), ids(&["node:1"]));
}

#[tokio::test(flavor = "current_thread")]
async fn references_forwarded_to_another_session_reach_their_owner() {
    let (a, ra) = pair();
    let (b, rb) = pair();
    let b_tag = b.tag().to_owned();
    // A owns a function (1), a future (2) and an object (3).
    let owner = std::thread::spawn(move || {
        let mut a = Remote::start(ra);
        let Frame::Invoke { id, .. } = a.read() else {
            panic!("invoke expected")
        };
        a.ret(
            id,
            WireValue::List(vec![
                reference(1, Kind::Function, false),
                reference(2, Kind::Future, false),
                reference(3, Kind::Object, false),
            ]),
        );
        let mut paths = Vec::new();
        let mut released = Vec::new();
        while released.len() < 3 {
            match a.read() {
                Frame::Call {
                    id,
                    path,
                    reference: 1,
                    method: None,
                    args,
                } => {
                    let n = data_of(args)[0].as_i64().unwrap();
                    paths.push(path);
                    a.ret(id, data(json!(n * 2)));
                }
                Frame::Await {
                    id,
                    path,
                    reference: 2,
                } => {
                    paths.push(path);
                    a.ret(id, data(json!(7)));
                }
                Frame::Call {
                    id,
                    reference: 3,
                    method: Some(method),
                    ..
                } => a.ret(id, data(json!(method))),
                Frame::Get {
                    id,
                    reference: 3,
                    property,
                    ..
                } => a.ret(id, data(json!(property))),
                Frame::Invoke { id, args, .. } => {
                    // The relay came home through Rust: A gets its own id.
                    assert!(matches!(
                        args,
                        WireValue::Reference {
                            id: 1,
                            home: true,
                            ..
                        }
                    ));
                    a.ret(id, WireValue::Undefined);
                }
                Frame::Release { reference, count } => {
                    assert_eq!(count, 1);
                    released.push(reference);
                }
                _ => panic!("unexpected frame"),
            }
        }
        released.sort();
        assert_eq!(released, vec![1, 2, 3]);
        paths
    });
    let user = std::thread::spawn(move || {
        let mut b = Remote::start(rb);
        let Frame::Invoke {
            id,
            args: WireValue::List(args),
            ..
        } = b.read()
        else {
            panic!("invoke expected")
        };
        let (function, kind) = granted(&args[0]);
        assert_eq!(kind, Kind::Function);
        let (future, kind) = granted(&args[1]);
        assert_eq!(kind, Kind::Future);
        let (object, kind) = granted(&args[2]);
        assert_eq!(kind, Kind::Object);
        // The same import forwarded twice is one reference.
        assert_eq!(granted(&args[3]).0, function);
        // Pumped by Rust's synchronous waiter: forwarded synchronously.
        let path = vec![id.clone()];
        let call = Frame::Call {
            id: "node:1".into(),
            path: path.clone(),
            reference: function,
            method: None,
            args: data(json!([20])),
        };
        assert_eq!(b.call(1, call), json!(40));
        let wait = Frame::Await {
            id: "node:2".into(),
            path: path.clone(),
            reference: future,
        };
        assert_eq!(b.call(2, wait), json!(7));
        let method = Frame::Call {
            id: "node:3".into(),
            path: path.clone(),
            reference: object,
            method: Some("greet".into()),
            args: data(json!([])),
        };
        assert_eq!(b.call(3, method), json!("greet"));
        let get = Frame::Get {
            id: "node:4".into(),
            path: path.clone(),
            reference: object,
            property: "name".into(),
        };
        assert_eq!(b.call(4, get), json!("name"));
        // Outside any Rust call chain: forwarded asynchronously.
        let unrelated = Frame::Call {
            id: "node:5".into(),
            path: vec![],
            reference: function,
            method: None,
            args: data(json!([1])),
        };
        assert_eq!(b.call(5, unrelated), json!(2));
        // A mismatched operation is refused without reaching the owner,
        // which would fail its session on it.
        b.send(Frame::Await {
            id: "node:6".into(),
            path: vec![],
            reference: function,
        });
        assert!(matches!(b.read(), Frame::Throw { id, .. } if id == "node:6"));
        b.ret(id, WireValue::Undefined);
        // Forwarded again later: still the same reference. Send it home.
        let Frame::Invoke {
            id,
            args: WireValue::List(args),
            ..
        } = b.read()
        else {
            panic!("invoke expected")
        };
        assert_eq!(granted(&args[0]).0, function);
        b.ret(id, reference(function, Kind::Function, true));
        for (reference, count) in [(function, 3), (future, 1), (object, 1)] {
            b.send(Frame::Release { reference, count });
        }
    });
    a.ready().await.unwrap();
    b.ready().await.unwrap();
    let references = a
        .invoke("", "references", Value::Undefined)
        .unwrap()
        .list()
        .unwrap();
    let mut args = references.clone();
    args.push(references[0].clone());
    b.invoke("", "use", Value::List(args)).unwrap();
    let returned = b
        .invoke("", "again", Value::List(vec![references[0].clone()]))
        .unwrap()
        .reference()
        .unwrap();
    // The relay decodes back to the import it forwards to.
    assert!(returned == references[0].clone().reference().unwrap());
    a.invoke("", "home", Value::Reference(returned)).unwrap();
    drop(references);
    user.join().unwrap();
    // B released every relay: their imports drop and A gets its releases.
    let paths = tokio::task::spawn_blocking(move || owner.join().unwrap())
        .await
        .unwrap();
    // B's chain reaches A tagged with B's session.
    let tagged = |id: &str| format!("{b_tag}/{id}");
    assert_eq!(paths[0], vec![tagged("rust:1"), tagged("node:1")]);
    assert_eq!(paths[1], vec![tagged("rust:1"), tagged("node:2")]);
    assert_eq!(paths[2], vec![tagged("node:5")]);
    let exports = b.0.exports.lock().unwrap();
    assert!(exports.entries.is_empty() && exports.relays.is_empty());
}

/// Both sessions have a synchronous call `node:1` outstanding. A's call is
/// forwarded to B, and B synchronously calls back a function from A: the
/// callback reaches the Rust thread serving A's `node:1` (no deadlock), A
/// finds its own `node:1` in the chain, and B's `node:1` stays untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reverse_call_through_a_relay_reaches_the_waiting_session() {
    struct Hold(Mutex<mpsc::Receiver<()>>);
    impl Dispatch for Hold {
        fn invoke(&self, _: &Connection, _: &str, _: &str, _: Value) -> Reply {
            self.0.lock().unwrap().recv().unwrap();
            Ok(json!("held").into())
        }
    }
    struct Forward(Connection);
    impl Dispatch for Forward {
        fn invoke(&self, peer: &Connection, _: &str, _: &str, args: Value) -> Reply {
            let target = &self.0;
            target.forward(peer, || target.invoke("svc", "run", args))
        }
    }
    let (release, held) = mpsc::channel();
    let (b, rb) = connected(Arc::new(Hold(Mutex::new(held))));
    let (a, ra) = connected(Arc::new(Forward(b.clone())));
    let (a_tag, b_tag) = (a.tag().to_owned(), b.tag().to_owned());

    let (b_waiting, b_waits) = mpsc::channel();
    let (b_ready, b_readied) = mpsc::channel();
    let user = std::thread::spawn(move || {
        let mut b = Remote::start(rb);
        // B's own synchronous call node:1, held open by Rust.
        b.send(Frame::Invoke {
            id: "node:1".into(),
            path: vec![],
            target: "svc".into(),
            method: "hold".into(),
            args: data(json!([])),
        });
        b_waiting.send(()).unwrap();
        let Frame::Invoke {
            id,
            path,
            args: WireValue::List(args),
            ..
        } = b.read()
        else {
            panic!("forwarded invoke expected")
        };
        // A's node:1 is tagged, so B cannot take it for its own node:1.
        assert_eq!(path, vec![format!("{a_tag}/node:1")]);
        let (function, _) = granted(&args[0]);
        // B calls back synchronously, inside the forwarded call.
        let mut chain = path;
        chain.push(id.clone());
        let callback = Frame::Call {
            id: "node:2".into(),
            path: chain,
            reference: function,
            method: None,
            args: data(json!([4])),
        };
        let value = b.call(2, callback);
        b.ret(id, data(value));
        b
    });
    let owner = std::thread::spawn(move || {
        let mut a = Remote::start(ra);
        b_waits.recv().unwrap();
        // Rust must have read B's handshake before A's call is forwarded
        // there; it reads it on B's reader thread, apart from this one.
        b_readied.recv().unwrap();
        a.send(Frame::Invoke {
            id: "node:1".into(),
            path: vec![],
            target: "svc".into(),
            method: "forward".into(),
            args: WireValue::List(vec![reference(1, Kind::Function, false)]),
        });
        let frame = a.read();
        let Frame::Call {
            id,
            path,
            reference: 1,
            args,
            ..
        } = frame
        else {
            panic!(
                "callback expected, got {}",
                serde_json::to_string(&frame).unwrap()
            )
        };
        // A finds its own waiting node:1 in the chain.
        assert_eq!(
            path,
            vec![
                "node:1".to_owned(),
                format!("{b_tag}/rust:1"),
                format!("{b_tag}/node:2")
            ]
        );
        let n = data_of(args)[0].as_i64().unwrap();
        a.ret(id, data(json!(n + 1)));
        let Frame::Return { id, value } = a.read() else {
            panic!("forward return expected")
        };
        assert_eq!((id.as_str(), data_of(value)), ("node:1", json!(5)));
        a
    });
    b.ready().await.unwrap();
    b_ready.send(()).unwrap();
    a.ready().await.unwrap();
    let mut user = tokio::task::spawn_blocking(move || user.join().unwrap())
        .await
        .unwrap();
    let _owner = tokio::task::spawn_blocking(move || owner.join().unwrap())
        .await
        .unwrap();
    // B's own node:1 was never answered on the way.
    release.send(()).unwrap();
    let Frame::Return { id, value } = user.read() else {
        panic!("held return expected")
    };
    assert_eq!((id.as_str(), data_of(value)), ("node:1", json!("held")));
}
