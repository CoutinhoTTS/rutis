# 原生跨进程挂载：交付与验收

依据：[需求](requirements-protocol-plugins.md)、[组合设计](design-protocol-plugin-mount.md)。完整目标不按当前代码删减。阶段交付必须注明未覆盖项，方法切片通过不等于任意插件兼容。

## 1. 阶段与交付条件

| 阶段 | 交付物 | 必须通过的检查 | 依赖 / 当前状态 |
| --- | --- | --- | --- |
| P0 修正当前 PR | 两端先清理后排空；导出捕获对象；维持已有方法形状 | disposer 解除在途等待；换槽位不改旧对象；失活导出 Context 不妨碍独立对象 | 本轮实现并验证；仍是固定导出的方法切片 |
| P1 接入可行性与成本 | rustdoc 类型提取原型、原生服务 / 事件 / update 接入原型；mmap 与文件读取两种版本读取对照 | 见下表；核实目标 Cordis 实现与现有扩展入口，输出接口缺口、构建依赖和性能 / 一致性证据后选择一种后端 | 尚未实现；先在本仓库核实方案，不以对外提案为前置 |
| P2 一套调用与对象协议 | 版本握手、invoke / await、回调、属性、拥有 / 借用 / 弱引用、迭代与释放计数；本地代理再导出 | 两方向引用与保存回调、受理先于 release、跨会话转交；可证等待环返回 SyncWaitCycle；FnOnce / FnMut、借用、流取消 | 使用 P1 类型模型；不得先把事件做成另一套 RPC |
| P3 原生服务与生命周期 | 实际注册观察、版本缓存、动态 / 可选依赖、isolate、撤销、更新、子插件、显式 close | 新读 B / 旧 A；撤销后禁止新取得，但允许合法旧对象的收尾操作；跨方向依赖、同时卸载；无 GC 关闭 | 依赖 P1 已验证的服务 / 更新接入方式与 P2；不能沿用 P0 的 closing 全量拒绝 |
| P4 原生事件组合 | 逐监听路由、原生分发算法、once / 过滤 / next | emit 同步前缀、bail Promise 身份、短路、监听顺序、重入和分发中注销；运行 §2 两方向组合 | 依赖 P1 已验证的事件接入方式及 P2 / P3；接入缺口不能用双广播冒充解决 |
| P5 包级自动接入 | 清单配置、构建集成、产物缓存、应用清理接入和开发者用法文档 | 干净 stable 应用接入受测插件产物；源码接入的 nightly 依赖明确；无手工生成；完整双向场景及性能对照 | 类型提取依赖独立记录；阶段完成后才评估整体兼容范围 |

P1 不要求先做完 P2—P5。先针对真正影响路线的接口做小型验证，避免先完成大批协议代码再发现无法接入原框架。P1 的检查如下：

| 方面 | 检查与决策证据 |
| --- | --- |
| Rust 类型 | 多模块、宏生成成员、re-export、trait、不透明返回、features / target cfg、文档专用 cfg；验证公开可调用闭包，不从 provide 控制流推导服务表 |
| 构建成本 | 固定提取工具链、JSON 格式检查；记录首次 / 缓存命中的下载量与构建时间；stable 应用与插件工具链分别列明 |
| 服务提交 | 按设计 §4.1 检查 store / impl.value、fiber.state、注入 store 快照、isolate 继承及读取规则。逐一对照属性、严格 / 非严格 get；测试 ACTIVE → UNLOADING、清理中旧快照和无 store 写入的状态变化；提交窗口不执行 callback / notify |
| 热路径 | 同机 debug / release 与原生对照；mmap 原子读取和 fs.readSync 定位 8 字节分别记录全部读取 / 检查开销。文件方案验证短读、并发写及进位，不把两次相等读数当成原子性证明；P1 最终只选择一种生产后端 |
| 调度 | 记录可获得的执行器 / 调用链信息，区分可证环与不可见业务等待；P2 要求明确失败的用例见下文，不以测出卡死为验收成功 |
| Cordis 原型 | 区分目标 Cordis 与当前测试依赖，核实服务 / 事件 / fiber 操作能否通过现有入口接入；提供原生回归证据，不因包的 repository 字段就推导出对外开发任务 |

### P2 必须增加的协议用例

- [ ] 两端分别在等待前检测已知执行器冲突；运行期 await 构成可证同步环时返回 SyncWaitCycle，而不是超时才算成功。
- [ ] current_thread 上无反向异步依赖的同步调用、同步立即回调、单纯返回 Promise / Future 正常完成，避免误拒绝。
- [ ] 多 worker 的可证线程亲和等待环报错；未知业务锁不假称已检测。跨会话转发保留链标记。
- [ ] 把业务分发暂停在引用受理之后，此时处理紧随的 release；任务启动时对象仍有效。取消 / 解码失败不泄漏持有，不能由 I/O Worker 越序释放。
- [ ] 同一应用挂载 A / B，将 A 对象传入 B、再次转交及往返，复用存活代理；释放 B 后按本地最后持有者归还 A 的计数。
- [ ] 分别断开 A / B：受影响对象明确失败，无关会话继续；基准单独记录两跳转发成本。

