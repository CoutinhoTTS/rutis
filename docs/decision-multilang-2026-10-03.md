# 决策记录：其他语言不进 rutis，Cordis 运行时插件化（2026-10-03）

本文记录一次从“扩展多语言”出发、最后收窄成“只改 P6”的讨论，保留思考过程和理由，供以后再提多语言时直接参考。

## 一、起点

原有路线图（[roadmap-protocol-plugin-languages](roadmap-protocol-plugin-languages-2026-09-26.md)，已标为历史参考）计划在 Cordis 之后接入 Python、Go、Shell、PowerShell、AppleScript/JXA 等语言。Cordis 挂载基本完工后，重新研究其他语言该怎么做，重点是 PowerShell、Bash、AppleScript 这类系统级语言；Python 参考 min-cordis 的 Python 实现。

## 二、调研（六路并行，原始报告见 [plan/analysis/multilang](plan/analysis/multilang/README.md)）

| 方向 | 主要结论 |
| --- | --- |
| 现有协议契约 | 协议 v1 的大部分与语言无关。但有几处写死了 Node：请求 id 前缀只接受 `node:`、启动命令固定为 `node`、生成器直接输出 Rust 源码而没有中立的接口描述。另有几条隐含约定：Rust future 是惰性的；`path` 传错会死锁；dispose 后 runner 必须以 0 退出 |
| Python / min_cordis | 技术上可行，runner 可近乎逐行移植。但 min_cordis 尚未发布，还没有任何插件；需要新约定（seam 类、类型化 Context、EVENTS 表）；venv 不能迁移部署 |
| PowerShell | 可行形态是每个挂载一个常驻 pwsh，每个插件一个 runspace。坑很多：线程亲和性错误会让整个进程崩溃；`Stop()` 会被孙进程阻塞，实测 102 s；不能用 ConvertTo-Json。价值主要在 Windows，而 interop 只支持 Unix |
| Bash | 可行形态是 Rust 侧每次调用起一个进程，结果走 fd 3，以进程组为单位取消，用 argc 注解声明接口。只能传数据；用 setsid 脱离进程组的进程无法回收 |
| AppleScript / JXA | 可行形态是常驻的原生 OSA host，用 send proc 实现宿主调用和日志捕获。TCC 授权归负责进程，CI 里测不了；平台处于维护状态，JXA 基本无人维护 |
| 先例 | Neovim 正在拆掉“每语言一个共享宿主”的模式（neovim#27949）；Pulumi 的“各语言抽取 → 中立描述 → 生成类型化 SDK”与 rutis 的思路最接近。教训：不要静默降级，不要用单独刷新的清单，协议版本不要与宿主发布绑死 |

据此先写过一份完整设计草案：框架型（Node、Python）加脚本型（PowerShell、Bash、AppleScript），配一套中立接口描述和统一的 Rust 代码生成器，分 P0–P7 推进。草案已撤回。

## 三、反面意见（撤回草案的原因）

**价值：**

1. 为脚本生成类型化绑定，服务的是错误的用户。写 Rust 的人用 `Command` 就够了；脚本真正的用户是 agent，它要的是运行时发现和动态调用。
2. Python 的完整方案是给一个不存在的生态做兼容：没有 min_cordis 插件，min-cordis 也没有发布。
3. Cordis 范式套在脚本上是包装过度。依赖重载、服务换值、清理树、子插件，脚本都用不上，剩下的只是带类型的 RPC。
4. 维护面与收益不成比例。一人维护五种运行时，结局多半是每种都停在 demo 水平。
5. PowerShell 离开 Windows 就没有多少用户；AppleScript 没有具体用例。

**技术上限：**

1. 脚本只能传数据，跨语言组合的上限就是一个命令调用层。
2. 取消只能尽力而为，结果只能报“未知”。
3. 类型看着强，实际弱：bash 全是字符串；PowerShell 的 `[OutputType]` 只是提示。
4. 构建要依赖 pwsh、venv、macOS，CI 和交叉编译都会变难。
5. Python 被钉在单个事件循环线程上，只适合做控制面。
6. agent 加任意系统脚本却没有沙箱，是真实的安全风险。

## 四、推理过程

1. **这些都该是插件。** 按 rutis 自己的范式，语言支持应拆成：运行时插件提供执行服务，每个脚本作为 inject 它的插件或 loader 行，agent 工具也是插件。这样“多语言”不再是框架特性，只是可选的插件。
2. **既然是插件，就不该在 rutis 库里做。** 它们与 rutis 的核心职责无关，需要时在库外用公开 API 实现即可。构建期代码生成装不进插件模型，这也从另一面说明脚本不该走类型化绑定。
3. **rutis 唯一该做的跨语言工作，是与同范式的 Cordis 互相挂载。** 两边的服务、依赖门控、清理、事件可以一一对应。两个方向只做一个：2026-10-02 已定 rutis 是宿主，反方向保持冻结。
4. **回头检查 Cordis 挂载本身是否已经都是插件。** 静态挂载是：生成的挂载就是普通的 rutis 插件。P6 的动态路径只做了一半：JS 行是插件，但它们共用的 Node 进程和 Cordis Context 挂在 `InteropResolver` 的 `OnceCell` 上，不受生命周期管理。具体问题：
   - 进程崩溃后，各行仍持有失效的进程，没有任何插件进入失败状态；
   - 宿主服务不参与依赖门控；
   - 诊断里看不到这个运行时。

## 五、结论

| 事项 | 决定 |
| --- | --- |
| PowerShell、Bash、AppleScript/JXA | 不在 rutis 内做。需要时在库外做成插件 |
| Python（min_cordis） | 同范式，但现在没有用户，归入“库外、以后再说” |
| 反方向（Cordis 宿主挂载 rutis） | 继续冻结 |
| 中立接口描述、启动方式抽象、`node:` 前缀放宽 | 不做；这些只是给其他语言铺路 |
| P6 | **要改**：Cordis 运行时插件化（#109），设计见 [design-cordis-runtime-plugin](design-cordis-runtime-plugin-2026-10-03.md) |
| 需求文档 | §8 补充结论（已改） |

## 六、什么情况下重新考虑

- 有真实的 min_cordis 插件要挂载：按 Cordis 挂载的模式做 Python 运行时插件；调研报告里的 runner 移植方案可以直接用。
- agent 需要把系统脚本当作工具：在库外做“命令型运行时插件”，加上运行时发现和 JSON Schema，不做代码生成；bash 和 PowerShell 报告里的进程与取消结论可以直接用。
- 要做 Windows 自动化：PowerShell 与 Windows 支持一起立项。
