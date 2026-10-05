"""The session conformance target, served by the Python session: connects
to the Unix socket given as the first argument as endpoint `python`,
expecting `main`, and serves `conformance` until the session ends."""

import asyncio
import socket
import sys

from rutis_runtime.peer import Peer


class Conformance:
    def __init__(self) -> None:
        self.held = None
        self.aborted = False
        self.peer: Peer | None = None

    def echo(self, value):
        return value

    def apply(self, fn, value):
        return fn(value)

    async def later(self, value):
        await asyncio.sleep(0.005)
        return value

    def fail(self, name, message):
        error = type(name, (Exception,), {})(message)
        raise error

    def hold(self, fn):
        self.held = fn

    def fire(self, value):
        if self.held is None:
            raise LookupError("nothing held")
        return self.held(value)

    def drop(self):
        if self.held is not None:
            self.peer.release(self.held)
        self.held = None

    async def abortable(self, signal):
        try:
            await signal.wait()
        finally:
            # Cancelled: the signal is set, and the task is cancelled too.
            self.aborted = signal.cancelled

    def is_aborted(self):
        return self.aborted

    def reenter(self, fn):
        return fn()


async def main(path: str) -> None:
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.connect(path)
    service = Conformance()

    def dispatch(target, method, args):
        if target != "conformance":
            raise LookupError(f"no target {target}")
        name = "is_aborted" if method == "aborted" else method
        operation = getattr(service, name, None)
        if operation is None or name.startswith("_"):
            raise LookupError(f"no method {method}")
        return operation(*args)

    peer = Peer(connection, dispatch, endpoint={"local": "python", "expected": "main"})
    service.peer = peer
    peer.start()
    await peer.closed


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1]))
