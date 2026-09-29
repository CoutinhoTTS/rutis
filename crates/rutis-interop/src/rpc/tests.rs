use super::*;
use serde_json::json;
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
        stream.write_all(&vec![0; 8 * 1024 * 1024])
    });
    blocked.recv().unwrap();
    peer.close(Error::Transport("explicit close".into()));
    assert!(writer.join().unwrap().is_err());
    peer.closed().await;
}
