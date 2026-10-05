"""`python3 -m rutis_runtime <channel> <project>`: one runtime process.

The channel is `fd:<n>`, a socket inherited from the process that started
this one; `unix:<path>` (or a bare path), a socket to dial; or
`listen:ws://…` / `listen:wss://…`, a WebSocket address to listen on for the
controlling rutis (the token it must present in RUTIS_INTEROP_TOKEN, a
listener certificate and key in RUTIS_INTEROP_CERT and RUTIS_INTEROP_KEY).
"""

import asyncio
import logging
import os
import socket
import sys

from .peer import Peer
from .runner import Runtime


def open_channel(spec: str):
    if spec.startswith("listen:"):
        from . import websocket
        from .peer import PROTOCOL

        return websocket.listen_once(
            spec[len("listen:"):],
            f"rutis.{PROTOCOL}",
            os.environ.get("RUTIS_INTEROP_TOKEN"),
            os.environ.get("RUTIS_INTEROP_CERT"),
            os.environ.get("RUTIS_INTEROP_KEY"),
        )
    if spec.startswith("fd:"):
        return socket.socket(fileno=int(spec[len("fd:"):]))
    path = spec[len("unix:"):] if spec.startswith("unix:") else spec
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.connect(path)
    return connection


async def run(channel: str, project: str) -> None:
    if project and project not in sys.path:
        sys.path.insert(0, project)
    # Listening may wait long for the controller: not on the event loop.
    connection = await asyncio.to_thread(open_channel, channel)
    runtime = Runtime()
    peer = Peer(connection, runtime.dispatch, settled=None)
    runtime.peer = peer
    peer.start()
    await peer.ready
    await peer.closed
    runtime.closing = True
    await runtime.dispose()


def main() -> None:
    if len(sys.argv) < 3:
        sys.exit("usage: python3 -m rutis_runtime <channel> <project>")
    try:
        asyncio.run(run(sys.argv[1], sys.argv[2]))
    finally:
        # Stray plugin threads must not keep the process alive: rutis waits
        # for it to exit.
        sys.stdout.flush()
        sys.stderr.flush()
        logging.shutdown()
        os._exit(0)


main()
