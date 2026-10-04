"""The session layer against a scripted peer on a socketpair."""

import asyncio
import json
import socket
import unittest

from rutis_runtime.peer import Peer


class PeerTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.ours, self.theirs = socket.socketpair()
        self.theirs.settimeout(5)
        self.peer = Peer(self.ours, lambda target, method, args: None)
        self.peer._handshake = True
        self.lines = self.theirs.makefile("rb")

    async def asyncTearDown(self):
        self.peer.close()
        self.ours.close()
        self.theirs.close()

    def read(self):
        return json.loads(self.lines.readline())

    async def test_a_finished_future_is_ready_during_a_synchronous_call(self):
        loop = asyncio.get_running_loop()
        future = loop.create_future()
        future.set_result(42)
        # Exported, but its done-callback is still only scheduled.
        reference = self.peer._encode(future, [], False)["value"]["id"]
        # Inside a synchronous call `node:1`, Rust awaits it on that chain.
        self.peer._waiting.append("node:1")
        self.peer._receive({"op": "await", "id": "rust:1", "path": ["node:1"], "reference": reference})
        self.peer._waiting.pop()
        reply = self.read()
        self.assertEqual(reply["op"], "return", reply)
        self.assertEqual(reply["value"], {"type": "data", "value": 42})

    async def test_a_pending_future_on_the_chain_is_a_cycle(self):
        future = asyncio.get_running_loop().create_future()
        reference = self.peer._encode(future, [], False)["value"]["id"]
        self.peer._waiting.append("node:1")
        self.peer._receive({"op": "await", "id": "rust:1", "path": ["node:1"], "reference": reference})
        self.peer._waiting.pop()
        reply = self.read()
        self.assertEqual(reply["op"], "throw")
        self.assertEqual(reply["error"]["name"], "SyncWaitCycle")
        future.cancel()


if __name__ == "__main__":
    unittest.main()
