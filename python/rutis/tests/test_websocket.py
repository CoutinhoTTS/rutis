"""The WebSocket binding of the Python runtime: authentication, the
subprotocol, close codes, the size limit and heartbeats."""

import os
import queue
import socket
import threading
import time
import unittest

try:
    from websockets.exceptions import ConnectionClosed, InvalidStatus
    from websockets.sync.client import connect

    from rutis import websocket
except ImportError:  # the optional `network` dependency is missing
    websocket = None

PROTOCOL = "rutis.2"


def listen(**options):
    """Start listen_once in a thread; return (address, channel queue)."""
    addresses: queue.Queue = queue.Queue()
    channels: queue.Queue = queue.Queue()

    def run():
        channels.put(
            websocket.listen_once(
                "ws://127.0.0.1:0/rutis",
                PROTOCOL,
                options.get("token", "secret"),
                announce=addresses.put,
            )
        )

    threading.Thread(target=run, daemon=True).start()
    return addresses.get(timeout=5), channels


def dial(address, token="secret", protocol=PROTOCOL, **options):
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    return connect(address, additional_headers=headers, subprotocols=[protocol], **options)


@unittest.skipIf(websocket is None, "needs the websockets package (rutis[network])")
class WebSocketTest(unittest.TestCase):
    def test_messages_cross_and_an_orderly_close_is_a_normal_end(self):
        address, channels = listen()
        client = dial(address)
        channel = channels.get(timeout=5)
        client.send('{"op":"hello"}')
        self.assertEqual(channel.recv(), b'{"op":"hello"}')
        channel.send(b'{"a":"x\\ny"}')
        self.assertEqual(client.recv(timeout=5), '{"a":"x\\ny"}')
        client.close(1001)
        self.assertIsNone(channel.recv())

    def test_wrong_or_missing_credentials_and_protocols_are_refused(self):
        address, _ = listen()
        cases = [("guess", PROTOCOL, 403), (None, PROTOCOL, 401), ("secret", "rutis.99", 400)]
        for token, protocol, status in cases:
            with self.subTest(token=token, protocol=protocol):
                with self.assertRaises(InvalidStatus) as refused:
                    dial(address, token=token, protocol=protocol)
                self.assertEqual(refused.exception.response.status_code, status)

    def test_a_takeover_closes_with_4002(self):
        address, channels = listen()
        client = dial(address)
        channels.get(timeout=5).replaced()
        with self.assertRaises(ConnectionClosed) as closed:
            client.recv(timeout=5)
        self.assertEqual(closed.exception.rcvd.code, 4002)

    def test_sending_over_the_limit_closes_with_1009(self):
        address, channels = listen()
        client = dial(address)
        channel = channels.get(timeout=5)
        limit, websocket.MAX_MESSAGE = websocket.MAX_MESSAGE, 64
        try:
            with self.assertRaises(ConnectionError):
                channel.send(b"x" * 65)
        finally:
            websocket.MAX_MESSAGE = limit
        with self.assertRaises(ConnectionClosed) as closed:
            client.recv(timeout=5)
        self.assertEqual(closed.exception.rcvd.code, 1009)

    def test_heartbeats_find_a_half_open_connection(self):
        os.environ["RUTIS_HEARTBEAT"] = "100,400"
        try:
            address, channels = listen()
        finally:
            del os.environ["RUTIS_HEARTBEAT"]
        relay, frozen = freezable_relay(int(address.rsplit(":", 1)[1].split("/")[0]))
        client = dial(f"ws://127.0.0.1:{relay}/rutis", ping_interval=None)
        channel = channels.get(timeout=5)
        client.send("{}")
        self.assertEqual(channel.recv(), b"{}")
        frozen.set()
        started = time.monotonic()
        with self.assertRaises(ConnectionError):
            channel.recv()
        self.assertLess(time.monotonic() - started, 3)


def freezable_relay(port):
    """A TCP relay that can stop carrying bytes without closing anything."""
    listener = socket.create_server(("127.0.0.1", 0))
    frozen = threading.Event()

    def pump(source, target):
        while data := source.recv(65536):
            if not frozen.is_set():
                target.sendall(data)

    def serve():
        client, _ = listener.accept()
        upstream = socket.create_connection(("127.0.0.1", port))
        for source, target in ((client, upstream), (upstream, client)):
            threading.Thread(target=pump, args=(source, target), daemon=True).start()

    threading.Thread(target=serve, daemon=True).start()
    return listener.getsockname()[1], frozen


if __name__ == "__main__":
    unittest.main()
