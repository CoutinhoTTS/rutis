# 原生跨进程挂载：交付与验收

依据：[需求](requirements-protocol-plugins.md)、[组合设计](design-protocol-plugin-mount.md)。完整目标不按当前代码删减。阶段交付必须注明未覆盖项，方法切片通过不等于任意插件兼容。

## 1. 阶段与交付条件

| 阶段 | 交付物 | 必须通过的检查 | 依赖 / 当前状态 |
| --- | --- | --- | --- |
| P0 修正当前 PR | 两端先清理后排空；导出捕获对象；维持已有方法形状 | disposer 解除在途等待；换槽位不改旧对象；失活导出 Context 不妨碍独立对象 | 本轮实现并验证；仍是固定导出的方法切片 |
| P1 接入可行性与成本 | rustdoc 类型提取原型、公开入口覆盖审计、rutis 最小事件接管接口原型；提交覆盖成立的路径才比较版本读取后端 | 按设计 §4.1 / §8 输出覆盖证据；事件按 Cordis 统一队列验证，rutis 核心扩展独立评审；成本独立记录 | Cordis 的 7 项入口检查与 3 项事件基准通过；跨进程对照、类型提取、rutis 原型与性能对照未完成。不修改 Cordis |
| P2 一套调用与对象协议 | 版本握手、invoke / await、回调、属性、拥有 / 借用 / 弱引用、迭代与释放计数；可迁移 Rust 异步回调的后台执行器；本地代理再导出 | 受理先于 release、跨会话转交；安全后台推进与 Send 但依赖原运行时的反例；静态风险诊断、可证环返回 SyncWaitCycle；FnOnce / FnMut、借用 / 流取消 | 使用 P1 类型模型；不为每次调用新建线程，不把 Send 当成可迁移证明；不得先把事件做成另一套 RPC |
| P3 原生服务与生命周期 | 实际注册观察、动态 / 可选依赖、isolate、撤销、更新、子插件、显式 close；提交覆盖成立的路径才启用缓存 | 新读 B / 旧 A；撤销后禁止新取得，但允许合法旧对象的收尾操作；跨方向依赖、同时卸载；无 GC 关闭 | 依赖 P1 已验证的服务 / 更新接入方式与 P2；Cordis 通用版本缓存不可交付，记录未满足项；不能沿用 P0 的 closing 全量拒绝 |
| P4 原生事件组合 | Cordis 统一监听队列、逐个 Rust 监听代理、原生分发算法、once / 过滤 / next | emit / parallel 的返回与等待、同步抛错 / 异步拒绝、AggregateError 结构；bail Promise、短路、顺序、重入、注销；运行 §2 两方向组合 | 依赖 P1 的 rutis 核心接管入口及 P2 / P3；入口未交付时完整事件仍未满足。不改 Cordis，不用双广播冒充解决 |
| P5 包级自动接入 | 清单配置、构建集成、产物缓存、应用清理接入和开发者用法文档 | 干净 stable 应用接入受测插件产物；源码接入的 nightly 依赖明确；无手工生成；完整双向场景及性能对照 | 类型提取依赖独立记录；阶段完成后才评估整体兼容范围 |

P1 不要求先做完 P2—P5。先核实影响路线的公开入口；缺少提交边界的路径不继续开发共享版本后端。事件所需的 rutis 核心接管入口单独设计与验证，不能假定已经存在。没有 Cordis 上游提案、fork 或 monkey patch 任务。P1 的检查如下：

