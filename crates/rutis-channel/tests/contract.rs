//! The channel contract, run against every channel kind.

use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use rutis_channel::{Channel, ChannelError};

const WAIT: Duration = Duration::from_secs(5);

fn memory() -> (Channel, Channel) {
    rutis_channel::pair(4)
}

#[cfg(unix)]
fn unix() -> (Channel, Channel) {
    let (one, two) = std::os::unix::net::UnixStream::pair().unwrap();
    (Channel::unix(one).unwrap(), Channel::unix(two).unwrap())
}

/// The end of a channel, as a receiver reports it.
fn ended(result: Result<Option<Vec<u8>>, ChannelError>) -> bool {
    matches!(result, Ok(None) | Err(ChannelError::Closed { .. }))
}

macro_rules! contract {
    ($kind:ident) => {
        mod $kind {
            use super::*;

            #[test]
            fn messages_keep_their_order_under_concurrent_senders() {
                order(super::$kind());
            }

            #[test]
            fn messages_keep_their_boundaries() {
                boundaries(super::$kind());
            }

            #[test]
            fn close_wakes_a_blocked_receive() {
                close_wakes_receive(super::$kind());
            }

            #[test]
            fn close_wakes_a_blocked_send() {
                close_wakes_send(super::$kind());
            }

            #[test]
            fn the_peer_sees_the_end() {
                peer_sees_end(super::$kind());
            }

            #[test]
            fn close_is_idempotent() {
                idempotent(super::$kind());
            }

            #[test]
            fn an_end_reason_replaces_the_end() {
                end_reason(super::$kind());
            }
        }
    };
}

contract!(memory);
#[cfg(unix)]
contract!(unix);

fn order((one, two): (Channel, Channel)) {
    const THREADS: usize = 4;
    const EACH: u32 = 200;
    let sender = Arc::new(Mutex::new(one.sender));
    let senders: Vec<_> = (0..THREADS)
        .map(|thread| {
            let sender = sender.clone();
            thread::spawn(move || {
                for index in 0..EACH {
                    let message = format!("{thread}:{index}").into_bytes();
                    sender.lock().unwrap().send(message).unwrap();
                }
            })
        })
        .collect();
    let mut receiver = two.receiver;
    let mut next = [0; THREADS];
    for _ in 0..THREADS as u32 * EACH {
        let message = String::from_utf8(receiver.recv().unwrap().unwrap()).unwrap();
        let (thread, index) = message.split_once(':').unwrap();
        let thread: usize = thread.parse().unwrap();
        assert_eq!(index.parse::<u32>().unwrap(), next[thread], "{message}");
        next[thread] += 1;
    }
    for sender in senders {
        sender.join().unwrap();
    }
}

fn boundaries((one, two): (Channel, Channel)) {
    let messages: Vec<Vec<u8>> = [0, 1, 4096, 1 << 20]
        .into_iter()
        .map(|size| (0..size).map(|i| b'a' + (i % 26) as u8).collect())
        .collect();
    let expected = messages.clone();
    let mut sender = one.sender;
    // A message larger than a socket buffer needs a concurrent reader.
    let sending = thread::spawn(move || {
        for message in messages {
            sender.send(message).unwrap();
        }
        sender
    });
    let mut receiver = two.receiver;
    for message in expected {
        assert_eq!(receiver.recv().unwrap().unwrap(), message);
    }
    sending.join().unwrap();
}

fn close_wakes_receive((one, two): (Channel, Channel)) {
    let mut receiver = one.receiver;
    let (done, finished) = mpsc::channel();
    thread::spawn(move || done.send(receiver.recv()).unwrap());
    one.closer.close("closed by the test");
    let result = finished.recv_timeout(WAIT).expect("receive stays blocked");
    assert!(ended(result));
    drop(two);
}

fn close_wakes_send((one, two): (Channel, Channel)) {
    // Nobody reads `two`, so the sender fills the channel and blocks.
    let mut sender = one.sender;
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        let error = loop {
            if let Err(error) = sender.send(vec![b'x'; 64 * 1024]) {
                break error;
            }
        };
        done.send(error).unwrap();
    });
    one.closer.close("closed by the test");
    let error = finished.recv_timeout(WAIT).expect("send stays blocked");
    assert!(matches!(error, ChannelError::Closed { .. }), "{error:?}");
    drop(two);
}

fn peer_sees_end((one, two): (Channel, Channel)) {
    let mut sender = one.sender;
    sender.send(b"last".to_vec()).unwrap();
    one.closer.close("closed by the test");
    drop((sender, one.receiver));
    let mut receiver = two.receiver;
    // A stream may drop what it had buffered when it is shut down.
    let mut result = receiver.recv();
    if let Ok(Some(message)) = &result {
        assert_eq!(message, b"last");
        result = receiver.recv();
    }
    assert!(ended(result));
}

fn idempotent((one, two): (Channel, Channel)) {
    one.closer.close("first");
    one.closer.close("second");
    let mut sender = one.sender;
    assert!(matches!(
        sender.send(b"late".to_vec()),
        Err(ChannelError::Closed { .. })
    ));
    let mut receiver = one.receiver;
    assert!(ended(receiver.recv()));
    drop(two);
}

fn end_reason((one, two): (Channel, Channel)) {
    let two = two.with_end_reason(|| "the process exited".to_owned());
    one.closer.close("closed by the test");
    drop(one);
    let mut receiver = two.receiver;
    let closed = Err(ChannelError::closed("the process exited"));
    assert_eq!(receiver.recv(), closed);
    assert_eq!(receiver.recv(), closed, "the reason is reported again");
}

#[test]
fn memory_carries_the_close_reason_after_queued_messages() {
    let (one, two) = rutis_channel::pair(4);
    let mut sender = one.sender;
    sender.send(b"1".to_vec()).unwrap();
    sender.send(b"2".to_vec()).unwrap();
    one.closer.close("bye");
    let mut receiver = two.receiver;
    assert_eq!(receiver.recv(), Ok(Some(b"1".to_vec())));
    assert_eq!(receiver.recv(), Ok(Some(b"2".to_vec())));
    assert_eq!(receiver.recv(), Err(ChannelError::closed("bye")));
}

#[test]
fn memory_ends_when_the_peer_drops_its_channel() {
    let (one, two) = rutis_channel::pair(4);
    drop(one);
    let mut receiver = two.receiver;
    assert!(ended(receiver.recv()));
    let mut sender = two.sender;
    assert!(sender.send(b"lost".to_vec()).is_err());
}
