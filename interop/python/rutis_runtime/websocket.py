"""The WebSocket binding for the Python runtime: it listens, and the
controlling rutis dials (a leaf runtime keeps no reconnection of its own).

One connection carries one channel of UTF-8 JSON text messages; the
session protocol is the subprotocol; the controller presents a bearer token
in the Authorization header, never in the URL. Both sides ping; a far end
silent for 30 s is dropped. Messages are limited to 16 MiB (1009 over it).
Needs the `websockets` package: `pip install rutis-runtime[network]`.
"""

from __future__ import annotations

import hmac
import os
import queue
import ssl
import sys
import threading
from http import HTTPStatus
from urllib.parse import urlsplit

try:
    from websockets.exceptions import ConnectionClosed, ConnectionClosedOK
    from websockets.sync.server import serve
except ImportError as error:  # pragma: no cover - depends on the environment
    raise ImportError(
        "WebSocket channels need the websockets package: pip install rutis-runtime[network]"
    ) from error

MAX_MESSAGE = 16 * 1024 * 1024
GOING_AWAY, TOO_BIG, REPLACED = 1001, 1009, 4002


def _heartbeat() -> tuple[float, float]:
    """Ping interval and the extra wait for an answer, in seconds. Tests
    shorten them with RUTIS_INTEROP_HEARTBEAT=<ping ms>,<timeout ms>."""
    configured = os.environ.get("RUTIS_INTEROP_HEARTBEAT")
    if configured:
        ping, timeout = (int(part) / 1000 for part in configured.split(","))
        return ping, max(timeout - ping, 0.001)
    return 10.0, 20.0


class WebSocketChannel:
    def __init__(self, connection, ended: threading.Event):
        self._connection = connection
        self._ended = ended

    def send(self, message: bytes) -> None:
        if len(message) > MAX_MESSAGE:
            reason = f"message of {len(message)} bytes exceeds the limit of {MAX_MESSAGE}"
            self.close_with(TOO_BIG, reason)
            raise ConnectionError(reason)
        try:
            self._connection.send(message.decode("utf-8"))
        except ConnectionClosed as error:
            raise ConnectionError(_reason(error)) from error

    def recv(self) -> bytes | None:
        try:
            message = self._connection.recv()
        except ConnectionClosedOK:
            self._ended.set()
            return None
        except ConnectionClosed as error:
            self._ended.set()
            raise ConnectionError(_reason(error)) from error
        if isinstance(message, bytes):
            self.close_with(1003, "binary messages are reserved for a binary encoding")
            raise ConnectionError("received a binary message")
        return message.encode("utf-8")

    def close_with(self, code: int, reason: str) -> None:
        self._connection.close(code, _truncate(reason))
        self._ended.set()

    def close(self, reason: str = "") -> None:
        self.close_with(GOING_AWAY, reason)

    def replaced(self) -> None:
        self.close_with(REPLACED, "replaced by a new connection")


def _truncate(reason: str) -> str:
    encoded = reason.encode("utf-8")[:123]
    return encoded.decode("utf-8", errors="ignore")


def _reason(error: ConnectionClosed) -> str:
    frame = error.rcvd or error.sent
    if frame is None:
        return "connection lost"
    if frame.code == REPLACED:
        return "replaced by a new connection"
    return f"closed ({frame.code}): {frame.reason}"


def _print_address(address: str) -> None:
    print(f"rutis-interop: listening on {address}", file=sys.stderr, flush=True)


def listen_once(
    spec: str,
    protocol: str,
    token: str | None,
    cert: str | None = None,
    key: str | None = None,
    announce=_print_address,
):
    """Listen on `spec` (ws:// on loopback, or wss:// with `cert` and `key`)
    and return the channel of the first connection that presents `token` and
    speaks `protocol`. `announce` gets the bound address (stderr by default)."""
    url = urlsplit(spec)
    secure = url.scheme == "wss"
    host = url.hostname or "127.0.0.1"
    if url.scheme not in ("ws", "wss"):
        raise ValueError(f"{spec} is not a ws:// or wss:// address")
    if not secure and host not in ("127.0.0.1", "::1", "localhost"):
        raise ValueError(f"{spec}: only a loopback listener may go without TLS")
    context = None
    if secure:
        if not (cert and key):
            raise ValueError(f"{spec}: wss needs a certificate and key")
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert, key)
    path = url.path or "/"

    def check(connection, request):
        if request.path != path:
            return connection.respond(HTTPStatus.NOT_FOUND, "no rutis endpoint here\n")
        header = request.headers.get("Authorization", "")
        if not header.startswith("Bearer "):
            return connection.respond(HTTPStatus.UNAUTHORIZED, "credentials required\n")
        if token is None or not hmac.compare_digest(header[len("Bearer "):].encode(), token.encode()):
            return connection.respond(HTTPStatus.FORBIDDEN, "not accepted here\n")
        offered = [
            item.strip()
            for value in request.headers.get_all("Sec-WebSocket-Protocol")
            for item in value.split(",")
        ]
        if protocol not in offered:
            return connection.respond(HTTPStatus.BAD_REQUEST, f"this endpoint speaks {protocol}\n")
        return None

    accepted: queue.Queue = queue.Queue()
    taken = threading.Event()

    def handler(connection) -> None:
        if taken.is_set():
            connection.close(GOING_AWAY, "this endpoint serves one session")
            return
        taken.set()
        ended = threading.Event()
        accepted.put(WebSocketChannel(connection, ended))
        # Returning closes the connection: hold it until the session ends.
        ended.wait()

    ping, timeout = _heartbeat()
    server = serve(
        handler,
        host,
        url.port if url.port is not None else (443 if secure else 80),
        ssl=context,
        subprotocols=[protocol],
        process_request=check,
        compression=None,
        max_size=MAX_MESSAGE,
        ping_interval=ping,
        ping_timeout=timeout,
        # A far end that stopped answering will not finish a closing
        # handshake either: do not stretch detection past the timeout.
        close_timeout=min(ping, 1.0),
    )
    bound_host, bound_port = server.socket.getsockname()[:2]
    shown = f"[{bound_host}]" if ":" in bound_host else bound_host
    announce(f"{url.scheme}://{shown}:{bound_port}{path}")
    threading.Thread(target=server.serve_forever, name="rutis-websocket", daemon=True).start()
    channel = accepted.get()
    # One session: stop accepting; the established connection stays.
    threading.Thread(target=server.shutdown, kwargs={"close_connections": False}, daemon=True).start()
    return channel