| 方面 | 检查与决策证据 |
| --- | --- |
| Rust 类型 | 多模块、宏生成成员、re-export、trait、不透明返回、features / target cfg、文档专用 cfg；验证公开可调用闭包，不从 provide 控制流推导服务表 |
| 构建成本 | 固定提取工具链、JSON 格式检查；记录首次 / 缓存命中的下载量与构建时间；stable 应用与插件工具链分别列明 |
| 服务提交 | 按设计 §4.1 逐项核对原生写入与公开钩子。已验证 Cordis 属性 get / set 与直接 get / set 的不同路径，next 内包含用户钩子，service / status 在提交后通知；快照及 isolate 边界依据源码控制流。rutis 的同等审计待完成 |
| 热路径 | 仅对提交覆盖成立的路径比较 mmap 原子读取和 fs.readSync 定位 8 字节，记录同机 debug / release 与原生对照。文件方案验证短读、并发写及进位，不把两次相等读数当成原子性证明；最终只选择一种生产后端。Cordis 未覆盖路径测逐次解析代价，不能以低开销读版本弥补失效缺口 |
| 调度 | 记录执行器 / 调用链信息、Send 与持有期；验证后台独立推进条件及依赖原 timer / Handle 的反例，不从类型推断任意运行时依赖。P2 的静态警告和运行期报错均不宣称穷举业务死锁 |
| Cordis 接入 | 输出覆盖审计结论：internal/get / accessor、internal/set、internal/listener、internal/update 各自的适用路径；internal/dispatch 不可替代分发、service / status 缺少提交前边界。证据见设计 §8 及下列原生验证；未验证的跨进程组合留给对应阶段，不安排修改 Cordis |
| 事件对照 | Cordis 统一队列下代理保留返回值 / 抛错，不从 internal/dispatch 猜 emit / parallel。按下述原生基准做跨进程对照，包括 AggregateError.errors 顺序、嵌套、cause，以及不得重复包装。rutis 原型独立覆盖各分发模式、动态键 / pattern、once、作用域和未接管路径；保留其原队列及短路规则 |

覆盖验证（也纳入 `npm --prefix interop/node test`）：

```sh
node --test interop/node/test/cordis-hooks.test.mjs
```

7 项入口检查：属性与直接读取、属性与直接换值、监听替代及 once / effect 注销、ACTIVE update、真实 fiber 的 Pending update、分发观察、提交后通知。另有 3 项事件原生基准：emit / parallel 等待差异、同步抛错与嵌套错误汇总、Promise 短路与异步拒绝。跨进程代理尚未参与这些测试，完整对照仍待 P4；更新同样未交付。

### P2 必须增加的协议用例

- [ ] 两端分别在等待前检测已知执行器冲突；运行期 await 构成可证同步环时返回 SyncWaitCycle，而不是超时才算成功。
- [ ] current_thread 上无反向异步依赖的同步调用、同步立即回调、单纯返回 Promise / Future 正常完成，避免误拒绝。
- [ ] Rust 等 JS，JS 回调 Rust：满足 Send、持有期和执行环境条件的 Future 由后台执行器独立完成；验证借用不逃逸、取消不提前销毁、执行器随适配器清理。
- [ ] 对照 Send 但使用原 current_thread 的 timer、保存的 Handle 或生命周期任务：不得因 Send 就判可迁移；已知环报 SyncWaitCycle，未知业务依赖不冒充可检测。
- [ ] Node 同步等待链必须等待本线程 timer / Promise continuation 时，对可证环报错；构建对已知接口风险给出位置及原因，既不把警告当成完备检测，也不误拒绝仅返回 Promise 引用。
- [ ] 多 worker 的可证线程亲和等待环报错；未知业务锁不假称已检测。跨会话转发保留链标记。
- [ ] 把业务分发暂停在引用受理之后，此时处理紧随的 release；任务启动时对象仍有效。取消 / 解码失败不泄漏持有，不能由 I/O Worker 越序释放。
- [ ] 同一应用挂载 A / B，将 A 对象传入 B、再次转交及往返，复用存活代理；释放 B 后按本地最后持有者归还 A 的计数。
- [ ] 分别断开 A / B：受影响对象明确失败，无关会话继续；基准单独记录两跳转发成本。

## 2. 双向组合验收清单

每个勾选项都要求自动化跨进程测试及对应的原生对照。以下均未完成；P0 的孤立回归或 P1 的钩子验证不能勾选整条组合。设计 §9 已确认做不到的组合保留为未满足项，不能以“启动前正确报错”勾选为兼容。共享同一协议测试工具，但两个方向分别记录结果。

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
- [ ] K 的同步属性 / 方法及立即回调返回普通值；Rust 回调遵守设计 §3.3 的执行器与借用条件，仅符合条件的异步工作后台推进。
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
