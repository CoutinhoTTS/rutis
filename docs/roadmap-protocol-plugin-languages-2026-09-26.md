# 协议插件语言扩展路线图（2026-09-26）

> 历史方案参考，不作为本分支的需求或实施依据。当前定义见[协议插件需求](requirements-protocol-plugins.md)，新设计见[跨进程挂载设计](design-protocol-plugin-mount.md)。下文的具体机制、路线与验收门槛不沿用。

> 状态：Rust/rutis 与 TS/Cordis 优先；其余语言为后续验证方向，尚未实现，不属于基础版本的交付门槛。
> 核心协议、M0–M5 阶段及 T01–T24 见[协议插件设计](design-protocol-plugins-2026-09-25.md)。
> 执行模型为候选方案；每个 runner 实施前需根据核心原型结果复核，不因列入路线图而冻结 API。

## 一、 语言路线图与发布顺序

先用真正的 rutis 与 Cordis 验证对象、上下文、事件和清理范式，再向其他语言推广。
新增语言主要增加本地上下文/服务与清理适配、对象代理 SDK、runner 和互通测试，不在 rutis 核心增加语言分支。
不是只给每种语言套 JSON RPC，也不要求完整复制 Cordis 内部实现。下面表示验证依赖与投入顺序，不是日期承诺。
后续实验可在 M2 对象纵向成立后开始，正式支持须使用 M5 冻结协议并通过适用的生命周期、对象及平台验收。

| 批次 | 语言/运行环境 | 执行形态 | 主要目标与进入条件 |
| --- | --- | --- | --- |
| L0 首版优先 | Rust / rutis、TS / Cordis | 原生框架托管插件，共享或单成员 runner；Rust 首版静态组合 | M0 Go/No-go；M1–M5 验证 scope 对象、borrow 回调、parallel/serial 与清理；Rust/TS 缺一不可 |
| L1 通用语言 | Python、Go | 原生语言上下文与对象代理；Go 单成员或静态组合 runner | Rust/TS 对象模型成立后接入，同一对象/授权/生命周期语料，逐语言测量 |
| L1 系统自动化 | Shell（先 Bash/Linux） | 共享协议 runner，管理每插件或每次调用的子 shell/命令 | 优先覆盖已有脚本、文件/进程/系统工具；不要求作者手写协议循环 |
| L1 系统自动化 | PowerShell（先 PowerShell 7） | 一个 runner 进程内，每个 activation 有专属 Runspace | 复用 PowerShell 引擎与程序集，保留插件会话；按已通过的 OS 逐个平台发布 |
| L2 macOS 自动化 | AppleScript、JXA | macOS automation runner；初版管理脚本执行子进程 | macOS 传输/回收验证完成，并验证自动化授权和目标应用行为 |
| L3 按需求扩展 | Lua / LuaJIT | Lua runner，在共享进程内管理插件环境；具体 VM 布局单独验证 | 有轻量嵌入/脚本扩展用例后开始 |
| L3 按需求扩展 | Ruby、PHP、Perl | 各自的常驻语言 runner，兼容依赖环境成组 | 有具体生态需求后实现；PHP 按常驻 CLI 生命周期编写 |
| L3 替代运行环境 | Bun、Deno | 分别实现 JS/TS runner，与 Node 分组 | 通过同一语料和生命周期测试后支持，不由“能执行 JS”推定 Node 等价 |

语言 runner 复用同一契约和管理协议，但可以有不同执行模型。共享的最小单位是协议宿主，
是否复用解释器、Runspace 或仅复用脚本进程管理，由语言能力决定；独立进程也不自动等于完整权限沙箱。
基础版本的完成不要求 L1–L3 全部交付，后续每个 runner 独立记录实验/可用状态、运行环境与已通过测试。

**Python**：提供惯用 async 接口、本地服务对象、上下文和清理 scope；不要求作者维护远端对象 id。
每组使用锁定的依赖环境，冲突时拆组，不能靠修改 sys.path 声称隔离。原生对象与代理的身份、回调及释放规则遵守核心协议。

