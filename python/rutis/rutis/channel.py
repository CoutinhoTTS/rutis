"""Channels a session runs on: ordered, reliable messages with their
boundaries kept, independent of the encoding of what they carry.

A channel has `send(message: bytes)`, `recv() -> bytes | None` (blocking;
`None` once the far end finished) and `close(reason)`. `recv` raises
`ConnectionError` when the channel failed.
"""

from __future__ import annotations

import socket
import threading


class SocketChannel:
    """A Unix socket (dialed, or inherited as fd 3), one message per line."""

    def __init__(self, sock: socket.socket):
        self._sock = sock
        self._reader = sock.makefile("rb")
        self._lock = threading.Lock()

    def send(self, message: bytes) -> None:
        if b"\n" in message:
            raise ValueError("a message contains a raw newline")
        with self._lock:
            self._sock.sendall(message + b"\n")

    def recv(self) -> bytes | None:
        line = self._reader.readline()
        if not line:
            return None
        if not line.endswith(b"\n"):
            raise ConnectionError("stream ended inside a message")
        return line[:-1]

    def close(self, reason: str = "") -> None:
        try:
            self._sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            self._sock.close()
        except OSError:
            pass
