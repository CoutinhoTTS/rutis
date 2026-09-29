# 跨进程挂载示例

主方向：**rutis 应用挂载原有的 Cordis 插件**。构建时从原 TS 插件生成 Rust 类型，原插件在真实 Cordis 中运行，rutis 侧按普通服务使用。设计见[兼容层设计](../../docs/design-protocol-plugin-mount.md)。

在仓库根目录运行：

```sh
npm --prefix interop/node ci
cargo run -p native-mount-example
# 反方向（次要）：Cordis 应用挂载 src/lib.rs 中的 rutis 插件。
cargo test -p native-mount-example --test cordis_mount -- --nocapture
```

没有独立的生成步骤：[build.rs](build.rs) 在普通 Cargo 构建时生成绑定，生成文件放在构建目录，不提交、不手工维护。

| 方向 | 原插件 | 消费方式 |
| --- | --- | --- |
| rutis 挂载 Cordis（主） | [counter.ts](../../interop/node/test/fixtures/counter.ts) | `ctx.plugin(bindings::Plugin::new(config))`，之后 `ctx.require::<bindings::Counter>()?` |
| Cordis 挂载 rutis（次） | [src/lib.rs](src/lib.rs) | `ctx.plugin(plugin(executable), config)`，之后 `ctx.counter.add(1)` |

原插件没有协议导入或注解。

| 已验证 | 行为 |
| --- | --- |
| 方法形状 | 同步方法同步返回，异步方法返回 Future / Promise；TS 异常映射为 `Result`，非有限数显式报错 |
| 依赖与清理 | 依赖方等服务就绪后启动；卸载时先清理消费者（仍可调用远端服务），最后关闭远端进程 |
| 换值跟随 | Cordis 换值后 rutis 新读取得到新代理，已取得的代理仍指向原对象；撤销后依赖方停止，重新提供后恢复 |
| 在途调用 | 卸载先启动 disposer 再等待在途调用 |
| 隔离与故障 | 两次隔离挂载互不串用；远端进程退出时在途和后续调用都失败 |

当前支持 `number` / `string` / `boolean` / `void` 及数组组成的方法签名，其他类型在构建时报错。事件转发、回调参数和对象返回值见[路线图](../../docs/roadmap-native-plugin-mount.md)。
