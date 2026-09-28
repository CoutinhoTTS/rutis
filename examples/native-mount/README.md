# 原生跨进程挂载：双向方法切片

这个示例验证 **rutis 挂载 Cordis 插件，以及 Cordis 挂载 rutis 插件**。构建时从原接口生成绑定，原插件仍在各自真实框架中执行。当前覆盖值类型方法，不代表任意插件已兼容。

在仓库根目录运行：

```sh
npm --prefix interop/node ci
cargo run -p native-mount-example
# 反方向：自动构建原 Rust 插件和绑定，再启动真实 Cordis 消费者测试。
cargo test -p native-mount-example --test cordis_mount -- --nocapture
```

没有独立的生成步骤。应用通过 [build.rs](build.rs) 选择原插件，生成文件放在 Cargo 的构建目录，不提交、不手工维护。当前接入仍使用本地源码路径和 Cargo 构建钩子，尚未实现最终的按插件包配置接入体验。

| 方向 | 原插件 | 生成内容 | 消费入口 |
| --- | --- | --- | --- |
| Cordis → rutis | [counter.ts](../../interop/node/test/fixtures/counter.ts) | Rust 服务类型、Config、原生 Plugin | `ctx.require::<bindings::Counter>()?` |
| rutis → Cordis | [src/lib.rs](src/lib.rs) | Rust 导出插件、TS 类和 Context 类型扩展、原生挂载插件 | `ctx.counter.add(1)` / `await ctx.counter.delayedAdd(1)` |

原插件没有协议导入或注解。反向测试自动读取构建目录中的模块，通过 `ctx.plugin(plugin(executable), { initial: 1 })` 挂载；源码路径、crate 名和执行位置由应用装配指定，业务消费者只使用原生 Context。

| 已验证 | 行为 |
| --- | --- |
| 自动生成 | TS 编译器读取原类型并跟踪导入文件；Rust 源码解析生成导出调用，由 Cargo 对照原 crate 编译检查 |
| 同步和异步方法 | 两个方向都保留同步返回值和 Promise / Future；TS 声明能直接扩展真实 Cordis Context |
| 状态和错误 | 调用原对象；TS 异常映射为 Rust `Result`，Rust `Result::Err` 映射为 JS 异常；非有限数显式报错 |
| 原生依赖和卸载 | 依赖就绪后装载；消费者清理能调用原服务，随后关闭远端进程 |
| 作用域 | 两次隔离挂载的服务和状态互不串用 |
| 进程退出 | 在途及后续调用失败，不重试、不返回默认值 |

TS 方法可能抛错，因此新生成的 Rust 方法返回 `Result<T, rutis_interop::Error>`；这不会把同步方法改成 Future，也不修改已有 Rust 插件的签名。

当前运行环境为 Unix、Node 和真实 Cordis 4.0.1；TS 生成器使用 TypeScript 6.0.3 编译器 API。支持普通 `apply` 中可发现的服务注册，以及 number/string/boolean/void 和数组组成的方法签名。

Rust 生成器当前读取单个入口源码中的公开具体插件、配置字段、服务和 `&self` 方法，支持 `f64` / `String` / `bool` / `()` / `Vec` 及结果的 `Result` 包装。泛型、借用接口、公开字段和宏展开接口会报错。入口含模块声明或顶层宏时拒绝生成，避免漏掉其中追加的方法；跨模块、trait 和完整构建条件下的类型发现尚未实现，不能把源码解析当成完整 Rust 类型分析。

Node 通信线程只收发消息，同步方法在调用线程等待结果；异步调用交还事件循环。此切片尚未实现等待期间的反向回调。Cordis 使用一个原生 generator effect 串行撤销服务、等待消费者清理、最后关闭 Rust 进程；Rust 导出插件按原生规则声明服务依赖。

**完整需求仍未完成**：运行期依赖变化、事件、同步属性、对象与回调、嵌套调用和包级自动接入尚未实现。[组合设计](../../docs/design-protocol-plugin-mount.md) 已统一定义服务读取、固定对象引用、调用与异步等待、事件分发及依赖清理，并提出两组原生框架扩展接口。扩展接口和设计中的约束取舍尚未获准实施；本示例只证明上述方法切片，不代表完整方案，也不缩小产品目标。

验证命令：

```sh
npm --prefix interop/node test
cargo test -p rutis-interop -p native-mount-example
cargo clippy -p rutis-interop -p native-mount-example --all-targets -- -D warnings
```
