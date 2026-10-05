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



class Weather:
    def today(self):
        return "monday"


class CapabilityTests(unittest.IsolatedAsyncioTestCase):
    """An object reference goes only to a far end that declared `objects`."""

    async def session(self, capabilities):
        ours, theirs = socket.socketpair()
        theirs.settimeout(5)
        self.addCleanup(ours.close)
        self.addCleanup(theirs.close)
        peer = Peer(ours, lambda target, method, args: Weather(), endpoint={"local": "py", "expected": "main"})
        self.addCleanup(peer.close)
        lines = theirs.makefile("rb")
        peer.start()
        self.assertEqual(json.loads(lines.readline())["op"], "hello")
        hello = {"op": "hello", "version": 3, "endpoint": "main", "capabilities": capabilities}
        peer._receive(hello)
        peer._receive({"op": "invoke", "id": "main:1", "path": [], "target": "svc", "method": "get", "args": {"type": "data", "value": []}})
        # The call runs on the loop: let it.
        await asyncio.sleep(0.05)
        return json.loads(lines.readline())

    async def test_an_object_is_refused_to_a_far_end_without_objects(self):
        reply = await self.session(["signals"])
        self.assertEqual(reply["op"], "throw", reply)
        self.assertIn("cannot receive object references", reply["error"]["message"])

    async def test_an_object_goes_to_a_far_end_with_objects(self):
        reply = await self.session(["objects"])
        self.assertEqual(reply["op"], "return", reply)
        self.assertEqual(reply["value"]["type"], "reference")
        self.assertEqual(reply["value"]["value"]["kind"], "object")


if __name__ == "__main__":
    unittest.main()
