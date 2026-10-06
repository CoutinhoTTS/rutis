"""{{name}}: a rutis plugin."""

from rutis import define_plugin


class Greeter:
    def __init__(self, greeting: str) -> None:
        self.greeting = greeting

    def hello(self, name: str) -> str:
        return f"{self.greeting}, {name}!"


def apply(ctx, config):
    ctx.provide("greeter", Greeter(config.get("greeting", "Hello")))
    # Return a cleanup, or use ctx.effect(cleanup), when the plugin holds
    # anything to release.


plugin = define_plugin(
    apply,
    # Services this plugin uses: inject=["llm"], then ctx.use("llm").
    inject=[],
    # Services it provides; the methods are read from the class (async def
    # ones are async).
    provides={"greeter": Greeter},
    # The JSON Schema of its config.
    config={"type": "object", "properties": {"greeting": {"type": "string", "default": "Hello"}}},
)
