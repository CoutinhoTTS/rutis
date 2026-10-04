"""`python3 -m rutis_runtime <socket> <project>`: one runtime process."""

import asyncio
import logging
import os
import socket
import sys

from .peer import Peer
from .runner import Runtime


async def run(socket_path: str, project: str) -> None:
    if project and project not in sys.path:
        sys.path.insert(0, project)
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.connect(socket_path)
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
        sys.exit("usage: python3 -m rutis_runtime <socket> <project>")
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
