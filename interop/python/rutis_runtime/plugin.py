"""What a Python plugin for rutis looks like.

A plugin is a module with an `apply(ctx, config)` function. It may declare:

- `inject`: the names of the services it uses. rutis starts the plugin only
  once all of them are available, and stops it when one goes away.
- `provides`: the services it provides to rutis, as
  `{ name: { method: "sync" | "async" } }`, or `{ name: SomeClass }` to
  take the methods from the class (public methods; `async def` ones are
  async).
- `Config`: the JSON Schema of its config, as a dict.

`apply` may be `async`. It returns a cleanup function (which may be async)
or None::

    inject = ["llm"]
    provides = {"weather": Weather}

    def apply(ctx, config):
        ctx.provide("weather", Weather(ctx.use("llm"), config["city"]))
        return lambda: None

A module may instead define `plugin = rutis_runtime.define_plugin(...)`.
"""

from __future__ import annotations

import inspect
from dataclasses import dataclass, field
from types import ModuleType
from typing import Any, Callable


class Context:
    """What `apply` receives."""

    def use(self, name: str) -> Any:
        """The service `name`: the object itself when a plugin in this
        process provides it, otherwise a proxy whose methods call it."""
        raise NotImplementedError

    def provide(self, name: str, value: Any) -> Callable[[], None]:
        """Provide `value` as the service `name` until the plugin unloads,
        or until the returned function is called."""
        raise NotImplementedError

    def effect(self, cleanup: Callable) -> None:
        """Run `cleanup` (sync or async) when the plugin unloads."""
        raise NotImplementedError


@dataclass
class Plugin:
    apply: Callable
    inject: list = field(default_factory=list)
    provides: dict = field(default_factory=dict)
    config: dict | None = None


def define_plugin(
    apply: Callable,
    *,
    inject: list | tuple = (),
    provides: dict | None = None,
    config: dict | None = None,
) -> Plugin:
    return Plugin(apply, list(inject), shapes(provides or {}), config)


def shapes(provides: dict) -> dict:
    """`{ name: { method: kind } }` from declared shapes or classes."""
    result = {}
    for name, declared in provides.items():
        if isinstance(declared, dict):
            for method, kind in declared.items():
                if kind not in ("sync", "async"):
                    raise ValueError(f"{name}.{method}: kind must be 'sync' or 'async'")
            result[name] = dict(declared)
        elif isinstance(declared, type):
            result[name] = {
                method: "async" if inspect.iscoroutinefunction(member) else "sync"
                for method, member in inspect.getmembers(declared, callable)
                if not method.startswith("_")
            }
        else:
            raise TypeError(f"provides[{name!r}] must be a dict of method kinds or a class")
    return result


def load(module: ModuleType) -> Plugin:
    declared = getattr(module, "plugin", None)
    if isinstance(declared, Plugin):
        return declared
    apply = getattr(module, "apply", None)
    if not callable(apply):
        raise TypeError(f"{module.__name__} has no apply(ctx, config)")
    inject = getattr(module, "inject", [])
    if isinstance(inject, str) or not all(isinstance(name, str) for name in inject):
        raise TypeError(f"{module.__name__}.inject must be a list of service names")
    return Plugin(
        apply,
        list(inject),
        shapes(getattr(module, "provides", {})),
        getattr(module, "Config", None),
    )
