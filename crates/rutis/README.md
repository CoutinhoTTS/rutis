# rutis

Cordis 核心范式的 Rust 惯用实现(自 [min-cordis](https://github.com/eric8810/min-cordis) 独立成库)。

## 五支柱

1. **插件 = 装配单元**:一次 `apply`,提供服务 / 监听 / 清理
2. **fiber = 生命周期容器**:六态状态机 + 依赖门控 + 子树永久关闭 + 恰好一次清理
3. **服务 = 类型键注册表 + 实例子树可见性 + isolate 作用域**
4. **事件总线 = 四分发语义**(emit / parallel / serial / waterfall),实例事件独立派发与保序
5. **依赖驱动重载**:provider 卸载 → 消费者驱逐并自动重载

## 使用

```toml
[dependencies]
rutis = "0.4.0"
```

内核零 serde、零 unsafe,依赖仅 tokio / tokio-util / thiserror。设计与对拍文档见[仓库 docs](https://github.com/arcships/rutis/tree/main/docs)。

首次使用建议先读[应用设计指南](https://github.com/arcships/rutis/blob/main/docs/development-guide.md)，再按[开发手册](https://github.com/arcships/rutis/blob/main/docs/development-handbook.md)实现。配套示例可在仓库中运行：`cargo run -p rutis --example development_workflow`。

## 0.4 事件接口

通过 `EventKey<E>` 统一默认、命名与实例通道；`EventPattern<E>` 支持带来源键的前缀订阅。`SyncEvent` 增加 `bail_sync` / `waterfall_sync`，同步终点可以借用当前调用栈。

0.4.0 为待发布版本。见 [迁移说明](../../docs/migration-0.3-to-0.4.md)。

## License

MIT(继承自 [Cordis](https://github.com/shigma/cordis) © Shigma)。
