"""Packaged plugins: `py:<name>` finds the module through the entry point
group `rutis.plugins`, with its package's version."""

import os
import sys
import tempfile
import unittest

from rutis.runner import Runtime, _entry_point


class EntryPoints(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        root = self.directory.name
        package = os.path.join(root, "weather_impl")
        os.makedirs(package)
        with open(os.path.join(package, "__init__.py"), "w") as module:
            module.write("inject = []\nprovides = {}\ndef apply(ctx, config):\n    pass\n")
        dist = os.path.join(root, "weather_plugin-1.2.3.dist-info")
        os.makedirs(dist)
        with open(os.path.join(dist, "METADATA"), "w") as metadata:
            metadata.write("Metadata-Version: 2.1\nName: weather-plugin\nVersion: 1.2.3\n")
        with open(os.path.join(dist, "entry_points.txt"), "w") as entries:
            entries.write("[rutis.plugins]\nweather = weather_impl\n")
        sys.path.insert(0, root)

    def tearDown(self):
        sys.path.remove(self.directory.name)
        sys.modules.pop("weather_impl", None)
        self.directory.cleanup()

    def test_an_entry_point_names_the_module_and_the_version(self):
        self.assertEqual(_entry_point("weather"), ("weather_impl", "1.2.3"))
        # Not an entry point: a module name, with no version.
        self.assertEqual(_entry_point("os.path"), ("os.path", None))

    def test_describe_reports_the_version(self):
        runner = Runtime.__new__(Runtime)
        runner.stamps = {}
        described = runner.describe("weather")
        self.assertEqual(described["version"], "1.2.3")
        self.assertEqual(described["inject"], [])


if __name__ == "__main__":
    unittest.main()
