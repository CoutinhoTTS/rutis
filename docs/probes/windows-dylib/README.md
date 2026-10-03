# Windows dylib 可行性实验(#103)

验证记录见 `docs/design-dylib-sdk-2026-09-24.md` §十一“Windows 可行性验证(2026-10-03)”。
这里是实验代码,每个目录是独立的 Cargo 工程(各自带 `[workspace]`,不属于根 workspace)。
工具链跟随仓库根目录的 `rust-toolchain.toml`(1.98.1),目标 `x86_64-pc-windows-msvc`。

| 目录 | 用途 |
| --- | --- |
| `pecount/` | 用 `object` crate 读 PE:导出表计数(按 crate 分组)、导入的 DLL、节列表、引导 blob 检查 |
| `w1-anchor/` | W1:按宿主方式构建真实的 `rutis-sdk`(依赖图与 `rutis-cli --features dylib-plugins` 相同,只去掉 Linux 专用的 `rutis-dylib`) |
| `w1-growth/` | W1:`rutis-sdk` 依赖集合的副本,可选加 serde derive、regex/tracing/chrono/uuid、reqwest,看导出数增长 |
| `runtime/` | W2、W3、W5–W8:`sdk`(dylib,重导出 tokio)、`greeter`/`w5plugin`/`w5bad`(插件 dylib)、`host`(宿主,`LoadLibraryExW` 加载插件) |
| `run-runtime.ps1` | 构建 `runtime/`,摆出发布目录和缓存目录,依次运行 W2–W8,打印 `RESULT ...` 行 |

## 重新运行

本地有 Windows + MSVC 时,在 pwsh 中:

```powershell
./docs/probes/windows-dylib/run-runtime.ps1            # W2-W8
cargo build --release --manifest-path docs/probes/windows-dylib/w1-anchor/Cargo.toml   # W1,之后用 pecount 计数
```

W1 前先 `cp Cargo.lock docs/probes/windows-dylib/w1-anchor/`,让依赖版本与根 workspace 一致;
并设置 `RUTIS_SDK_ARTIFACT_SHA256` 为 64 个 0。

在 CI 上:把下面的 workflow 存为 `.github/workflows/probe-windows-dylib.yml`,推到任意 `probe/**` 分支,
用 `gh run list --branch <分支>` 和 `gh run view <id> --log | grep RESULT` 看结果。验证完删掉这个文件。

```yaml
WORKFLOW_PLACEHOLDER
```
