"""`python3 -m rutis_runtime <channel> [--id <endpoint>] [--peer <endpoint>] <project>`:
one runtime process.

The channel is `fd:<n>`, a socket inherited from the process that started
this one; `unix:<path>` (or a bare path), a socket to dial; or
`listen:ws://…` / `listen:wss://…`, a WebSocket address to listen on for the
controlling rutis (the token it must present in RUTIS_INTEROP_TOKEN, a
listener certificate and key in RUTIS_INTEROP_CERT and RUTIS_INTEROP_KEY).
Local channels speak the compat protocol; network channels the endpoint
format, as `--id`, expecting `--peer` as the controller when given.
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
        from .peer import ENDPOINT_PROTOCOL

        return websocket.listen_once(
            spec[len("listen:"):],
            f"rutis.{ENDPOINT_PROTOCOL}",
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


def parse(argv: list[str]) -> tuple[str, dict | None, str]:
    """The channel, the endpoint (network channels), and the project."""
    if not argv:
        raise ValueError("usage: python3 -m rutis_runtime <channel> [--id <endpoint>] [--peer <endpoint>] <project>")
    channel, rest, flags = argv[0], list(argv[1:]), {}
    while rest and rest[0].startswith("--"):
        flag = rest.pop(0)[2:]
        if not rest:
            raise ValueError(f"--{flag} needs a value")
        flags[flag] = rest.pop(0)
    if len(rest) != 1:
        raise ValueError("usage: python3 -m rutis_runtime <channel> [--id <endpoint>] [--peer <endpoint>] <project>")
    endpoint = None
    if channel.startswith("listen:"):
        if "id" not in flags:
            raise ValueError(f"a network channel needs --id <endpoint>: {channel}")
        # A runner is a runtime: the controller manages its rows.
        endpoint = {"local": flags["id"], "expected": flags.get("peer"), "declare": ["runtime"]}
    return channel, endpoint, rest[0]


async def run(channel: str, endpoint: dict | None, project: str) -> None:
    if project and project not in sys.path:
        sys.path.insert(0, project)
    # Listening may wait long for the controller: not on the event loop.
    connection = await asyncio.to_thread(open_channel, channel)
    runtime = Runtime()
    peer = Peer(connection, runtime.dispatch, settled=None, endpoint=endpoint)
    runtime.peer = peer
    peer.start()
    await peer.ready
    await peer.closed
    runtime.closing = True
    await runtime.dispose()


def main() -> None:
    try:
        channel, endpoint, project = parse(sys.argv[1:])
    except ValueError as error:
        sys.exit(str(error))
    try:
        asyncio.run(run(channel, endpoint, project))
    finally:
        # Stray plugin threads must not keep the process alive: rutis waits
        # for it to exit.
        sys.stdout.flush()
        sys.stderr.flush()
        logging.shutdown()
        os._exit(0)


main()
