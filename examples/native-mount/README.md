# 原生跨进程挂载：首个开发切片

这个示例验证 **rutis 挂载普通 Cordis 插件**。TypeScript 编译器读取原插件的服务注册和类型，Cargo 构建自动生成 Rust 类型及挂载插件；运行时仍由真实 Cordis 执行原插件。

在仓库根目录运行：

```sh
npm --prefix interop/node ci
cargo run -p native-mount-example
```

没有独立的生成步骤。应用通过 [build.rs](build.rs) 选择原插件，生成文件放在 Cargo 的构建目录，不提交、不手工维护。当前接入仍使用本地源码路径和 Cargo 构建钩子，尚未实现最终的按插件包配置接入体验。

原插件见 [counter.ts](../../interop/node/test/fixtures/counter.ts)，只使用 Cordis 的 Context 和 `provide`。Rust 通过生成的 `Plugin` 挂载，并用 `Ctx::get` / `require` 读取生成的 `Counter`；普通消费者的依赖等待和清理由 rutis 管理。

| 已验证 | 行为 |
| --- | --- |
| 自动生成 | 从原插件方法签名生成绑定，跟踪导入类型文件的变化 |
| 同步和异步方法 | 同步返回 Rust 值；Promise 方法生成 async 方法 |
| 状态和错误 | 多次调用使用同一个原对象；TS 异常映射为 Rust `Result`，非有限数不静默变为 null |
| 原生依赖和卸载 | 依赖就绪后装载；消费者清理能调用原服务，随后关闭远端进程 |
| 作用域 | 两次隔离挂载的服务和状态互不串用 |
| 进程退出 | 在途及后续调用失败，不重试、不返回默认值 |

TS 方法可能抛错，因此新生成的 Rust 方法返回 `Result<T, rutis_interop::Error>`；这不会把同步方法改成 Future，也不修改已有 Rust 插件的签名。

当前运行环境为 Unix、Node 和真实 Cordis 4.0.1；生成器固定使用 TypeScript 6.0.3 的编译器 API。首个切片支持普通 `apply` 中可发现的服务注册，以及 number/string/boolean/void 和数组组成的方法签名。不支持的接口会报告原源码位置。

**完整需求仍未完成**：反向挂载、运行期依赖变化传播、事件、同步属性、对象引用、回调及嵌套调用还需实现和验证。它们没有被移出产品目标；此示例不能代表“任意插件已经兼容”。

验证命令：

```sh
npm --prefix interop/node test
cargo test -p rutis-interop -p native-mount-example
cargo clippy -p rutis-interop -p native-mount-example --all-targets -- -D warnings
```
