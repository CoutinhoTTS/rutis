"""Unit tests of the plugin SDK and of value encoding; the protocol itself is
tested end to end from Rust (crates/rutis-bridge/tests/python_runtime.rs,
crates/rutis-loader/tests/multilang.rs)."""

import types
import unittest

from rutis import define_plugin
from rutis.peer import UNDEFINED, _apply, _data, _holds_reference, _is_live
from rutis.plugin import load, shapes


class Weather:
    def today(self):
        return "sunny"

    async def later(self):
        return "later"

    def _private(self):
        pass


class PluginTests(unittest.TestCase):
    def test_shapes_from_classes_and_dicts(self):
        self.assertEqual(
            shapes({"weather": Weather, "clock": {"now": "sync"}}),
            {"weather": {"today": "sync", "later": "async"}, "clock": {"now": "sync"}},
        )
        with self.assertRaises(ValueError):
            shapes({"clock": {"now": "maybe"}})

    def test_load_reads_module_attributes(self):
        module = types.ModuleType("weather_plugin")
        module.apply = lambda ctx, config: None
        module.inject = ["llm"]
        module.provides = {"weather": Weather}
        module.Config = {"type": "object"}
        plugin = load(module)
        self.assertEqual(plugin.inject, ["llm"])
        self.assertEqual(plugin.provides["weather"]["later"], "async")
        self.assertEqual(plugin.config, {"type": "object"})

    def test_define_plugin_wins(self):
        module = types.ModuleType("defined")
        module.plugin = define_plugin(lambda ctx, config: None, inject=["a"], provides={"b": {"m": "sync"}})
        self.assertEqual(load(module).inject, ["a"])

    def test_a_module_without_apply_is_refused(self):
        with self.assertRaises(TypeError):
            load(types.ModuleType("empty"))


class ValueTests(unittest.TestCase):
    def test_live_objects_cross_by_reference(self):
        self.assertTrue(_is_live(Weather()))
        self.assertFalse(_is_live({"a": 1}))
        self.assertTrue(_holds_reference({"cb": print}))
        self.assertFalse(_holds_reference([1, {"a": "b"}]))

    def test_data(self):
        self.assertEqual(_data((1, {"a": UNDEFINED, "b": None})), [1, {"b": None}])
        with self.assertRaises(TypeError):
            _data({1: "a"})

    def test_undefined_arguments(self):
        self.assertEqual(_apply(lambda a, b=2: (a, b), [1, UNDEFINED]), (1, 2))
        self.assertEqual(_apply(lambda a, b=2: (a, b), [UNDEFINED, 3]), (None, 3))


if __name__ == "__main__":
    unittest.main()
