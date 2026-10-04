# rutis_runtime：Python 插件运行时

rutis 用它在一个 Python 进程里运行 Python 插件。一个运行时实例就是一个进程，装多少个插件都只有这一个进程。依赖、启动停止顺序、重启和配置都由 rutis（rutis-loader）负责，这个包只负责按指令装载、卸载插件，以及和 rutis 之间的通信。

需要 Python 3.12 或更高，没有第三方依赖。

## 写插件

一个插件就是一个模块，里面有 `apply(ctx, config)`：

```python
# weather_plugin.py
inject = ["llm"]                       # 用到的服务：都就绪才启动，任何一个撤销就停下
Config = {"type": "object", "properties": {"city": {"type": "string"}}}   # 配置的 JSON Schema


class Weather:
    def __init__(self, llm, city):
        self.llm, self.city = llm, city

    def today(self):                   # 同步方法
        return self.llm.ask(f"weather in {self.city}")

    async def later(self):             # 异步方法
        ...


provides = {"weather": Weather}        # 提供给 rutis 的服务；方法形状从类里读


def apply(ctx, config):
    ctx.provide("weather", Weather(ctx.use("llm"), config["city"]))
    return lambda: None                # 清理，可以是 async，也可以不返回
```

- `apply` 可以是 `async def`。
- `ctx.use(name)`：同一个进程里另一个插件提供的服务，拿到的就是对象本身；其他服务（Rust、JavaScript 或另一个运行时提供的）拿到代理，按方法形状同步调用或返回协程。
- `ctx.provide(name, value)`：提供服务，直到插件卸载，或调用返回的函数。
- `ctx.effect(cleanup)`：卸载时运行 `cleanup`。
- `provides` 也可以直接写形状：`{"weather": {"today": "sync", "later": "async"}}`。
- 也可以用 `plugin = rutis_runtime.define_plugin(apply, inject=[...], provides={...}, config={...})`。

在 rutis-loader 的配置里，这个插件的行名是 `py:weather_plugin`。

## 规则

- **可重入**：插件在调用 rutis 服务的同步方法时，它自己提供的服务可能在这期间被调用（见 rutis-interop README 的"同步调用与可重入"）。调用 rutis 服务时不要持有锁。
- **线程**：插件代码在运行时的 asyncio 线程上执行。从其他线程调用代理也可以，调用会转到 asyncio 线程上进行，只阻塞调用的那个线程。
- **值**：`None`、布尔、数字、字符串、列表、字符串键的字典和 dataclass 按数据传递；函数、协程和带行为的对象按引用传递。对方传来的 `undefined` 参数等同于没传，用参数默认值。
- **配置变化**：Python 插件没有就地更新的字段，配置变了就重启插件。
- **退出**：运行时结束时用 `os._exit` 退出，插件留下的线程不会让进程挂住。

## 测试

```bash
python3 -m unittest discover -s tests
```

协议和行为的端到端测试在 Rust 一侧：`crates/rutis-interop/tests/python_runtime.rs`、`crates/rutis-loader/tests/multilang.rs`。
