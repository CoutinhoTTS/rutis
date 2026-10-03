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
| `w8-real/` | W8:真实 `rutis-sdk` + `tests/dylib-fixtures/greeter-v1` 的副本(去掉 Linux 专用的 `.init_array`),宿主在 `Ctx::root()` 上运行插件 |
| `run-runtime.ps1` | 构建 `runtime/` 和 `w8-real/`,摆出发布目录和缓存目录,依次运行 W2–W8,另做 SDK 字节可复现的检查,打印 `RESULT ...` 行 |

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
# Temporary: Windows dylib feasibility probe (#103). Removed before merge;
# a copy lives in docs/probes/windows-dylib/README.md.
name: probe-windows-dylib

on:
  push:
    branches: ["probe/**"]

concurrency:
  group: probe-windows-dylib-${{ github.ref }}
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: never
  RUTIS_SDK_ARTIFACT_SHA256: "0000000000000000000000000000000000000000000000000000000000000000"

# Profiles: release; dev; dev with opt-level 2 for rutis-sdk only; dev with
# opt-level 2 for every non-workspace package (all SDK dependencies).
jobs:
  w1-real-sdk:
    name: W1 real SDK (${{ matrix.profile }})
    runs-on: windows-2025
    timeout-minutes: 90
    strategy:
      fail-fast: false
      matrix:
        profile: [release, dev, dev-sdk-o2, dev-deps-o2]
    defaults:
      run:
        shell: bash
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.98.1
      - name: Toolchain
        run: rustc -vV && cargo -V
      # rutis-cli itself fails to compile on Windows (rutis_dylib::Loader is
      # Linux-only), but Cargo has already linked rutis_sdk.dll for its graph.
      - name: Try the real host anchor (rutis-cli --features dylib-plugins)
        if: matrix.profile == 'release' || matrix.profile == 'dev'
        continue-on-error: true
        run: |
          flag=""; [ "${{ matrix.profile }}" = release ] && flag="--release"
          set +e
          cargo build -p rutis-cli --features dylib-plugins $flag > rutis-cli.log 2>&1
          code=$?
          grep -E '^error|LNK[0-9]+' rutis-cli.log | head -30
          echo "RESULT W1.rutis-cli-build[${{ matrix.profile }}]: exit=$code"
          exit 0
      - name: Build the probe anchor (same graph minus rutis-dylib)
        run: |
          case "${{ matrix.profile }}" in
            release) flags=(--release);;
            dev) flags=();;
            dev-sdk-o2) flags=(--config 'profile.dev.package.rutis-sdk.opt-level=2');;
            dev-deps-o2) flags=(--config 'profile.dev.package."*".opt-level=2');;
          esac
          cp Cargo.lock docs/probes/windows-dylib/w1-anchor/Cargo.lock
          export CARGO_TARGET_DIR="$PWD/target-w1"
          set +e
          cargo build --manifest-path docs/probes/windows-dylib/w1-anchor/Cargo.toml "${flags[@]}" > anchor.log 2>&1
          code=$?
          tail -5 anchor.log
          grep -E 'LNK[0-9]+|^error' anchor.log | head -30
          echo "RESULT W1.anchor-build[${{ matrix.profile }}]: exit=$code"
          CARGO_TARGET_DIR="$PWD/target-pecount" cargo build --release --manifest-path docs/probes/windows-dylib/pecount/Cargo.toml
          exit $code
      # After all builds: in bash, the dev shell PATH can let Git's link.exe shadow MSVC's.
      - uses: ilammy/msvc-dev-cmd@v1
      - name: Count exports
        run: |
          dir=target-w1/$([ "${{ matrix.profile }}" = release ] && echo release || echo debug)
          ls -la $dir/rutis_sdk.dll $dir/rutis_sdk.dll.lib $dir/w1-anchor.exe
          ./target-pecount/release/pecount.exe exports $dir/rutis_sdk.dll | tee exports.txt
          echo "RESULT W1[${{ matrix.profile }}]: $(head -1 exports.txt)"
          real=target/$([ "${{ matrix.profile }}" = release ] && echo release || echo debug)/rutis_sdk.dll
          if [ -f "$real" ] && [ "${{ matrix.profile }}" != dev-sdk-o2 ] && [ "${{ matrix.profile }}" != dev-deps-o2 ]; then
            echo "RESULT W1.rutis-cli-graph[${{ matrix.profile }}]: $(./target-pecount/release/pecount.exe exports $real | head -1)"
          fi
          dumpbin -nologo -exports $dir/rutis_sdk.dll | grep -E 'number of (functions|names)'
          echo "W4 dumpbin -dependents rutis_sdk.dll:"
          dumpbin -nologo -dependents $dir/rutis_sdk.dll | sed -n '/Image has the following dependencies/,/Summary/p'
          std=$(/usr/bin/find "$(rustc --print sysroot)" -name "std-*.dll" | head -1)
          echo "W4 dumpbin -dependents $std:"
          dumpbin -nologo -dependents "$std" | sed -n '/Image has the following dependencies/,/Summary/p'
          echo "W4 dumpbin -dependents w1-anchor.exe:"
          dumpbin -nologo -dependents $dir/w1-anchor.exe | sed -n '/Image has the following dependencies/,/Summary/p'
          cp "$std" $dir/
          ./$dir/w1-anchor.exe && echo "RESULT W1.anchor-run[${{ matrix.profile }}]: ok"

  w1-growth:
    name: W1 growth (${{ matrix.variant }}, ${{ matrix.profile }})
    runs-on: windows-2025
    timeout-minutes: 60
    strategy:
      fail-fast: false
      matrix:
        profile: [release, dev]
        variant: [base, serde, misc, web, all]
        include:
          - { variant: all, profile: dev-deps-o2 }
    defaults:
      run:
        shell: bash
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.98.1
      - name: Build
        run: |
          case "${{ matrix.profile }}" in
            release) flags=(--release);;
            dev) flags=();;
            dev-deps-o2) flags=(--config 'profile.dev.package."*".opt-level=2' --config 'profile.dev.package.rutis-sdk-copy.opt-level=2');;
          esac
          case "${{ matrix.variant }}" in
            base) feats=();; serde) feats=(--features serde);; misc) feats=(--features misc);;
            web) feats=(--features web);; all) feats=(--features serde,misc,web);;
          esac
          export CARGO_TARGET_DIR="$PWD/target-g"
          set +e
          cargo build --manifest-path docs/probes/windows-dylib/w1-growth/Cargo.toml -p anchor "${flags[@]}" "${feats[@]}" > build.log 2>&1
          code=$?
          tail -5 build.log
          grep -E 'LNK[0-9]+|^error' build.log | head -30
          echo "RESULT W1.growth-build[${{ matrix.variant }},${{ matrix.profile }}]: exit=$code"
          exit $code
      - name: Count exports
        run: |
          dir=target-g/$([ "${{ matrix.profile }}" = release ] && echo release || echo debug)
          CARGO_TARGET_DIR="$PWD/target-pecount" cargo build --release --manifest-path docs/probes/windows-dylib/pecount/Cargo.toml
          ./target-pecount/release/pecount.exe exports $dir/rutis_sdk.dll | tee exports.txt
          echo "RESULT W1.growth[${{ matrix.variant }},${{ matrix.profile }}]: $(head -1 exports.txt)"

  runtime:
    name: W2-W8 runtime probes
    runs-on: windows-2025
    timeout-minutes: 60
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.98.1
      - name: Run
        shell: pwsh
        run: ./docs/probes/windows-dylib/run-runtime.ps1 -Work "$env:RUNNER_TEMP\wdyl"
      - uses: ilammy/msvc-dev-cmd@v1
      - name: W4 dumpbin
        shell: pwsh
        run: |
          $std = Get-ChildItem "$env:RUNNER_TEMP\wdyl\app" -Filter 'std-*.dll' | Select-Object -First 1
          foreach ($f in @($std.FullName, "$env:RUNNER_TEMP\wdyl\app\sdk.dll", "$env:RUNNER_TEMP\wdyl\app\host.exe")) {
            Write-Host "W4 dumpbin /dependents $f"
            dumpbin /nologo /dependents $f
          }
```
