"""Test a plugin without a host: give it the services it injects, call the
services it provides, unload it::

    from rutis.testing import load

    async def test_weather():
        async with load(weather_plugin, config={"city": "Oslo"}, services={"llm": FakeLlm()}) as t:
            assert await t.service("weather").today() == "sunny in Oslo"

It checks what a host would: the plugin uses only the services it declares
in `inject`, provides what it declares in `provides` with every declared
method, and runs its cleanups on unload. With `strict` (the default), values
cross between the plugin and the test as they would between processes: data
is copied (a dataclass arrives as a dict), functions and objects with
behaviour pass by reference, sync methods return values and async ones
awaitables; what works only in one process fails here too.
"""

from __future__ import annotations

import dataclasses
import inspect
from contextlib import asynccontextmanager
from types import ModuleType
from typing import Any, AsyncIterator, Callable

from . import plugin as sdk
from .peer import _is_live


class PluginTestError(AssertionError):
    """The plugin does something a host would refuse or that would break
    across processes."""


def _cross(value: Any, where: str) -> Any:
    """`value` as the other process sees it."""
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (list, tuple)):
        return [_cross(item, f"{where}[{i}]") for i, item in enumerate(value)]
    if isinstance(value, dict):
        if not all(isinstance(key, str) for key in value):
            raise PluginTestError(f"{where}: only string keys cross between processes")
        return {key: _cross(item, f"{where}.{key}") for key, item in value.items()}
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return _cross(dataclasses.asdict(value), where)
    if inspect.isawaitable(value):
        return _awaited(value, where)
    if callable(value) and not isinstance(value, type):
        return _function(value, where)
    if _is_live(value):
        return _Reference(value, where)
    raise PluginTestError(f"{where}: {type(value).__name__} cannot cross between processes")


async def _awaited(value: Any, where: str) -> Any:
    return _cross(await value, where)


def _function(function: Callable, where: str) -> Callable:
    def call(*args: Any) -> Any:
        return _cross(function(*(_cross(arg, f"{where} argument {i}") for i, arg in enumerate(args))), where)

    return call


class _Reference:
    """An object with behaviour, used from the other process."""

    def __init__(self, target: Any, where: str) -> None:
        object.__setattr__(self, "_target", target)
        object.__setattr__(self, "_where", where)

    def __getattr__(self, name: str) -> Any:
        return _cross(getattr(self._target, name), f"{self._where}.{name}")


def _shaped(name: str, service: Any, shape: dict, strict: bool) -> Any:
    """A service seen through its declared shape: only declared methods, sync
    ones returning values, async ones awaitables."""

    class Service:
        pass

    seen = Service()
    for method, kind in shape.items():
        def call(*args: Any, _method: str = method, _kind: str = kind) -> Any:
            where = f"{name}.{_method}"
            crossed = [_cross(arg, f"{where} argument {i}") for i, arg in enumerate(args)] if strict else list(args)
            result = getattr(service, _method)(*crossed)
            if _kind == "sync":
                if inspect.isawaitable(result):
                    if inspect.iscoroutine(result):
                        result.close()
                    raise PluginTestError(f"{where} is declared sync but returned an awaitable: declare it 'async'")
                return _cross(result, where) if strict else result

            async def settle() -> Any:
                value = await result if inspect.isawaitable(result) else result
                return _cross(value, where) if strict else value

            return settle()

        setattr(seen, method, call)
    return seen


class Loaded:
    def __init__(self, plugin: sdk.Plugin, strict: bool) -> None:
        self._plugin = plugin
        self._strict = strict
        self._provided: dict[str, list] = {}
        self._cleanups: list[Callable] = []
        self._unloaded = False

    def service(self, name: str) -> Any:
        """The service `name` the plugin provides, as rutis sees it: its
        declared methods only."""
        if self._unloaded:
            raise PluginTestError("the plugin is unloaded")
        shape = self._plugin.provides.get(name)
        if shape is None:
            raise PluginTestError(f"{name} is not declared in provides, so rutis cannot use it")
        if name not in self._provided:
            raise PluginTestError(f"the plugin does not provide {name} (now)")
        return _shaped(name, self._provided[name][0], shape, self._strict)

    def provided(self) -> list[str]:
        """The names of the services the plugin provides now."""
        return list(self._provided)

    async def unload(self) -> None:
        """Run the cleanups, latest first; the plugin's services are withdrawn."""
        if self._unloaded:
            return
        self._unloaded = True
        for cleanup in reversed(self._cleanups):
            result = cleanup()
            if inspect.isawaitable(result):
                await result
        self._provided.clear()


class _Context(sdk.Context):
    def __init__(self, loaded: Loaded, services: dict) -> None:
        self._loaded = loaded
        self._services = services

    def use(self, name: str) -> Any:
        plugin = self._loaded._plugin
        if name not in plugin.inject:
            raise PluginTestError(f"the plugin uses {name} without declaring it in inject")
        service = self._services[name]
        return _cross(service, f"service {name}") if self._loaded._strict else service

    def provide(self, name: str, value: Any) -> Callable[[], None]:
        shape = self._loaded._plugin.provides.get(name)
        for method in shape or {}:
            if not callable(getattr(value, method, None)):
                raise PluginTestError(f"{name} is declared with {method} in provides, but the service has no such method")
        entry = [value]
        self._loaded._provided[name] = entry

        def withdraw() -> None:
            if self._loaded._provided.get(name) is entry:
                del self._loaded._provided[name]

        return withdraw

    def effect(self, cleanup: Callable) -> None:
        if not callable(cleanup):
            raise PluginTestError("effect needs a cleanup function")
        self._loaded._cleanups.append(cleanup)


@asynccontextmanager
async def load(
    plugin: sdk.Plugin | ModuleType,
    *,
    config: Any = None,
    services: dict | None = None,
    strict: bool = True,
) -> AsyncIterator[Loaded]:
    """Load `plugin` (a module with `apply`, or what `define_plugin` returns)
    with `config` and the services in `services`; unload it on exit."""
    if isinstance(plugin, ModuleType):
        plugin = sdk.load(plugin)
    if not isinstance(plugin, sdk.Plugin):
        raise PluginTestError("load needs a plugin module or what define_plugin returns")
    if plugin.api > sdk.PLUGIN_API:
        raise PluginTestError(f"the plugin needs plugin API {plugin.api}; this SDK supports {sdk.PLUGIN_API}")
    services = services or {}
    for name in plugin.inject:
        if name not in services:
            raise PluginTestError(f"the plugin injects {name}: give the test a service {name}")
    loaded = Loaded(plugin, strict)
    ctx = _Context(loaded, services)
    config = config if config is not None else {}
    returned = plugin.apply(ctx, _cross(config, "config") if strict else config)
    if inspect.isawaitable(returned):
        returned = await returned
    if returned is not None:
        if not callable(returned):
            raise PluginTestError("apply must return a cleanup function or None")
        loaded._cleanups.append(returned)
    try:
        yield loaded
    finally:
        await loaded.unload()


__all__ = ["Loaded", "PluginTestError", "load"]
