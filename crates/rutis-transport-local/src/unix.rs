use std::io::ErrorKind;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use rutis_channel::{Channel, ChannelInfo, Closer, ConnectError};

use crate::lines;

struct Shut(UnixStream);
impl Closer for Shut {
    fn close(&self, _reason: &str) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

pub(crate) async fn dial(path: &str) -> Result<Channel, ConnectError> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .and_then(|stream| stream.into_std())
        .and_then(|stream| {
            stream.set_nonblocking(false)?;
            Ok(stream)
        })
        .map_err(|error| classify(path, error))?;
    let split = || -> std::io::Result<_> { Ok((stream.try_clone()?, stream.try_clone()?)) };
    let (reader, closer) = split().map_err(|error| classify(path, error))?;
    Ok(lines::channel(
        reader,
        stream,
        Arc::new(Shut(closer)),
        ChannelInfo {
            transport: "unix",
            peer: None,
            label: path.to_owned(),
        },
    ))
}

/// The category comes from the error kind, never from its text.
fn classify(path: &str, error: std::io::Error) -> ConnectError {
    let reason = format!("{path}: {error}");
    match error.kind() {
        ErrorKind::PermissionDenied => ConnectError::AuthRejected { reason },
        _ => ConnectError::Retryable { reason },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn end(stream: UnixStream) -> Channel {
        let reader = stream.try_clone().unwrap();
        let closer = Arc::new(Shut(stream.try_clone().unwrap()));
        lines::channel(reader, stream, closer, ChannelInfo::default())
    }

    #[test]
    fn meets_the_channel_contract() {
        rutis_channel::testing::contract(|| {
            let (a, b) = UnixStream::pair().unwrap();
            (end(a), end(b))
        });
    }
}
