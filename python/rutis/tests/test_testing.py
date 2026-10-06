"""rutis.testing: plugins tested without a host."""

import asyncio
import unittest
from dataclasses import dataclass

from rutis import PLUGIN_API, define_plugin
from rutis.testing import PluginTestError, load


@dataclass
class Reading:
    city: str
    degrees: int


class Weather:
    def __init__(self, llm, city):
        self.llm, self.city = llm, city

    async def today(self):
        return f"{await self.llm.ask(self.city)} in {self.city}"

    def reading(self):
        return Reading(self.city, 21)


class Llm:
    async def ask(self, question):
        return "sunny"


def weather_plugin():
    order = []

    def apply(ctx, config):
        ctx.provide("weather", Weather(ctx.use("llm"), config["city"]))
        ctx.effect(lambda: order.append("effect"))
        return lambda: order.append("returned")

    return define_plugin(apply, inject=["llm"], provides={"weather": Weather}), order


def run(coroutine):
    return asyncio.run(coroutine)


class Testing(unittest.TestCase):
    def test_a_plugin_runs_with_its_services_and_unloads_latest_first(self):
        plugin, order = weather_plugin()

        async def go():
            async with load(plugin, config={"city": "Oslo"}, services={"llm": Llm()}) as t:
                self.assertEqual(await t.service("weather").today(), "sunny in Oslo")
                # A dataclass crosses as data.
                self.assertEqual(t.service("weather").reading(), {"city": "Oslo", "degrees": 21})
                self.assertEqual(t.provided(), ["weather"])
            self.assertEqual(order, ["returned", "effect"])

        run(go())

    def test_undeclared_and_missing_services_are_errors(self):
        plugin, _ = weather_plugin()

        async def missing():
            async with load(plugin, config={"city": "x"}):
                pass

        with self.assertRaisesRegex(PluginTestError, "injects llm"):
            run(missing())

        sneaky = define_plugin(lambda ctx, config: ctx.use("llm"))

        async def undeclared():
            async with load(sneaky, services={"llm": Llm()}):
                pass

        with self.assertRaisesRegex(PluginTestError, "without declaring it in inject"):
            run(undeclared())

    def test_strict_mode_crosses_values_as_between_processes(self):
        class Store:
            def __init__(self):
                self.kept = []

            def put(self, item):
                self.kept.append(item)

            def first(self):
                return self.kept[0]

            def wrong(self):
                return asyncio.sleep(0)

        plugin = define_plugin(
            lambda ctx, config: ctx.provide("store", Store()),
            provides={"store": {"put": "sync", "first": "sync", "wrong": "sync"}},
        )

        async def go():
            async with load(plugin) as t:
                item = {"n": 1}
                t.service("store").put(item)
                item["n"] = 2
                self.assertEqual(t.service("store").first(), {"n": 1})
                with self.assertRaisesRegex(PluginTestError, "cannot cross"):
                    t.service("store").put({1, 2})
                with self.assertRaisesRegex(PluginTestError, "declared sync but returned an awaitable"):
                    t.service("store").wrong()

        run(go())

    def test_a_plugin_that_needs_a_newer_plugin_api_is_refused(self):
        plugin, _ = weather_plugin()
        plugin.api = PLUGIN_API + 1

        async def go():
            async with load(plugin, services={"llm": Llm()}):
                pass

        with self.assertRaisesRegex(PluginTestError, "needs plugin API"):
            run(go())


if __name__ == "__main__":
    unittest.main()