## 2. 双向组合验收清单

每个勾选项都要求自动化跨进程测试及对应的原生对照。以下均未完成；P0 的孤立回归不能勾选整条组合。共享同一协议测试工具，但两个方向分别记录结果。

### rutis 应用挂载 Cordis 插件

- [ ] Rust Audit 先投影到 Node；原 Cordis Search 依赖就绪后启动，Rust 消费者通过原生 Context 取得 Search。
- [ ] 构建从 Search / Audit / Cursor / page 的公开类型自动生成接口；业务源码无协议声明。
- [ ] scan 返回异步结果，再取得 Cursor K；K 身份往返不变，scan 保存的 Rust 回调可在其返回后调用。
- [ ] K 的同步属性 / 方法及立即回调返回普通值；异步回调交还正确执行器。
- [ ] 本地与远端监听 A / B / C 按原顺序运行，B 可异步调用 Audit；覆盖 emit、parallel、serial、bail、waterfall。
- [ ] 单独对照 bail 返回 Promise、Some(false)、next 次数、once 重入、过滤和分发中注销；不兼容契约诊断明确。
- [ ] Search A 换成 B 后，新读取取得 B，保存的 A / K 保持原对象；缓存命中零 IPC，动态 getter 仍执行副作用。
- [ ] 可选依赖不变必需；两个 isolate 不串用；相同短名的不同服务在发布前报冲突，应用映射后各自正确。
- [ ] update / restart 进入原 Cordis fiber，校验及 internal/update 只执行一次；Pending 更新、拒绝更新、子插件独立撤销按原规则。
- [ ] 撤销 Audit 沿真实依赖开始清理；K.close 可反向调用并解除 scan 的在途等待；双方同时卸载无新增等待环。
- [ ] release 与新授予交叉、弱缓存代理更替、往返引用、异步借用及流取消均不悬空、不多持有。
- [ ] 不触发 GC，显式 close 仍回收自有 Node 进程；普通 fiber 重启复用会话，显式进程替换不残留旧进程。
- [ ] 在各阶段断连：在途调用失败、结果未知不重放、旧代消息不操作新实例、无关插件继续运行。

### Cordis 应用挂载 rutis 插件

- [ ] JS Audit 先投影到 Rust；原 rutis Search 依赖就绪后启动，Cordis 消费者通过 ctx 属性取得 Search。
- [ ] 构建从原 Rust 公开接口生成 Search / Audit / Cursor / page 绑定及 Context 声明；业务源码无协议注解。
- [ ] scan 保持 Future / Promise 形状，返回的 K 与异步结果是不同引用；保存的 JS 回调不随 scan 返回过期。
- [ ] K 的同步属性 / 方法及立即回调返回普通值；Rust 回调的执行器和借用不因通信迁移。
- [ ] 逐个登记 A / B / C，B 可异步调用 Audit；使用同一协议覆盖全部事件入口和调用期间的反向操作。
- [ ] 对照 Promise 短路、Some(false)、next 有效期 / 次数、once 重入、过滤、注销；不把 Rust 事件规则静默改成 JS 规则。
- [ ] Rust 槽位 A → B 后，新的 ctx 读取取得 B，旧代理及 K 保持 A；缓存版本与实际提交一致。
- [ ] 可选依赖、qualifier、isolate 和多 crate 同名类型各按原生规则处理；冲突映射不改变类型身份。
- [ ] 更新进入原 rutis factory fiber；静态插件保留不可更新结果；子插件按原父子关系激活和清理。
- [ ] 撤销 JS Audit 触发 Search 及消费者清理；Rust disposer 先启动并解除在途调用，保留清理所需通信。
- [ ] 覆盖授予 / release 交叉、对象往返、FnOnce / FnMut、借用 Future、弱引用和流提前关闭。
- [ ] 不触发 GC，显式 close 回收自有 Rust 进程；原插件卸载与独立对象存活分别验收。
- [ ] 任意阶段断连和重新激活不复用旧引用 / 代次；错误与执行结果未知按约定报告。

## 3. 性能记录与完成标准

每个方向记录硬件、Node / Rust 版本、优化级别、样本数，以及同步往返和事件的 p50 / p95 / p99、吞吐、调用线程停顿。缓存基准必须包含变化中的提供者和动态读取，不能只测空缓存命中函数。

当前同步方法样本可复现：

```sh
cargo build -p native-mount-example --bin rutis-counter
node interop/node/bench/sync-call.mjs target/debug/rutis-counter
cargo build --release -p native-mount-example --bin rutis-counter
node interop/node/bench/sync-call.mjs target/release/rutis-counter
```

微基准仅用于选择机制；P5 必须用代表性插件负载对照原生运行，并记录可接受负载与延迟预算。未达到预算的同步读取 / 事件路径保留为已知差异，不能以“通信天然较慢”为由忽略。

每个阶段的 PR 同时更新实现状态、验收结果和未满足要求。§9 的类型、执行器及框架接入限制必须在最终报告中单列，不把拒绝这些组合计作完整兼容。
