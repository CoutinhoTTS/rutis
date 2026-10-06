import asyncio
import unittest

from rutis.testing import load

from {{module}} import plugin


class Plugin(unittest.TestCase):
    def test_greets_with_the_configured_greeting(self):
        async def go():
            async with load(plugin, config={"greeting": "Hi"}) as t:
                self.assertEqual(t.service("greeter").hello("Ada"), "Hi, Ada!")

        asyncio.run(go())


if __name__ == "__main__":
    unittest.main()