**Go**：独立可执行插件使用单成员组；接受统一构建的部署可将多个 package 与 runner 编译成共享可执行文件，复用 Go runtime。
factory/契约清单绑定 runner 哈希，缺失成员拒绝；代码更新重建并重启整组，配置更新只重装实例。
共享保留独立于主进程发布的能力，不保留各成员独立部署能力。构建由部署方流水线负责，宿主不临时编译插件源码。
使用生成接口、context.Context、显式 scope 和清理；取消 context 不等于任务退出，登记 goroutine 必须确认完成。
不采用 Go 动态 plugin 作为默认装载方式；工具链/共同依赖和不能卸载的限制见 [Go plugin 文档](https://pkg.go.dev/plugin)。
默认分组独立测量，不直接套用 Node 的结论。

## 二、 系统自动化 runner 的设计边界

**统一作者体验**：清单声明服务接口、配置及已实现能力；作者实现脚本函数/入口，runner 承担协议收发、
参数传递、值校验、对象表/代理、错误映射和任务登记。简单命令可只支持值；需要对象时由常驻 runner 保有真实对象并管理引用，不能把一次性子进程内已销毁的对象继续暴露。
清单指定解释器/语言及显式入口，不从用户提供的方法名拼接可执行源码。
协议端口和结果通道不占用插件普通日志输出；系统命令退出码、脚本错误与协议错误分别保留。

**Shell / Bash**：

- 共享 runner 可运行多个插件，但不把无关脚本全部 source 到同一个 shell；各自的变量、cwd、trap 和退出归属需分开。
  Bash 的子执行环境规则见 [Bash 手册](https://www.gnu.org/software/bash/manual/html_node/Command-Execution-Environment.html)。
- 首轮采用按调用启动脚本的命令式入口；需要持久状态时显式选择每 activation 常驻脚本模式，后者须另验装卸和清理。
  命令执行完成不销毁代理 fiber，后续请求仍能调用同一插件声明的服务。
- 参数用 argv 或约定的 JSON 输入传入，绝不把业务字符串插进 `sh -c` 的脚本内容。
  正常 stdout/stderr 用于日志，结构化结果由 helper 通过专用结果管道/fd 返回；helper 负责 JSON 编码，不能解析人类日志冒充结果。
- 子 shell、pipeline 和后台任务都属于其 activation 的执行记录。取消先请求停止并确认受管进程退出；
  未回收则不得发送任务完成确认。runner 崩溃仍按 runtime 组故障处理。
- 首版只承诺已测试的 Bash/Linux 组合；POSIX sh、其他 shell、其他 OS 分别声明并验收，不混称为全平台 Shell 支持。

**PowerShell**：

- 每个插件 activation 使用专属 Runspace，在同一 runner 进程复用引擎；不把有状态插件请求随机分配到任意池内 Runspace。
  多 Runspace 托管能力见 [PowerShell 托管文档](https://learn.microsoft.com/en-us/powershell/scripting/developer/hosting/creating-multiple-runspaces)。
- Runspace 分开管理会话变量和模块，但程序集、进程环境及部分静态状态仍共享；它不是进程隔离。
  不兼容模块/运行版本或要求独立故障范围时拆 runtime 组。
- 用参数绑定调用明确的命令/函数，不用拼接后的脚本文本承载用户数据。
  输出、错误、警告、信息和原生命令输出由 runner 分别处理，协议连接保持独立。
- 值返回按契约投影成 DTO；声明为对象的 .NET/系统实例留在所属 Runspace，通过 runner 导出对象引用，调用回到正确 Runspace。
  不承诺任意对象可以无损转成 JSON，也不默认暴露全部反射成员；对象 pin 必须在 Runspace 销毁前清理。
  显式测试 null、空数组、单元素数组、深层对象和错误信息；[ConvertTo-Json](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertto-json)的深度规则不能被当成完整对象序列化保证。
- 停止 pipeline 后还要确认关联任务和子进程完成；无法确认时 StopUnconfirmed，不把 Runspace.Dispose 当成任意后台工作的强杀保证。

**AppleScript / JXA**：

- 二者共享 macOS automation runner 的管理/传输层。初版可由 runner 调用脚本执行工具，
  后续再验证常驻 Cocoa/OSA 宿主是否值得引入；不承诺初版共享所有脚本对象或执行上下文。
  Apple 对两种语言的定位见 [Mac 自动化指南](https://developer.apple.com/library/archive/documentation/LanguagesUtilities/Conceptual/MacAutomationScriptingGuide/)。
- 业务参数作为 handler/run 参数或由宿主包装器提供的结构化数据传入，不把参数插值进 AppleScript/JXA 源码。
  值结果由包装器按契约显式转换。应用对象引用/Apple Event descriptor 不是通用 JSON；后续对象能力需由常驻 automation runner 保存可验证的应用对象定位与有效性，不等于保存用户应用内存。
  初版命令模式不支持的对象返回明确拒绝，不能将字符串描述伪装成活对象代理。
- 声明依赖的应用标识及自动化能力，验证应用未安装、未运行、用户拒绝授权等路径。
  以实际打包后的 runner/执行链测试 macOS 自动化授权，不以开发终端已授权推定发布后仍可用，见 [Apple 授权说明](https://support.apple.com/en-mz/guide/mac-help/mchl108e1718/mac)。
- 取消会停止等待并尝试终止脚本执行，但已发给目标应用的动作可能继续或已经完成；返回执行结果未知，不自动重放。
  用户自己的目标应用不属于 runtime 进程树，不能为停止脚本而杀 Finder、浏览器或其他用户应用。
- 第一批样例先使用应用提供的脚本接口；依赖 GUI 操作的脚本另行声明和验证所需系统权限、交互会话及应用版本。

## 三、 扩展能力与验收

runner 在 runtime/hello 声明对象引用、回调、事件模式、流及反向调用等已实现能力；插件清单及可达接口共同决定所需能力。
Rust/TS 首版必须满足核心基础验收 T01–T24；delegate、持久回调、waterfall、流按核心 X01–X04 独立验收，不能用能力子集绕过基础对象目标。后续脚本适配可以先仅支持值 unary，仍须生命周期、契约验证、取消/退出确认和错误路由。
缺少对象引用就拒绝包含对象的接口，缺少 callback/events/stream 则分别拒绝；缺少反向调用就拒绝 requires。
不得将不支持的对象降为快照、回调降为函数名或事件降为通知。没有静态类型的语言提供验证包装和对象 helper。

每个扩展 runner 的发布除适用的 T01–T24 外，还须通过以下验收；L1–L3 不通过的能力明确标 unsupported：

| 编号 | 验收场景 | 必须断言 |
| --- | --- | --- |
| E01 | 两个脚本插件、配置更新及单插件 stop | 代理/参数/状态不串用，声明为独立执行的成员正常清理不结束同组其他成员 |
| E02 | 空格、引号、换行、Unicode 及看似 shell/脚本代码的参数 | 原样作为数据到达，不被执行；结果与日志不会混入协议帧 |
| E03 | 子命令失败、脚本异常、错误返回类型、深层/空/单项数据与对象返回 | 明确区分值与对象引用；不把格式化输出当数据，不把已退出脚本内对象当活对象 |
| E04 | pipeline、后台任务、取消与退出竞态 | 完成确认晚于受管任务退出；不协作时如实报告，强停仍遵守组边界 |
| E05 | PowerShell 两个专属 Runspace、模块冲突、对象连续调用与释放 | 会话与对象归属稳定；销毁前清理对象，冲突者可拆组 |
| E06 | macOS 正式 runner 授权允许/拒绝、目标应用缺失、取消已发出的应用操作 | 原因可读，不自动重放，不关闭用户应用，不宣称操作回滚 |
| E07 | 值 unary runner 装载对象/回调/事件/流/反向调用插件，换另一 OS 或解释器 | 不支持的能力在装载前拒绝且不降级；通过对应环境测试才标可用 |

路线图不会重新引入预设的资源配额。性能与内存对照仍作为测量任务，流控优化、池规模和默认时限待数据支持后独立讨论。
