use super::*;
use std::time::Duration;

#[test]
fn delivers_in_order_with_boundaries_and_ends_normally() {
    let (mut a, mut b) = pair();
    for message in [&b"one"[..], b"", b"three\nlines"] {
        a.sender.send(message).unwrap();
    }
    drop(a.sender);
    assert_eq!(b.receiver.recv().unwrap().unwrap(), b"one");
    assert_eq!(b.receiver.recv().unwrap().unwrap(), b"");
    assert_eq!(b.receiver.recv().unwrap().unwrap(), b"three\nlines");
    assert_eq!(b.receiver.recv().unwrap(), None);
}

#[test]
fn a_full_buffer_blocks_the_sender_until_the_receiver_reads() {
    let (mut a, mut b) = pair();
    for _ in 0..CAPACITY {
        a.sender.send(b"x").unwrap();
    }
    let (sent, done) = mpsc::channel();
    let sender = std::thread::spawn(move || {
        a.sender.send(b"last").unwrap();
        sent.send(()).unwrap();
    });
    assert!(done.recv_timeout(Duration::from_millis(100)).is_err());
    b.receiver.recv().unwrap();
    done.recv_timeout(Duration::from_secs(3)).unwrap();
    sender.join().unwrap();
}

#[test]
fn close_is_idempotent_and_wakes_blocked_send_and_receive() {
    let (mut a, b) = pair();
    for _ in 0..CAPACITY {
        a.sender.send(b"x").unwrap();
    }
    let closer = a.closer.clone();
    let blocked_send = std::thread::spawn(move || a.sender.send(b"blocked"));
    let (mut c, _d) = pair();
    let closer_c = c.closer.clone();
    let blocked_recv = std::thread::spawn(move || c.receiver.recv());
    std::thread::sleep(Duration::from_millis(50));
    closer.close("first");
    closer.close("second");
    closer_c.close("done");
    assert_eq!(
        blocked_send.join().unwrap(),
        Err(ChannelError::Closed {
            reason: "first".into()
        })
    );
    assert_eq!(
        blocked_recv.join().unwrap(),
        Err(ChannelError::Closed {
            reason: "done".into()
        })
    );
    drop(b);
}

#[test]
fn the_far_end_sees_a_close_as_the_end_after_draining() {
    let (mut a, mut b) = pair();
    a.sender.send(b"before").unwrap();
    a.closer.close("bye");
    assert_eq!(b.receiver.recv().unwrap().unwrap(), b"before");
    assert_eq!(b.receiver.recv().unwrap(), None);
    assert!(b.sender.send(b"after").is_err());
}

#[tokio::test]
async fn dial_reaches_a_listener_and_unknown_names_are_retryable() {
    let transport = Arc::new(MemoryTransport::default());
    let listener = transport.listen("svc").unwrap();
    assert!(matches!(
        transport.listen("svc"),
        Err(ConnectError::Incompatible { .. })
    ));
    let mut dialed = transport.dial("svc").await.unwrap();
    let mut accepted = listener.accept().unwrap();
    dialed.sender.send(b"hi").unwrap();
    assert_eq!(accepted.receiver.recv().unwrap().unwrap(), b"hi");
    assert!(matches!(
        transport.dial("other").await,
        Err(ConnectError::Retryable { .. })
    ));
    transport.close_all();
    assert!(dialed.sender.send(b"after").is_err());
    drop(listener);
    assert!(transport.dial("svc").await.is_err());
}
