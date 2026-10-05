"""The runtime conformance plugin (rutis_interop::conformance::runtime),
for the Python runtime."""
import os

inject = ["clock"]


class Weather:
    def __init__(self, clock):
        self.clock = clock

    def today(self):
        return f"Oslo at {self.clock.now()}"

    async def later(self):
        return "Oslo later"

    def each(self, callback):
        return [callback(day) for day in ("mon", "tue")]

    def crash(self):
        os._exit(17)


provides = {"weather": Weather}


def apply(ctx, config):
    ctx.provide("weather", Weather(ctx.use("clock")))
