# Mounting shell-script plugins in rutis: design research (Bash first)

Date: 2026-10-03. Scope: how a rutis application can mount shell scripts ("bash plugins", system automation) as typed rutis services. This complements `rutis-interop` (which mounts real Cordis/Node plugins over protocol v1). No changes were made in the rutis repository. All experiments live in `scratchpad/experiments/bash/` (paths below are relative to that directory); exact commands and outputs are in §13.

Test machines: macOS 26.6.2 on an Apple M4 Max (`/bin/bash` 3.2.57, `/bin/zsh` 5.9, `/bin/dash`, `/usr/bin/jq` 1.7.1-apple, Rust 1.98.1). Linux was tested in Docker Desktop's arm64 VM (kernel 6.10.14-linuxkit) using `debian:bookworm-slim` / `rust:1.98.1-bookworm` (bash 5.2.15) and `bash:5.2` (Alpine 3.22, bash 5.2.37). Linux timings are from that VM, not bare metal.

---

## 0. Executive summary and recommendation

**Recommendation in one paragraph.**
- **Execution model.** Implement a *shell mount* inside `rutis-interop` as a Rust-side host. It needs no Node and no separate runner process. Each method call runs as **one `bash` process in its own process group** (per-call model).
- **Calls and results.** The call receives typed arguments as **argv**, with argc-compatible variables. It returns its result through **NUL-framed records on a dedicated channel** (fd 3, created with Concourse's `exec 3>&1 1>&2` swap so the spawn stays on `posix_spawn`). stdout and stderr are logs. The Rust side converts results to the **declared types**, so authors never need jq.
- **Interface declaration.** The interface comes from **argc comment annotations**, plus rutis extensions under `# @meta rutis.*` (argc rejects unknown tags). At build time these become the same kind of generated module the Cordis mounts produce: `Config`, a service struct with `async fn` methods, record structs, event types, and host-service traits.
- **Cancellation.** Dropping a call future sends **SIGTERM to the call's process group**, then SIGKILL after a grace period.
- **Process ownership.** Processes started by a call are cleaned up when the call ends. Processes started by the `apply` hook live until `dispose`.
- **Host death.** A per-mount *lifeline pipe* plus a tiny watchdog per call kills the group if the rutis process itself dies. This works on macOS too, where no PDEATHSIG or subreaper exists.
- **Supported features.** Values only (string, int, number, bool, enum, string list, record of scalars, raw JSON). Events can be emitted and received as notifications. Value-only host services can be injected through a bash `rutis_call` helper.
- **Rejected at build or mount time.** Live objects, callbacks, streams, sync methods, and waterfall/bail events.
- **Bash version.** The runtime helpers must stay in the **bash 3.2 subset** so stock macOS works. Plugins may declare a newer minimum. zsh and POSIX sh are separate, later dialects. Windows is out of scope.

**Most design-relevant findings (measured).**
1. **Per-call cost** is 0.69 ms (Linux) or 2.6 ms (macOS) per call, against 0.046 ms for a persistent bash loop. Once a method runs any external program, the per-call model's *extra* cost stays fixed at about 0.6 ms (Linux) or 2.5 ms (macOS): with one `uname` call it is 0.92 vs 0.30 ms on Linux and 3.9 vs 1.3 ms on macOS. A real system-automation call costs tens of milliseconds or more, so per-call overhead is not the deciding factor. The full tokio prototype measured 0.66 ms per call (Linux) and 3.4 ms (macOS); 200 concurrent calls took 64 ms on macOS (§1, §12).
2. **A persistent bash loop is slow at ingesting data.** bash `read` does **one `read(2)` per byte on a pipe**: 10,006 syscalls for a 10 KB field. A 100 KB argument costs 18–23 ms and 1 MB costs 296 ms, against 1.7–4 ms and 20 ms through argv. Even from a regular file, `read -d ''` took 67 ms for 200 KB. Re-sourcing a 7,000-line (160 KB) script on every call costs about 6 ms (§1, §2).
3. **Completion must be detected by process exit, not by pipe EOF.** Any backgrounded child inherits stdout, stderr and fd 3 and keeps them open. Reading to EOF blocked 3.01 s in the test; waiting for exit and then draining without blocking took 0.00 s. This bit my own test harness three times (§3).
4. **Signals.**
   - TERM sent to bash alone is **deferred until the foreground child exits**. This held for bash 3.2, bash 5.2, zsh and dash.
   - TERM sent to the **process group** kills the child and lets bash's trap run immediately.
   - Only bash runs an EXIT trap on a fatal TERM.
   - `setsid` and `set -m` children escape group kill on both OSes.
   - On Linux a `PR_SET_CHILD_SUBREAPER` supervisor or cgroup v2 `cgroup.kill` catches them. The cgroup must be joined *before* the process forks (§5).
5. **Spawn path matters.** `process_group(0)` keeps Rust's `posix_spawn` path. Any `pre_exec` (for example to `dup2` a fd 3) forces fork+exec. On Linux with a 4 GiB host RSS that costs 17.5 ms per spawn instead of 0.17 ms (§5).
6. **macOS FIFO quirk.** When several processes block reading one *named FIFO* and the last writer closes, most never see EOF: 15–35 of 30–40 readers stayed stuck. Linux delivers EOF to all of them, and so do anonymous pipes on both OSes. A per-mount lifeline must therefore be an anonymous pipe (§5).
7. **Environment injection.** `BASH_ENV`, exported functions (`BASH_FUNC_ls%%`), `SHELLOPTS=xtrace` and `CDPATH` all alter or inject code into a non-interactive bash. Use an environment allowlist; `bash -p` adds defense in depth but does not ignore CDPATH on 3.2 (§2).
8. **Data limits.**
   - Linux rejects any single argv or env string of 128 KiB or more (`MAX_ARG_STRLEN`).
   - macOS has only a 1 MiB total limit.
   - NUL can never be passed and is silently dropped by `$(...)` in 3.2.
   - jq turns invalid UTF-8 into U+FFFD.
   - On macOS `ps` is setuid and shows every user's argv, so secrets must not go in argv (§2).

---

## 1. Execution models and trade-offs (Q1)

### 1.1 The three models

| | (a) one process per call | (b) persistent bash per activation | (c) many plugins sourced into one shell |
|---|---|---|---|
| Invocation | `bash shim.bash plugin.sh <fn> args…` | `bash loop.sh plugin.sh`, framed requests on stdin | one shell sources A, B, …; dispatches calls |
| Isolation | full: variables, cwd, traps, options and `exit` are scoped to the call | per plugin; leaks between calls of the same plugin | none (see 1.4) |
| Concurrency | real parallelism (one process per call) | serial: one request at a time per activation | serial for *all* plugins |
| Cancellation | kill the call's process group; nothing else is affected | killing the running handler kills the loop shell and its state; cooperative stop is unreliable because traps are deferred while a foreground child runs | kills every plugin |
| State across calls | files (`$RUTIS_STATE_DIR`) | shell variables (the point of the model) | shell variables, shared by accident |
| Large inputs | argv: kernel copy (fast) | bash `read` on a pipe: 1 syscall per byte | same as (b) |
| Script parse cost | every call (≈6 ms for 7,000 lines) | once | once |
| Crash blast radius | one call | one activation | everything |

### 1.2 Measured costs

The harness is `harness/src/main.rs`. It uses Rust `std::process::Command` (which uses `posix_spawn`) with medians over 500 runs; ms per call.

| Scenario | macOS (bash 3.2) | Linux VM (bash 5.2) |
|---|---|---|
| spawn `/usr/bin/true` (OS floor) | 0.97 | 0.18 |
| `bash -c :` | 1.37 | 0.34 |
| `dash -c :` | 1.03 | 0.20 |
| `zsh -c :` / `zsh -f -c :` | 2.89 / 2.65 | – |
| `/bin/sh -c :` (macOS `/bin/sh` is a shim that re-execs bash) | 2.41 | – |
| `jq -n 1` | 1.80 | – |
| `python3 -c pass` | 20.1 | 3.9 |
| **(a) per-call, trivial method** (`echo`, stdout swapped to fd 3) | **2.63** | **0.69** |
| (a) per-call, real fd 3 via `pre_exec` dup2 (fork path) | 3.09 | 0.72 |
| (a) per-call, result file in env | 3.13 | – |
| (a) method that runs `uname` once | 3.87 | 0.92 |
| (a) method that runs `/bin/echo` + `ls` and prints logs | 5.29 | 1.20 |
| **(b) persistent loop, trivial method** | **0.046** | **0.046** |
| (b) method that runs `uname` once | 1.32 | 0.30 |
| (b) method with `/bin/echo` + `ls` + logs | 2.63 | 0.55 |
| (a) per-call with a 7,000-line / 160 KB plugin file | 8.83 | 6.39 |
| (b) persistent with the same file | 0.056 | 0.045 |
| (a) argv 10 KB / 100 KB / 1 MB argument | 2.73 / 4.13 / 20.1 | 0.78 / 1.74 / (E2BIG) |
| (b) stdin 10 KB / 100 KB / 1 MB argument | 1.95 / 23.4 / 296 | 1.53 / 18.1 / – |

Interpretation:
- The fixed cost of (a) over (b) is about 2.6 ms on macOS and 0.65 ms on Linux. Most of it is the OS process spawn: macOS `posix_spawn` of `true` alone takes 0.97 ms.
- Methods in a system-automation plugin almost always run external programs, and those cost the same in both models. One `uname` already makes (b) only 2.5 ms cheaper on macOS and 0.6 ms on Linux.
- (b) gets **slower** than (a) at about 10 KB of argument data, because bash's `read` processes input byte by byte (§2.3).
- `/bin/sh` on macOS costs 2.4 ms because it is a re-exec shim. Spawn `/bin/bash` (or a configured interpreter path) directly.

### 1.3 Why (b) is hard to get right

The prototype loop (`05-loop/loop.sh`) works in bash 3.2. It reads NUL-framed requests with `read -r -d ''`, keeps state (`incr` returned 510 after 510 calls), and sends handler stdout to logs. But:

- **Cancellation conflicts with state.** A handler runs inside the loop shell. Stopping it means signalling that shell, whose trap is deferred until the current foreground child exits (§5.4), or killing the shell and losing the state. Running handlers in `( … )` subshells would make cancellation clean but throw the state away, which is the whole reason for the model.
- **Errors leak.** `exit`, `set -e` and changes to `cd`, traps, `IFS` or `shopt` in one handler persist into every later call, or end the loop entirely.
- **Calls are serialized.** One slow call blocks the activation. Concurrency would need several loops, which is a pool and no longer "state in variables".
- **Input is slow** past a few KB (table above).
- Bazel's persistent workers show the protocol work this requires: request ids, "answered exactly once", cancellation support and idle reaping (https://bazel.build/remote/persistent, https://bazel.build/remote/multiplex). Nushell stops idle plugins after 10 s by default (https://www.nushell.sh/book/plugins.html).

**Conclusion:**
- v1 should support only (a).
- State goes in files under a per-activation `$RUTIS_STATE_DIR`.
- Keep scripts small, since parsing is paid on every call. Measured: about 0.8–0.9 ms per 1,000 lines (7,024 lines added 5.7 ms on Linux and 6.2 ms on macOS).
- If a "warm session" mode is wanted later, make it opt-in per plugin (`@meta rutis.mode session`). A cancelled call would then **restart the session** and report the state as lost, rather than pretend the cancellation was cooperative.

### 1.4 Why (c) is bad (demonstrated)

`11-shared-shell/host.sh` sources two independent plugins into one bash and serves calls:

```
1) A's log() after B loaded:
B: work done in /tmp                 <- A's call ran B's log(): function names collide
2) B sees A's leftovers:
B: config is /etc/b.conf ..., cwd is /tmp   <- A's `cd /tmp` leaked into B; `config` was overwritten (A now sees B's)
B cleanup                            <- A's `set -e` leaked: B's harmless failing `ls` killed the shared shell;
                                        only B's EXIT trap ran because B's `trap … EXIT` replaced A's
shared shell exit status: 1          <- call 3 never ran
```

Plugins sharing one shell share their functions, variables, cwd, options (`set -e`), traps and `exit`. This confirms the old roadmap's warning (§11).

---

## 2. Safe input (Q2)

### 2.1 Channels compared

| Channel | Injection-safe | Size | Speed into a bash variable | Who can see it | Notes |
|---|---|---|---|---|---|
| **argv** (`execve` array) | yes; never parsed as code | Linux: **128 KiB per string** (`MAX_ARG_STRLEN`, 32 pages) and about 2 MiB total including env; macOS: 1 MiB total, no per-string limit | kernel copy; fastest (1 MB in 20 ms) | Linux: `/proc/<pid>/cmdline` world-readable unless `hidepid`; macOS: setuid `/bin/ps` shows **every user's** argv | best default for ordinary arguments |
| env vars | yes | same limits (counts against ARG_MAX and the per-string limit) | kernel copy | same uid (Linux `/proc/pid/environ` needs ptrace-read access; macOS `ps -E` own uid only) | inherited by **every** child; fine for config, poor for per-call data |
| stdin pipe | yes | unlimited | **byte-at-a-time**: 100 KB ≈ 18–23 ms | private | ends up consumed by child programs unless redirected |
| file in a private temp dir (0700/0600) | yes | unlimited | `read -d ''` loop: 200 KB = 67 ms; `$(cat f; printf .)` = 8.7 ms (fork) | private | the Ansible "args file" approach; needs cleanup |
| `sh -c "<string>"` interpolation | **no** | – | – | – | never; Alfred explicitly recommends argv over `{query}` substitution for this reason (https://www.alfredapp.com/help/workflows/inputs/script-filter/) |

Sources: execve(2) https://man7.org/linux/man-pages/man2/execve.2.html; xnu `ARG_MAX` 1 MiB (https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/sys/syslimits.h) with no per-string check in kern_exec.c; proc(5) https://man7.org/linux/man-pages/man5/proc.5.html; proc_pid_environ(5) https://man7.org/linux/man-pages/man5/proc_pid_environ.5.html; macOS `ps` source https://raw.githubusercontent.com/apple-oss-distributions/adv_cmds/main/ps/print.c.

Measured (§13.6):
- Linux: a single 131,071-byte argument execs; 131,072 bytes fails with "Argument list too long" (exit 126 from bash). An env string `X=<131070 bytes>` also fails.
- macOS: a single 1,040,000-byte argument works; 1,048,576 fails.
- A hostile argument containing quotes, a newline, `$(touch /tmp/pwned)`, backticks, `; rm -rf ~`, Unicode and an empty string round-tripped exactly through argv into `"$@"`. The tokio prototype showed the same.

### 2.2 Recommendation

- **Ordinary parameters** (scalars, enums, flags, lists of short strings) go in **argv**. The shim turns them into positional parameters and argc-style `argc_<name>` variables (§6.4) using assignment without `eval` (`printf -v "argc_$name" '%s' "$value"`, where the name was validated at build time).
- **The host checks sizes before spawning.** Any string over 64 KiB, or argv+env over about 512 KiB, fails with `Error::Value` and guidance. Do not let `execve` return E2BIG, because the Linux and macOS limits differ.
- **Large data** goes in a `<FILE>`-typed parameter. The host writes the bytes to a 0600 file in the call's private temp dir and passes the *path*. The script hands the file to tools (`jq`, `grep`, `tar`) rather than loading it into bash variables. This is how Concourse (directory argument) and Ansible (args file) work.
- **Secrets** (`@meta rutis.secret <param>`) never go in argv or env. The host writes them first on the call's stdin (host→script channel), NUL-framed. The shim reads them before calling the function. They are small, so byte-wise `read` cost is irrelevant.
- **Config** (`@env` declarations, §8) goes in env vars. This is the Unix convention and what tools such as `gh` and `git` expect. Document that env is visible to same-uid processes and inherited by children.

### 2.3 Unicode, newlines and NUL

Measured in `06-limits/data.sh`, §13.6:
- **NUL**: no channel can carry it.
  - argv and env are C strings; Rust's `Command` rejects NUL with `InvalidInput`.
  - Bash variables cannot hold NUL. `$(printf 'a\0b')` gives `ab`: silently in 3.2, with "warning: command substitution: ignored null byte in input" in 5.2.
  - Binary values must therefore be files (or base64, if a `bytes` type is added later).
- **Newlines and arbitrary bytes**: fine in argv and variables. Invalid UTF-8 (`\377`) survives in bash as raw bytes.
  - JSON cannot carry non-UTF-8 strings: jq turns `bad\377byte` into `"bad�byte"` silently (https://github.com/jqlang/jq/issues/2660).
  - **The rutis frame protocol is NUL-delimited bytes**, so non-UTF-8 values reach Rust intact. The host decides by declared type: `string` requires UTF-8 and errors otherwise; a `path` type can map to `PathBuf` through `OsString` and keep arbitrary bytes, which matters for Linux file names.
- **Locale**: `${#s}` counts characters by locale. `héllo✓` gives 6 under `en_US.UTF-8` or `C.UTF-8` and 9 (bytes) under `C`. Pass the host's `LANG`/`LC_*` through explicitly (allowlist) and never set a locale silently.

### 2.4 Environment hygiene (measured)

Tested with `07-env/probe.sh` on bash 3.2 and 5.2 (§13.7):

| Variable inherited by bash | Effect |
|---|---|
| `BASH_ENV=file` | file is **sourced before the plugin** (arbitrary code) |
| `BASH_FUNC_ls%%=() { …; }` | exported function **replaces `ls`** for the script |
| `SHELLOPTS=xtrace` | xtrace on: every command and expanded value goes to stderr/logs (secret leak) |
| `CDPATH=.:/tmp` | `cd sub` prints the directory to stdout, so `$(cd sub && pwd)` returned **two lines** |
| `IFS=x` | ignored; bash resets IFS (good) |

**Use an allowlist with `env_clear()`.** Keep `PATH` (configurable; note that macOS GUI apps get a minimal PATH because `path_helper` runs only for login shells), `HOME`, `USER`/`LOGNAME`, `TMPDIR`, `LANG`/`LC_*` and `TZ`, plus declared config and `RUTIS_*` runtime variables.

Also pass **`bash -p`** as defense in depth. Privileged mode stops processing `BASH_ENV`/`ENV`, function import and `SHELLOPTS`/`BASHOPTS` (bash manual, The Set Builtin: https://www.gnu.org/software/bash/manual/html_node/The-Set-Builtin.html).
- Measured on 5.2: all four hazards neutralized.
- Measured on 3.2: everything **except `CDPATH`**.
- `-p` affects only that one bash process. Child shells the script starts would import the environment again, so the allowlist remains the primary control.

---

## 3. Result and output channel (Q3)

### 3.1 Options

| Option | Robust to programs printing to stdout | Streaming (events, host calls during a call) | Problem with escaped background children | Spawn path |
|---|---|---|---|---|
| stdout JSON (Terraform, CNI, Ansible, Alfred) | **no**: any `echo` or child output corrupts the result | no | readers hang until EOF | posix_spawn |
| result file named in env (`$GITHUB_OUTPUT`) | yes | no; read after exit | none | posix_spawn |
| dedicated fd 3 via `pre_exec` + `dup2` | yes | yes | must not wait for EOF | **fork** (slow for big hosts, §5.6) |
| **fd 3 via the stdout swap**: host hands the frame pipe in as stdout, script runs `exec 3>&1 1>&2` | yes | yes | must not wait for EOF | posix_spawn |

The swap is an established idiom. Concourse's `git-resource/assets/in` starts with `exec 3>&1 # make stdout available as fd 3 for the result` / `exec 1>&2 # redirect all output to stderr for logging` (fetched: https://raw.githubusercontent.com/concourse/git-resource/master/assets/in). debconf's `confmodule` (https://salsa.debian.org/pkg-debconf/debconf/-/raw/master/confmodule) and direnv's `stdlib.sh` (https://raw.githubusercontent.com/direnv/direnv/master/stdlib.sh) do the same. Nix uses a declared `NIX_LOG_FD`, and bats uses fd 3 with the documented pitfall that long-running children must close it (https://bats-core.readthedocs.io/en/stable/writing-tests.html).

GitHub's history shows why results must not be parsed out of stdout. `set-env`/`add-path` were deprecated after CVE-2020-15228, and `::set-output`/`::save-state` in Oct 2022, because logged untrusted data could forge commands (https://github.blog/changelog/2022-10-11-github-actions-deprecating-save-state-and-set-output-commands/). Their file replacement then had a fixed-delimiter injection, CVE-2022-35954 (https://github.com/actions/toolkit/security/advisories/GHSA-7r3h-m5j6-3q42). Text formats with delimiters need random delimiters or must reject newlines. NUL framing avoids the problem by construction, because a bash value cannot contain NUL.

### 3.2 Measured behavior

- **Inheritance** (`02-fd3/inherit.sh`): fd 3 was writable from the main shell, `( … )`, `$( … )` (the write bypasses capture), a pipeline element, a background job, a child `/bin/sh` and a child `perl`. Child programs' stdout went to the log stream. A child that closes fd 3 cannot write results.
- **EOF hazard** (`02-fd3/eof_hang.py`). The script writes its result, then runs `sleep 3 &` and exits:
  ```
  1. read fd3 to EOF:             3.01s  result=b'R\x00done\x00'
  2. communicate() stdout/stderr: 3.01s  stdout=b'log\n'
  3. wait exit, drain non-block:  0.00s  result=b'R\x00done\x00'
  4. result file after exit:      0.00s  result=b'R\x00done\x00'
  ```
  This is the long-standing bug class reported as Python bpo-13422 and gh-82388, and handled by Go's `os/exec` `WaitDelay` (https://pkg.go.dev/os/exec#Cmd). Rust's `Command::output()` has the same behavior. **Rule: a call is complete when the leader process exits. The host then drains whatever is buffered without waiting for EOF and stops reading.** The tokio prototype does this (§12).
- **Atomicity.** Frames are written by `printf` to a pipe. POSIX guarantees writes of at most PIPE_BUF bytes are not interleaved (https://pubs.opengroup.org/onlinepubs/9799919799/functions/write.html). PIPE_BUF is **512 on macOS** and 4096 on Linux (https://man7.org/linux/man-pages/man7/pipe.7.html). Frames from *concurrent* writers (background jobs) can therefore interleave and corrupt the stream. The host detects that as a contract error. Documented rule: only one process at a time writes frames.

### 3.3 Recommended framing (v1)

The script→host channel is fd 3 (the original stdout). It carries NUL-terminated fields, the first being a one-letter tag:

| Frame | Fields | Written by |
|---|---|---|
| `R` value | scalar result (string, int, number, bool as text) | `rutis_return "$v"` |
| `L` n v1…vn | list result | `rutis_return_list "${a[@]}"` |
| `F` key value | one field of a record result | `rutis_field kib "$k"` |
| `J` json | raw JSON result (validated by the host) | `rutis_return_json "$json"` |
| `E` name message | typed error; authoritative even if exit status is 0 | `rutis_throw NotFound "…"` (exits non-zero) |
| `V` name n a1…an | event emission (notification) | `rutis_emit progress 1 3` |
| `C` service method n a1…an | call into a host service | `rutis_call llm complete "$p"` |

The host→script channel is the original stdin, moved to fd 4. It carries secrets at start, then `R value` / `E message` replies to `C` frames. The script's own stdin becomes `/dev/null`, so commands such as `ssh` cannot swallow the channel. Logs are stderr plus the original stdout, merged into one stream that rutis forwards line by line and caps (for example, keep the last 4 KiB for error reports; Nagios reads only the first 4 KB, https://nagios-plugins.org/doc/guidelines.html).

**Exit semantics:**
- Success means the leader exited 0, no `E` frame was seen, and the declared type was satisfied: exactly one result for non-void methods, none for `void`. That is Bazel's "answered exactly once".
- `E` frame → `Error::Remote { name, message, graph: {status, stderr_tail} }`.
- Non-zero exit without `E` → `Error::Remote { name: "ExitStatus", … }`. 126 and 127 mean not executable or not found, and 128+n means a signal according to the shell (POSIX §2.8.2, https://pubs.opengroup.org/onlinepubs/9799919799/utilities/V3_chap02.html). Death by an actual signal is reported as such.
- Missing, duplicate or mistyped results, undeclared events and undeclared host calls → `Error::Value` (contract violation).
- Cancellation returns nothing (the future was dropped). The outcome is unknown and the call may have had side effects, as with the existing Node mounts.
- Strict typing is the safety net against bash's silent failures. In the prototype, `kib=$(du -sk "$1" | cut -f1)` on a missing path "succeeded" without `pipefail`, and the host rejected the result as `field kib is not an int: ""` instead of returning garbage.

---

## 4. JSON in shell; bash, zsh and POSIX sh versions (Q4)

### 4.1 jq vs a pure-bash helper vs host-side typing

- **jq availability.** `/usr/bin/jq` ships with macOS 15 and later (Apple DTS confirmation: https://developer.apple.com/forums/thread/765803; this Mac has jq-1.7.1-apple). It is **not** in `debian:bookworm-slim`/`trixie-slim`, `ubuntu:24.04`, `alpine`, `amazonlinux:2023` or the `bash:5.2` image (docker-library repo-info manifests, e.g. https://raw.githubusercontent.com/docker-library/repo-info/master/repos/debian/local/bookworm-slim.md; verified locally for bookworm-slim and bash:5.2). Each jq invocation is also another process: 1.8 ms on macOS.
- **Pure-bash JSON.**
  - *Encoding* is feasible: escape `\`, `"` and U+0000–U+001F (RFC 8259 §7). The fastest library, h4l/json.bash, needs bash 4.4+ (https://github.com/h4l/json.bash).
  - *Decoding* is slow and fragile. JSON.sh tokenizes with grep/awk, and its issue #61 reports minutes for 80–100 KB documents (https://github.com/dominictarr/JSON.sh).
  - Invalid UTF-8 and NUL remain unrepresentable.
- **Recommendation: neither.** The contract carries *strings*: argv in, NUL-framed fields out. **The Rust side owns all typing**: it formats ints and bools into argv and parses and validates result fields according to the declared types (§6.3). Authors write ordinary bash: `rutis_field kib "$kib"`.
  - Terraform's `external` source proves that a flat map of strings is enough for most shell results (https://registry.terraform.io/providers/hashicorp/external/latest/docs/data-sources/external).
  - CNI and Concourse show that requiring JSON pushes authors to jq.
  - Raw JSON stays available as an explicit `json` type (`J` frame, validated by the host) for authors who already use jq.

### 4.2 bash 3.2 vs 4.4 or 5.x (measured, strict probe `08-features/probe.sh`)

| Feature | bash 3.2.57 (macOS) | bash 5.2 | introduced (NEWS) |
|---|---|---|---|
| `printf -v`, `read -d ''`, `[[ =~ ]]`, `set -o pipefail`, `printf %q`, `+=`, `$'…'` | **ok** | ok | ≤3.2 |
| `declare -A`, `mapfile`, `coproc`, `BASHPID`, `${v,,}`, fractional `read -t` | missing | ok | 4.0 |
| `{fd}>` | missing | ok | 4.1 |
| `declare -g`, negative subscripts | missing | ok | 4.2 |
| `local -n`, `wait -n` | missing | ok | 4.3 |
| `${v@Q}` | missing | ok | 4.4 |
| empty `"${a[@]}"` under `set -u` | **error** ("unbound variable") | ok | fixed in 4.4 |
| `EPOCHREALTIME` | missing | ok | 5.0 |
| `wait -p` | missing | ok | 5.1 |

Source for versions: bash NEWS, https://tiswww.case.edu/php/chet/bash/NEWS. The latest release is bash 5.3, announced 5 Jul 2025 (https://lists.gnu.org/archive/html/info-gnu/2025-07/msg00001.html).

Deployed versions:
- macOS: 3.2.57. Apple stays on GPLv2 bash, and zsh has been the default login shell since Catalina (https://support.apple.com/en-us/102360; GPLv3 reason: https://scriptingosx.com/2019/06/moving-to-zsh/).
- Debian bookworm 5.2.15; trixie 5.2.37.
- Ubuntu 24.04 5.2.21.
- RHEL 9 5.1.8.
- AL2023 5.2.15.
- Alpine: no bash by default (busybox ash).

**Minimum version:**
- The **runtime (shim and helpers) must use only the 3.2 subset**. It needs only `printf`, `read -d ''`, `printf -v`, `case`, `$((…))` and `${1+"$@"}`, and the prototype runs unchanged on 3.2 and 5.2.
- **Plugins** declare their own minimum (`# @meta rutis.bash 4.4`). The host checks `BASH_VERSINFO` at mount and fails with a clear message, suggesting a configured interpreter path such as Homebrew's `/opt/homebrew/bin/bash`.
- CI runs the acceptance suite on macOS `/bin/bash` 3.2 and Linux bash 5.x.
- Do not adopt bashly's approach of refusing bash older than 4.2 (https://bashly.dev/). Stock macOS matters for a developer audience.

### 4.3 zsh and POSIX sh

Measured in `08-features`:

| | bash | zsh 5.9 | dash (POSIX) |
|---|---|---|---|
| `printf 'R\0%s\0'` frames (output side) | ok | ok | ok |
| `read -r -d ''` (needed for host replies and secrets, and for a session loop) | ok | ok | **"Illegal option -d"** |
| unquoted `$a` with `a='x y'` | 2 words | **1 word** (no SH_WORD_SPLIT) | 2 words |
| arrays | 0-based | **1-based** | none |
| startup files for `-c` | none, except `BASH_ENV` | **`~/.zshenv` sourced** (`-f` avoids it; `/etc/zshenv` is always read) | `ENV` only when interactive |
| EXIT trap on fatal TERM | **runs** | not run | not run |
| spawn cost (macOS) | 1.37 ms | 2.9 ms | 1.03 ms |

Sources: zsh startup files https://zsh.sourceforge.io/Doc/Release/Files.html; word splitting https://zsh.sourceforge.io/Doc/Release/Expansion.html; `emulate` https://zsh.sourceforge.io/Doc/Release/Shell-Builtin-Commands.html. `emulate sh` restored 2-word splitting in the test.

- **zsh.** It is the macOS *interactive* default, but system-automation scripts are written with `#!/bin/bash` or `#!/bin/sh` shebangs, and zsh is rarely installed on Linux. A zsh dialect would need its own shim (`zsh -f`), its own helper library (1-based arrays, `read` differences) and its own acceptance runs. **Not in v1.**
- **POSIX sh** (dash on Debian/Ubuntu, busybox ash on Alpine). It would allow minimal images without bash. Results and events work (printf NUL), but without `read -d ''` there is no NUL-framed host→script channel, so no `rutis_call` and no secrets unless a line-safe encoding is added. Possible as a later "sh" dialect limited to values, declared per plugin (`@meta rutis.shell sh`) and checked at build time. **Not in v1.**

---

## 5. Process lifecycle and cancellation (Q5)

### 5.1 Process groups: what a group kill reaches (measured, both OSes)

`03-kill/tree.sh` runs as the plugin call in its own group (`process_group(0)`) and starts:

| child | same pgid? | survives `killpg(TERM)` | survives `killpg(KILL)` |
|---|---|---|---|
| `sleep 1001 &` | yes | no | – |
| `nohup sleep 1002 &` (nohup ignores only HUP) | yes | no | – |
| `( sleep 1003 & )` double fork, reparented to init | yes | no | – |
| TERM-ignoring child (`trap '' TERM`) | yes | **yes** | no |
| `set -m; sleep 1005 &` (job control gives the job its own group) | **no** | **yes** | **yes** |
| `setsid sleep 1004` (new session; macOS has no `setsid` command, so perl `POSIX::setsid` was used) | **no** | **yes** | **yes** |
| foreground `sleep 1000` | yes | no | – |

Escaping takes only `setsid`/`setpgid`, and any process may change its own group within its session (setpgid(2): https://man7.org/linux/man-pages/man2/setpgid.2.html). SysV daemons double-fork and call `setsid` (daemon(7)).

**Accidental demonstration:** the escapees kept the experiment's own `| tee` pipe open, so that shell command hung (§3.2).

### 5.2 Linux containment (measured, `03-kill/linux_contain.py`, Docker with `--init`)

```
[plain]      after killpg(KILL): sleep 1005 (pgid=14, ppid=1), sleep 1004 (sid=16, ppid=1) survive
[subreaper]  after killpg(KILL): both reparented to the supervisor (ppid=7) -> found & killed -> final: (none)
[cgroup]     after killpg(KILL): cgroup.procs still has: ['16', '18'] -> echo 1 > cgroup.kill -> populated 0 -> final: (none)
```

- `PR_SET_CHILD_SUBREAPER` (Linux 3.4) reparents orphans to the nearest subreaper, which must reap them (https://man7.org/linux/man-pages/man2/PR_SET_CHILD_SUBREAPER.2const.html). In a tokio app this is intrusive. The *whole app* would adopt every orphan of every child, and tokio only reaps its own children; `waitpid(-1)` would steal tokio's statuses. It belongs in a **separate per-mount supervisor process**, not in the app.
- cgroup v2 `cgroup.kill` (Linux 5.14) SIGKILLs every process in the cgroup tree, safely against concurrent forks (https://docs.kernel.org/admin-guide/cgroup-v2.html; https://kernelnewbies.org/Linux_5.14). **Pitfall (measured):** my first attempt moved bash into the cgroup *after* `Popen` returned. Children it had already forked stayed outside, and `cgroup.kill` did not reach them. The process must join the cgroup **before exec** (a wrapper `echo $$ > cgroup.procs; exec bash …`, or `clone3(CLONE_INTO_CGROUP)`).
- An unprivileged process needs a delegated cgroup, for example `systemd-run --user --scope` or `Delegate=` (https://systemd.io/CGROUP_DELEGATION/, https://man7.org/linux/man-pages/man1/systemd-run.1.html). Containers and CI often lack a user systemd. So this is an **optional Linux enhancement**, not a baseline.
- `PR_SET_PDEATHSIG` is tied to the **thread** that forked the child, not the process (https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html). A child spawned from tokio's blocking pool got SIGKILL when that idle thread exited after 10 s (https://www.recall.ai/blog/pdeathsig-is-almost-never-what-you-want). It also reaches only the direct child (bash), not its children, and requires `pre_exec`, which forces the fork path (§5.6). **Do not use it.**
- `pidfd_send_signal` (5.1) with `PIDFD_SIGNAL_PROCESS_GROUP` (6.9) can signal a group race-free (https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html). Nice to have; not needed given §5.5.

### 5.3 macOS

- There is no `prctl`, no PDEATHSIG and no subreaper.
- `kqueue` `EVFILT_PROC`/`NOTE_EXIT` can watch a known pid, but `NOTE_TRACK` (follow forks) has been unsupported since 10.5 (https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/sys/event.h).
- `proc_listpgrppids`/`proc_listchildpids` exist but are marked private (https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/libsyscall/wrappers/libproc/libproc.h). A `setsid` escapee reparented to launchd has **no remaining link** to the call.
- **On macOS, the process group is the containment boundary, and escapees are untracked.** The honest position.

### 5.4 Bash trap behavior (measured, identical on bash 3.2, bash 5.2, zsh 5.9 and dash; `04-trap/traps.py`)

```
T1 trap TERM; foreground sleep 4; TERM->shell only     shell ended after 3.5s rc=143 (trap ran only after sleep finished)
T2 trap TERM; sleep & wait;      TERM->shell only     shell ended after 0.0s rc=143  sleep-survivor=yes (orphaned!)
T3 trap TERM; foreground sleep;   TERM->process group  shell ended after 0.0s rc=143  sleep-survivor=no
T4 no trap;   foreground sleep;   TERM->shell only     shell ended after 0.0s rc=-15  sleep-survivor=yes (orphaned!)
T5 EXIT trap only; TERM->group                         bash: "EXIT trap ran"; zsh, dash: EXIT trap NOT run
T6 no trap; foreground sleep; INT->shell only          bash 3.2/5.2: waited 3.5s then CONTINUED ("after sleep", rc=0);
                                                       zsh, dash: died after the child (rc=-2)
```

Bash manual: if bash is waiting for a command to complete and a trapped signal arrives, "it will not execute the trap until the command completes"; under `wait`, `wait` returns at once with status >128 and the trap then runs (fetched: https://www.gnu.org/software/bash/manual/html_node/Signals.html; the same text is in https://tiswww.case.edu/php/chet/bash/bashref.html). Without job control, background commands ignore SIGINT/SIGQUIT, and on SIGINT bash waits for the foreground child ("wait and cooperative exit"; https://www.cons.org/cracauer/sigint.html).

Consequences:
- **Cancel with SIGTERM sent to the process group, never to bash alone.** Signalling only bash either defers the trap until the child finishes (T1, which can be unbounded, for example a hung `curl`) or kills bash and orphans the child (T4).
- **Never use SIGINT for cancellation.** Background jobs ignore it, and bash may simply continue (T6).
- With group signalling, ordinary cleanup traps run promptly (T3). Authors do not need the `cmd & wait $!` idiom. If they use it, the trap must kill `$!` itself (T2).
- Portable cleanup code should trap `TERM` explicitly; only bash runs `EXIT` traps on fatal signals (T5).
- POSIX: signals ignored at shell startup cannot be trapped (https://pubs.opengroup.org/onlinepubs/9799919799/utilities/V3_chap02.html). The host must spawn bash with default dispositions. Rust's std resets only SIGPIPE to default (and clears the signal mask) in children. Any other signal the host process ignores, such as SIGINT or SIGTERM, is inherited as ignored. A rutis app that ignores TERM would therefore make its shell plugins uncancellable except by SIGKILL. Check this at mount.

### 5.5 Host death: lifeline pipe (measured)

If the rutis process dies (SIGKILL, panic=abort, OOM), nothing on macOS signals its children. Design: the host owns one **anonymous pipe per mount**. The write end is CLOEXEC and held only by the host. The read end is deliberately inheritable, and its fd number is passed as `RUTIS_LIFELINE_FD`. Each call's shim forks a watchdog that blocks on `read` from that fd. EOF means the host is gone, and the watchdog runs `kill -TERM 0; sleep 2; kill -KILL 0`, which reaches its own group, i.e. the call's group.

- `03-kill/run_crash.sh`: the host was SIGKILLed while a call ran bash, the watchdog and three `sleep`s. **0.5 s later: `count=0`** processes in the call's group.
- Cost: one fork per call. `bash -c :` 1.02 ms vs 1.34 ms with watchdog fork+kill (macOS) (`03-kill/macos-watchdog-cost.txt`).
- **Do not use a named FIFO** (`03-kill/fifo_readers*.py`). On macOS, with N blocked readers on one FIFO and the last writer closing, **17/30 and 35/40** bash readers never saw EOF (15/30 and 27/40 with raw `os.read`). Linux: 0/30 and 0/40. One anonymous pipe shared by 8 readers: 0/80 stuck on both OSes. A related macOS FIFO EOF quirk, missing kqueue EOF for the last writer, is golang/go#24164 (https://github.com/golang/go/issues/24164). My first FIFO-based lifeline cleaned up only 1 of 3 concurrent calls in one run and 2 of 3 in another.
- The inheritable read end leaks into other processes the app spawns. That is harmless: only write ends keep a pipe alive.
- Alternative for Linux, if escapee reaping becomes a requirement: a per-mount supervisor process (subreaper, optional cgroup) that the host talks to over a socket, as with the Node runner. It detects host death through socket EOF on its single connection (reliable) and kills everything. This costs a shipped binary and an extra hop. Not recommended for v1.

### 5.6 Spawning from Rust/tokio

- `CommandExt::process_group(0)` (std 1.64; tokio 1.22) maps to `POSIX_SPAWN_SETPGROUP` and **stays on `posix_spawn`** (Rust std source: https://github.com/rust-lang/rust/blob/master/library/std/src/sys/process/unix/unix.rs; docs: https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html). Measured on macOS: 0.81 ms with `process_group(0)` vs 0.82 ms plain.
- **Any `pre_exec` forces fork+exec.**

  | Host RSS | plain | process_group(0) | pre_exec (fork path) |
  |---|---|---|---|
  | Linux, 0 | 0.18 | 0.18 | 0.20 |
  | Linux, 1 GiB | 0.17 | 0.18 | 0.35 |
  | **Linux, 4 GiB** | 0.17 | 0.17 | **17.5** |
  | macOS, 4 GiB | 0.98 | 1.02 | 1.53 |

  (ms, medians; `01-startup/*variants*`. Linux copies page tables on fork; Mach copies map entries.) A real fd 3, a pdeathsig, or joining a cgroup in `pre_exec` all pay this. The stdio swap (§3.3), the inheritable lifeline and the env-based configuration avoid it.
- **tokio `kill_on_drop` sends SIGKILL to the direct child only.** It never touches the group, and reaping is best-effort (https://docs.rs/tokio/latest/tokio/process/struct.Command.html). For bash that kills the shell and orphans its work. `process-wrap`'s Unix `KillOnDrop` has the same limitation; only its Windows JobObject kills a tree (https://github.com/watchexec/process-wrap). The shell mount needs its own guard: on drop, `kill(-pgid, SIGTERM)`, then SIGKILL after a grace period, run off the dropping task. The prototype's `GroupGuard` does this.
- **PID reuse.** POSIX forbids reusing a pgid while the group exists (XBD 4.17: https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/V1_chap04.html), so `killpg` is safe while any member lives. To close the last tiny window (group empties and a new leader gets the same number), observe the leader's exit with `waitid(WNOWAIT)` and do group cleanup before letting tokio reap it. `rutis-interop`'s `peek_exit` already uses this pattern.

### 5.7 What "cancel" can honestly guarantee

| Event | Guarantee (Linux and macOS, v1) | Not guaranteed |
|---|---|---|
| Call future dropped (timeout) | SIGTERM to the call's group immediately; SIGKILL after the grace period (default proposal: 5 s, configurable per mount). The script's TERM traps run promptly (T3). No result is reported; the outcome is unknown. | Effects already caused (files written, requests sent); processes that escaped with `setsid`/`set -m`/daemonizing |
| Call completes normally | The call's group gets SIGTERM; anything left over is killed after the grace period ("a call owns what it starts") | escapees |
| Plugin dispose | `dispose` hook runs; then the activation group (started by `apply`) is TERMed, then KILLed | escapees |
| rutis process dies | Lifeline EOF; every in-flight call's watchdog kills its group within milliseconds | escapees; work between host death and watchdog wake-up |
| Linux with optional cgroup per mount | `cgroup.kill` also removes escapees | needs a delegated cgroup v2 |

This matches the old roadmap's honesty requirement. It refutes its implied promise that every managed process is confirmed reaped before completion is acknowledged: escapees cannot be confirmed on macOS at all.

---

## 6. Interface declaration and build-time codegen (Q6)

### 6.1 Options evaluated

| Format | Lives in the script | Typed values | Machine-readable export | Usable from `build.rs` | Notes |
|---|---|---|---|---|---|
| **argc** (`# @describe/@cmd/@alias/@arg/@option/@flag/@env/@meta`) | yes | enum (choices), list (`*`/`+`), bool (flags), notation hints (`<INT>`); strings otherwise | `argc --argc-export` (JSON), **`argc::export(source, name)` in the Rust crate** | yes: crate `argc` 1.24.0, MIT/Apache, feature `export` | scripts stay runnable CLIs (`eval "$(argc --argc-eval "$0" "$@")"`); **unknown `@tags` are hard errors**; arbitrary `@meta key value` is allowed and exported per command |
| Raycast `# @raycast.*` | yes | text/password/dropdown; ≤3 args | no | DIY parser | namespacing precedent; no return types |
| bashly YAML | no (separate YAML, generates the script) | enum, list, flags; int only as a validation | no (YAML is the source) | parse YAML | generated scripts require bash ≥ 4.2 |
| docopt usage text | yes (as the help text) | bool, counts, lists, strings | no | port needed | ambiguous grammar; no return types |
| separate manifest (TOML/JSON) | no | anything | trivially | trivially | drifts from the script; nobody can run the script from the CLI with it |

Sources: argc spec https://github.com/sigoden/argc/blob/main/docs/specification.md; `argc::export` https://docs.rs/argc/latest/argc/fn.export.html; Raycast https://github.com/raycast/script-commands; bashly https://bashly.dev/; docopt http://docopt.org/.

**Verified locally with argc 1.24.0** (`10-argc/`):
- A `# @rutis.returns string` line fails with `@rutis.returns(line 3) is unknown tag`.
- A `# @meta rutis.returns record{…}` line **inside a `@cmd` block** is exported on *that subcommand*: `subcommand: usage | metadata: {'rutis.returns': 'record{used:int,total:int,mount:string}'}`. The root keeps `{'rutis.service': 'disk'}`.
- The export carries per parameter: `id`, `notation` (e.g. `INT`, `PATH`), `required`, `multiple`/`multiple_occurs`, `default`, `choice` (`{"type":"Values","data":["k","m","g"]}`), flags, short/long names, and `envs` with `required`.
- `argc --argc-eval` costs about 2.5 ms per call (an extra process). **rutis should parse at build time only and bind arguments itself at runtime.**

**Recommendation:**
- Use **argc syntax as the declaration language**, parsed in `build.rs` through the argc crate (`default-features = false, features = ["export"]`), so semantics match the argc CLI exactly.
- Put **all rutis extensions under `# @meta rutis.<key> <value>`**, which argc accepts.
- rutis validates its own keys strictly: an unknown `rutis.*` key is a build error.
- Bonus: an existing argc script stays usable as a CLI, and an argc CLI can become a rutis plugin with only annotations added. Dual use helps the old roadmap's goal of covering existing scripts.

### 6.2 rutis `@meta` keys (proposal)

| Key | Where | Meaning |
|---|---|---|
| `rutis.service <name>` | root | service name (default: script file stem) |
| `rutis.bash <major.minor>` | root | minimum bash; checked at mount |
| `require-tools a,b` (argc's own key) | root or cmd | `command -v` checked at mount with the configured PATH |
| `rutis.returns <type>` | cmd | `void` (default), `string`, `int`, `number`, `bool`, `path`, `string[]`, `record{f:type,…}` (scalar fields), `json` |
| `rutis.secret <param>` | cmd | param delivered over the private channel, never argv/env/logs |
| `rutis.inject <svc>.<method>(<p>:<type>,…)-><type>` | cmd | host service this method may call through `rutis_call`; value types only |
| `rutis.emits <event>(<f>:<type>,…)` | cmd/root | event the plugin may emit (notification) |
| `rutis.on <event> <function>` | root | handler run as a call for each rutis event (notification only) |
| `rutis.apply <function>` / `rutis.dispose <function>` | root | lifecycle hooks |
| `rutis.concurrency <n or unlimited>` | root | parallel calls per activation (default proposal: 1, safe for scripts written as CLIs) |
| `rutis.mode session` | root | reserved for a future persistent mode (§1.3) |

### 6.3 Type mapping

| Declaration | Shell side | Rust (generated) |
|---|---|---|
| `@arg name!` / `<STRING>` | `$1`, `$argc_name` | `&str` |
| `<PATH>`/`<FILE>`/`<DIR>` notation | same | `&Path` (bytes via `OsStr`, so non-UTF-8 paths survive) |
| `<INT>` / `<NUM>` notation | decimal text | `i64` / `f64` (host formats; finite check) |
| `@flag -H --human` | `argc_human=1` or unset | `bool` |
| `@option --unit[=k|m|g]` | `argc_unit=k` | generated `enum Unit { K, M, G }` |
| `@arg files*` / `@option --x*` | `"$@"` / `argc_x=( … )` | `&[&str]` / `Vec<String>` |
| optional arg/option with default | default applied by generated code | `Option<T>` in a `<Method>Options` struct with `Default` |
| `@env NAME[!][=default] <T>` at root | env var | field of `Config` (`Option` unless `!`) |
| `rutis.returns string/int/number/bool/path` | `rutis_return "$v"` | `String` / `i64` / `f64` / `bool` / `PathBuf` |
| `rutis.returns string[]` | `rutis_return_list "${a[@]}"` | `Vec<String>` |
| `rutis.returns record{…}` | `rutis_field k v` per field | generated struct; per-field parse |
| `rutis.returns json` | `rutis_return_json "$(jq …)"` | `serde_json::Value` |
| dynamic choices/defaults (`` [`fn`] ``, `` =`fn` ``) | evaluated by the script | `String` (no enum); documented |

### 6.4 Example: annotated plugin, and what gets generated

```bash
#!/usr/bin/env bash
# @describe Disk utilities for the host
# @meta rutis.service disk
# @meta require-tools du
# @env DISK_ROOT=/ <PATH>        Root that relative paths resolve against
# @meta rutis.apply check_root
# @meta rutis.dispose cleanup

# @cmd Report usage of a path
# @meta rutis.returns record{path:string,kib:int,human:string}
# @arg path! <PATH>              Path to inspect
# @option --depth=1 <INT>        Max depth
# @option --unit[=k|m|g]         Unit for the human field
# @option --exclude* <GLOB>      Patterns to skip
# @flag -H --human               Also compute a human-readable size
usage() {
  local kib
  kib=$(du -sk "$argc_path") || rutis_throw NotFound "cannot read $argc_path"
  rutis_field path "$argc_path"
  rutis_field kib "${kib%%[[:space:]]*}"
  rutis_field human "$( [ -n "${argc_human:-}" ] && du -sh "$argc_path" | cut -f1 )"
}

# @cmd Summarize a directory with the host's LLM
# @meta rutis.inject llm.complete(prompt:string)->string
# @meta rutis.emits disk/progress(done:int,total:int)
# @meta rutis.returns string
# @arg dir! <DIR>
summarize() {
  rutis_emit disk/progress 0 1
  rutis_call llm complete "Summarize this listing: $(ls -1 "$argc_dir" | head -50)" || return
  rutis_emit disk/progress 1 1
  rutis_return "$RUTIS_REPLY"
}

check_root() { [ -d "$DISK_ROOT" ] || rutis_throw ConfigError "DISK_ROOT $DISK_ROOT is not a directory"; }
cleanup()    { rm -rf "${RUTIS_STATE_DIR:?}/cache"; }

# Still a normal argc CLI; inside rutis the shim defines `argc` as a no-op.
eval "$(argc --argc-eval "$0" "$@")"
```

Generated (sketch; same shape as the Cordis mounts' generated modules):

```rust
pub mod disk {
    #[derive(Debug, Clone, Default)]
    pub struct Config { pub disk_root: Option<std::path::PathBuf> }      // @env DISK_ROOT=/

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum UsageUnit { K, M, G }
    #[derive(Debug, Clone, Default)]
    pub struct UsageOptions { pub depth: Option<i64>, pub unit: Option<UsageUnit>, pub exclude: Vec<String>, pub human: bool }
    #[derive(Debug, Clone, serde::Deserialize)]
    pub struct Usage { pub path: String, pub kib: i64, pub human: String }

    /// rutis service `disk`, provided by plugins/disk.sh (interface hash checked at mount).
    #[derive(Clone)]
    pub struct Disk { /* Arc<ShellMount> */ }
    impl Disk {
        /// Report usage of a path. Cancellable: dropping the future TERMs the call's process group.
        pub async fn usage(&self, path: &std::path::Path, options: UsageOptions) -> Result<Usage, rutis_interop::Error>;
        /// Summarize a directory with the host's LLM.
        pub async fn summarize(&self, dir: &std::path::Path) -> Result<String, rutis_interop::Error>;
    }

    /// Host service the plugin injects (from `rutis.inject`); the mount waits for it natively.
    pub trait LlmHost: Send + Sync + 'static {
        fn complete(&self, prompt: String) -> rutis::BoxFuture<'static, Result<String, rutis_interop::Error>>;
    }
    pub fn provide_llm(ctx: &rutis::Ctx, host: impl LlmHost) -> Result<rutis::Disposer, rutis::CordisError>;

    /// `disk/progress` (notification; `rutis::Event` with `Value = ()`).
    pub struct DiskProgress { pub done: i64, pub total: i64 }

    pub struct Plugin { /* config, injects: [dyn LlmHost] */ }
    impl Plugin { pub fn new(config: Config) -> Self; }
    // impl rutis::Plugin: apply = mount checks + `check_root` + provide `Disk`; effects dispose in LIFO order.
}
```

The prototype's minimal parser printed real output from `09-proto/plugin/sysinfo.sh` (§12): `pub async fn disk_usage(&self, path: &str) -> Result<SysinfoDiskUsage, rutis_interop::Error>;`, and so on.

### 6.5 Build-time and mount-time checks (reject instead of degrade)

- **Build (`build.rs`):**
  - argc parse errors.
  - Unknown `rutis.*` keys.
  - Unsupported return or parameter types.
  - `@cmd` without a function.
  - Invalid identifiers.
  - Events with return values; an event both emitted and listened to.
  - `rutis.inject` signatures that are not value-only.
  - `bash -n` syntax check.
  - Optional `shellcheck` if installed, reported as warnings (https://github.com/koalaman/shellcheck/wiki/Directive).
  - The generated module embeds an **interface hash** and the script path; the path is relocatable like `RUTIS_INTEROP_ROOT`.
- **Mount (`apply` of the generated plugin):**
  - Interpreter exists and `BASH_VERSINFO` ≥ declared.
  - `require-tools` present on the configured PATH.
  - Script readable and its interface hash equals the build-time hash.
  - One probe call (`shim --check`) sources the plugin and checks `declare -F` for every declared function and hook, so the bindings cannot silently diverge from the deployed script. This costs one process, about 3 ms.
  - Then the `apply` hook runs. Any failure means the mount fails and no service is registered, the same rule as the Cordis mounts.

---

## 7. Precedents (Q7)

Each entry gives the contract, then the lessons for rutis.

1. **CNI plugins** (https://github.com/containernetworking/cni/blob/main/SPEC.md)
   - *Contract:* the runtime runs the binary with `CNI_COMMAND` (ADD/DEL/CHECK/STATUS/GC/VERSION) and `CNI_CONTAINERID`/`NETNS`/`IFNAME`/`ARGS`/`PATH` in env, config JSON on stdin, and gets result JSON on stdout. Failure is non-zero exit plus `{cniVersion, code, msg, details}`; codes 1–99 are reserved (11 = try again later). stderr carries logs. `VERSION` lists supported versions.
   - *Lessons:* reserved error codes ("retry later"); a version handshake; per-command idempotency rules (DEL); the spec is ambiguous about stdout vs stderr for errors, so libcni falls back from stdout JSON to stderr text to "no message". Name one channel and define the fallback.
2. **Terraform `external` data source** (https://registry.terraform.io/providers/hashicorp/external/latest/docs/data-sources/external)
   - *Contract:* `program` argv with no shell; a JSON object of strings on stdin; one JSON object with **string values only** on stdout; error = non-zero exit plus stderr text; one-shot, no side effects.
   - *Lessons:* a flat string map is enough for most shell results; the docs teach `jq @sh`/`--arg`, which is the jq tax. rutis avoids it with host-side typing.
3. **Concourse resource types** (https://concourse-ci.org/docs/resource-types/implementing/)
   - *Contract:* `/opt/resource/check|in|out`, JSON (source, version, params) on stdin, JSON on stdout, logs on stderr; `in` and `out` get a directory argument.
   - *Lessons:* the official git-resource scripts use `exec 3>&1; exec 1>&2`, which is exactly the swap recommended here; large data goes through directories; secrets arrive on stdin, not argv.
4. **Docker credential helpers** (https://github.com/docker/docker-credential-helpers)
   - *Contract:* `docker-credential-<name> get|store|erase|list|version`; input is a raw string or JSON depending on the verb; errors are printed to **stdout** plus exit 1.
   - *Lessons:* "not found" is detected by comparing a magic string, so use structured error names; keep input formats uniform.
5. **Git credential helpers** (https://git-scm.com/docs/gitcredentials, https://git-scm.com/docs/git-credential)
   - *Contract:* `key=value` lines ended by a blank line; values cannot contain newline or NUL; capability announcements; unknown keys are ignored; `!` snippets are run by a shell.
   - *Lessons:* line formats must reject newline and NUL. rutis's NUL framing removes the problem.
6. **asdf plugins** (https://asdf-vm.com/plugins/create.html)
   - *Contract:* fixed `bin/*` scripts, inputs in env (`ASDF_INSTALL_VERSION`/`PATH`…), space-separated output on stdout, exit status.
   - *Lessons:* ad-hoc formats per script break on spaces; the host should own output directories and their cleanup.
7. **kubectl and git PATH plugins** (https://kubernetes.io/docs/tasks/extend-kubectl/kubectl-plugins/, https://git-scm.com/docs/git)
   - *Contract:* found by name on PATH, argv passed through, no contract.
   - *Lessons:* zero ceremony but no types, no help and shadowing problems, which argues for explicit registration (the Cargo.toml mount).
8. **Nagios plugins** (https://nagios-plugins.org/doc/guidelines.html)
   - *Contract:* exit code is the state (0 OK, 1 WARNING, 2 CRITICAL, 3 UNKNOWN); first stdout line is `text | perfdata`; only about 4 KB is read; stderr is not captured.
   - *Lessons:* the host enforces timeouts and maps them to a state; cap output; keep a "plugin itself is broken" state separate from business errors.
9. **GitHub Actions** (https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands)
   - *Contract:* `$GITHUB_OUTPUT`/`$GITHUB_ENV`/`$GITHUB_STATE` files of `k=v` or heredoc `k<<DELIM` lines, read after the step.
   - *Lessons:* stdout workflow commands were deprecated as injectable (2020 and 2022 changelogs); a fixed heredoc delimiter was CVE-2022-35954. Never parse control data from logs, use unforgeable framing, and allowlist environment keys (GitHub blocks `NODE_OPTIONS`).
10. **Ansible modules** (https://docs.ansible.com/ansible/latest/dev_guide/developing_program_flow_modules.html)
    - *Contract:* a non-Python module containing `WANT_JSON` gets the path of a JSON args file as argv[1] (else `k=v`), and prints one JSON object (`changed`, `failed`, `msg`…) on stdout.
    - *Lessons:* an args file avoids argv limits; sniffing a marker string is worse than declared metadata; Ansible tolerates junk around the JSON with warnings, which hides bugs. Be strict.
11. **Bazel persistent workers** (https://bazel.build/remote/persistent, https://bazel.build/remote/creating)
    - *Contract:* `--persistent_worker`; `WorkRequest`/`WorkResponse` on stdin/stdout (protobuf or NDJSON), logs to stderr, `request_id` for multiplexing, `cancel` support, each request answered exactly once.
    - *Lessons:* persistent mode needs ids, exactly-once answers and cooperative cancellation backed by kill. That is why bash session mode is deferred.
12. **Nushell plugins** (https://www.nushell.sh/contributor-book/plugin_protocol_reference.html)
    - *Contract:* encoding byte (json or msgpack), `Hello{version, features}`, signatures cached at `plugin add`, `EngineCall` callbacks into the engine, streams with Ack, GC of idle plugins.
    - *Lessons:* precedent for callbacks into the host (`rutis_call`) and for caching signatures (rutis caches at build time and checks a hash at mount); streams are out of reach for bash.
13. **Raycast script commands** (https://github.com/raycast/script-commands)
    - *Contract:* `# @raycast.schemaVersion/title/mode/argument1..3` comments with JSON-typed arguments; output modes read stdout (last line, first line, or all); non-zero exit means failure.
    - *Lessons:* namespaced comment metadata works well for scripts; three arguments and "last line is the message" do not scale.
14. **Alfred script filters** (https://www.alfredapp.com/help/workflows/inputs/script-filter/json/)
    - *Contract:* query as argv (recommended) or `{query}` substitution; JSON `{"items":[…],"rerun","variables"}` on stdout; "terminate previous script" behavior.
    - *Lessons:* substitution into script text is an injection risk, so use argv; cancellation is by kill.

Shared lessons: separate results from logs by construction (fd 3, or a file); detect completion by exit; never parse control data from a log stream; reject newline and NUL in line formats or use NUL framing; declare signatures statically and check them against a hash; keep exit code and error name authoritative and use a structured error; the host enforces timeouts and output caps; default to one-shot execution.

---

## 8. Mapping to the Cordis/rutis model (Q8)

| Cordis/rutis concept | Bash plugin equivalent |
|---|---|
| plugin (one `apply`) | one script file; the generated mount plugin's `apply` checks the environment, runs the optional `rutis.apply` hook, then provides the service |
| service | one service per script: a set of `@cmd` functions; all methods are `async fn` (a call is a process, and there is no meaningful sync shape) |
| config | root-level `@env` declarations become `Config`, passed as env vars to every call and hook |
| fiber effects and LIFO cleanup | the generated plugin registers its cleanup effect before providing the service (as the Cordis mounts do). Disposal withdraws the service first (consumers' disposers still run with the service callable), then lets in-flight calls finish or be cancelled, then runs the `rutis.dispose` hook, then TERMs/KILLs the **activation group** (processes started by `apply`), then closes the lifeline |
| dependency gating (`inject`) | `rutis.inject svc.method(sig)` generates a `…Host` trait plus `provide_…`; the mount's `injects` waits for it natively; when it is withdrawn, the mount stops (as Node host services do) |
| calling an injected service | `rutis_call svc method args…` writes a `C` frame and reads the reply from fd 4; value types only; one call at a time per process (the prototype answered `summarize` through it) |
| events: emit | `rutis_emit name args…` writes a `V` frame, which becomes the generated event type, emitted fire-and-forget on the bus. Workers started by `apply` may keep emitting until dispose; the host keeps reading the apply call's frame channel for `V` frames only |
| events: listen | `rutis.on <event> <fn>` runs one call per event, in arrival order under the plugin's concurrency limit; notifications only |
| service replacement (`ctx.set`) | none; the service object is fixed for the activation |
| reentrancy | a call path id (like protocol v1's `path`) detects host→plugin→host→same-plugin cycles under `concurrency 1` and fails fast instead of deadlocking (analogue of `SyncWaitCycle`) |

**Supported** (value-only subset): scalar, enum, list and record arguments and results; JSON results; typed errors; notification events in both directions; value-only host services; cancellation.

**Rejected at build time** (never degraded):

| Feature | Why | Where rejected |
|---|---|---|
| live objects, object references in results | a process that has exited has no objects | there is no type for them in the annotation grammar |
| callbacks as parameters; returned functions | bash cannot pass function references | injected service signatures with function types are a build error |
| sync methods | every call is a process | all generated methods are `async`; documented |
| streams, binary values | not in frame protocol v1 | use a `<FILE>` path parameter; a `bytes` (base64) type is possible later |
| waterfall/bail/serial events with return values | a notification cannot answer | build error, as the Cordis mounts do |
| event both emitted and listened | forwarding loop | build error, same rule as `Bindings::event`/`emit` |
| unknown `rutis.*` meta, unknown types | silent drift | build error |

**Rejected at mount time:** interpreter or version mismatch; missing tools; interface hash mismatch; declared function missing in the deployed script.

**Calling back into the host: helper CLI vs bash function.** The question raised `rutis-call llm.complete …`. Two ways to build it:
- **(i) A binary that connects to a per-mount Unix socket.** Safe for concurrent use and usable from child programs, but it means shipping a binary. `nc -U`/`socat` are not universal: GNU netcat lacks `-U`, and macOS has no `socat`.
- **(ii) A bash function over inherited fds 3/4.** No binary and works in 3.2, but serialized per call and unsafe from concurrent background jobs.

v1 uses (ii), as prototyped. If calls from child programs or parallel jobs become necessary, add (i) or a `mkdir`-lock around (ii).

---

## 9. Windows (Q9)

**Out of scope; say so plainly.**
- `rutis-interop` is already `#[cfg(unix)]`.
- **Git Bash/MSYS2** emulates POSIX signals per process. For native `.exe` children, SIGTERM becomes an injected `ExitProcess`, SIGINT a remote `CtrlRoutine` thread, and SIGKILL `TerminateProcess` (https://raw.githubusercontent.com/msys2/msys2-runtime/msys2-3.6.4/winsup/cygwin/include/cygwin/exit_process.h; regressions such as https://github.com/git-for-windows/git/issues/1470). There are no process groups for native trees.
- The Windows way to kill a tree is a **Job Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects), which is a different implementation.
- **WSL2** is a real Linux kernel in a VM (https://learn.microsoft.com/en-us/windows/wsl/compare-versions). A rutis app running *inside* WSL is just Linux and works. A Windows rutis app driving WSL bash cannot use process groups or cgroups across the boundary.
- Recommendation: unsupported on Windows; Windows users run the rutis app under WSL2.

---

## 10. Recommended v1 contract (consolidated)

1. **Declaration.** argc annotations plus `@meta rutis.*` (§6). `[package.metadata.rutis-interop.shell.<module>] script = "plugins/disk.sh"` with an optional `interpreter` path. `build.rs` stays `rutis_interop::build::from_manifest()` and `rutis_interop::include_mounts!()` exposes the module. No npm project is involved.
2. **Spawn per call.**
   - `<bash> -p <rt>/shim.bash <plugin.sh> <fn> [--rutis-opt <name> <value>]… -- <positionals…>`.
   - `process_group(0)` and `env_clear()` plus an allowlist, config and `RUTIS_*` variables.
   - cwd = activation dir.
   - stdin = host→script pipe, stdout = frame pipe, stderr = log pipe.
   - Inheritable lifeline read end.
   - Never `pre_exec`. Never `sh -c` with data.
3. **Shim (bash 3.2 subset).**
   - Run `exec 4<&0 </dev/null 3>&1 1>&2`.
   - Fork the lifeline watchdog.
   - Read secrets from fd 4.
   - Assign `argc_*` variables without `eval` of data. Array appends use `eval` with only a validated identifier interpolated and the value referenced as `\$v`.
   - Define `argc(){ :; }` and source the runtime helpers, then the plugin. `$0` is the shim, so plugins should use `${BASH_SOURCE[0]}` or `$RUTIS_PLUGIN_DIR`.
   - Call the function with its positionals.
4. **Frames** as in §3.3. The host converts per declared type and enforces exactly one result.
5. **Completion** is the leader's exit (observed with `waitid(WNOWAIT)`), then a non-blocking drain, then TERM of the call group (leftovers), then reaping. The error mapping is in §3.3.
6. **Cancellation:** the future's drop guard TERMs the group, then KILLs it after the grace period.
7. **Concurrency:** `rutis.concurrency` per activation, default 1. Queued calls are cancellable without spawning.
8. **Lifecycle:** `apply` and `dispose` hooks; an activation group for processes started by `apply`; the lifeline for host death; optional Linux cgroup per mount later.
9. **Limits:** argv pre-check (64 KiB per string); frame size cap (for example 16 MiB per call); log tail cap; no default call timeout. As with the Cordis mounts, callers use `tokio::time::timeout`. Only the TERM→KILL grace is a duration the layer owns.
10. **Not in v1:** session mode, zsh and POSIX sh dialects, streams, bytes, Windows.

---

## 11. Review of the old roadmap's Shell section (`docs/roadmap-protocol-plugin-languages-2026-09-26.md`)

| Roadmap claim | Verdict | Evidence or refinement |
|---|---|---|
| A shared runner must not source unrelated scripts into one shell; variables, cwd, traps and exit must stay separate | **confirmed** | §1.4: function, variable and cwd leaks, `set -e` killed the shell, EXIT trap overwritten |
| Start with per-call command entry points; opt into per-activation persistent mode for state | **confirmed, sharpened** | per-call overhead 0.7 ms (Linux) / 2.6 ms (macOS) is small next to real work. The persistent mode's problems (cancellation vs state, byte-wise `read`, serialization) make it a later opt-in, and a cancelled call must restart the session |
| Arguments via argv or a JSON convention; never interpolate into `sh -c` | **confirmed; JSON input refined** | argv is injection-safe (§2.1). JSON input needs jq or a slow pure-bash parser, so prefer argv plus host-side typing. Add the Linux 128 KiB per-string limit, `<FILE>` for large data and private delivery for secrets |
| stdout/stderr are logs; structured results through a helper on a dedicated pipe/fd | **confirmed** | implement it as the `exec 3>&1 1>&2` swap (posix_spawn-compatible). Completion must be exit-based (EOF hazard) |
| Subshells, pipelines and background jobs belong to the activation; cancel and confirm exit before acknowledging | **partly refuted** | process groups capture subshells, pipelines, background jobs, nohup and double forks, but **not** `setsid`/`set -m` escapees. On macOS those cannot even be detected; on Linux only a subreaper or cgroup catches them. The guarantee must be stated per §5.7 |
| First release promises only tested Bash/Linux | **revise** | stock macOS bash 3.2 passed every experiment and the prototype. Promise bash ≥ 3.2 on Linux and macOS, with the runtime in the 3.2 subset; zsh and POSIX sh are separate dialects |
| Missing reverse calls means rejecting plugins that require host services | **revise** | value-only reverse calls work from bash (`rutis_call`, prototyped). Reject only injected methods with callbacks or objects |
| Unsupported capabilities are rejected before loading, never downgraded | **confirmed** | §6.5 and §8 tables |
| E-series acceptance (E01–E04, E07) | **keep, add** | add: EOF hazard with a backgrounded child; trap cleanup on cancel; host SIGKILL with lifeline; escapee documented; bash 3.2/5.x matrix; env-injection hygiene; argv E2BIG pre-check |

---

## 12. Prototype: tokio host plus bash 3.2 plugin (`09-proto/`)

`host/src/main.rs` (about 330 lines) is a minimal shell mount:
- an annotation parser that prints generated signatures;
- a per-call spawn with `process_group(0)`, `env_clear` plus allowlist, and stdio swap;
- a streaming NUL frame parser;
- `C`-frame host calls answered over stdin;
- `V` events delivered while the call runs;
- exit-based completion plus a drain with a 5 ms timeout;
- leftover TERM;
- a `GroupGuard` that TERMs, then KILLs, on drop;
- per-declared-type result conversion.

`plugin/` holds `rutis.bash` (helpers), `shim.bash` and `sysinfo.sh`. macOS output (`proto-run-macos.txt`):

```
call greet(["Robert'); rm -rf / #\n$(id) `whoami` ✓"])
  -> Ok(String("hello, Robert'); rm -rf / #\n$(id) `whoami` ✓"))  (6.55 ms, first call)
call disk_usage(["/usr/bin"])   -> Ok(Object {"human": "80M", "kib": 82348, "path": "/usr/bin"})  (28.28 ms; du itself)
call disk_usage(["/nonexistent"]) -> Err(Contract("field kib is not an int: \"\""))   <- strict typing caught a pipeline failure
call list([...plugin dir])      -> Ok(Array ["rutis.bash", "shim.bash", "sysinfo.sh"])  (3.71 ms)
call fail([])                   -> Err(Remote { name: "PermissionDenied", message: "cannot touch /etc/shadow", status: Some(1), stderr_tail: "about to fail" })
call summarize(["shell plugins"]) -> Ok(String("summary=[LLM<SUMMARIZE: SHELL PLUGINS>]"))  <- host service via rutis_call
call work([])   [event] progress["1","3"] … ["3","3"] -> Ok(Number(42))   <- events streamed before completion
call slow() with 300 ms timeout -> timed out after 301 ms; "[log] slow: cleaned …tmp.eY5nPpoUpr";
     temp file still exists after cancel: false; leftover 'sleep 300' processes: ""
200 sequential greet calls: 683.2 ms (3.42 ms/call)     200 concurrent greet calls: 200 ok in 64.2 ms
```

Linux bash 5.2 (`proto-run-linux.txt`): identical behavior; 0.66 ms per call sequentially; 200 concurrent in 74.5 ms.

---

## 13. Experiment log (exact commands and outputs)

Everything was run from `scratchpad/experiments/bash`. `H=harness/target/release/harness` (macOS) or `harness/target-linux/release/harness` (in Docker). Values are min/med/mean/p95 in ms.

### 13.1 Startup (`01-startup`)
```
$ N=500 $H startup /usr/bin/true            -> min 0.757 med 0.965 mean 0.990 p95 1.284
$ N=500 $H startup /bin/bash -c :           -> min 1.119 med 1.372 mean 1.424 p95 1.685
$ N=500 $H startup /bin/bash --noprofile --norc -c : -> med 1.418
$ N=500 $H startup /bin/sh -c :             -> med 2.407
$ N=500 $H startup /bin/dash -c :           -> med 1.032
$ N=500 $H startup /bin/zsh -c :            -> med 2.889      (-f: med 2.647)
$ N=500 $H startup /usr/bin/jq -n 1         -> med 1.801
$ N=500 $H startup /usr/bin/perl -e 1       -> med 2.357
$ N=500 $H startup /usr/bin/python3 -c pass -> med 20.114
Linux (docker run rust:1.98.1-bookworm): true med 0.179; bash -c : med 0.339; dash med 0.195; perl 0.460; python3 3.887
```
A rough shell-loop variant (`01-startup/loop-startup.sh`, spawning from bash 3.2) gave 1.68 ms for `true` and 1.91 ms for `bash -c :`, because bash's own fork is slower than Rust's posix_spawn.

### 13.2 Spawn variants vs host RSS (`$H variants /usr/bin/true`, `RSS_MB=…`)
```
macOS RSS 0:    plain 0.819  process_group(0) 0.806  pre_exec 1.174
macOS RSS 4096: plain 0.983  process_group(0) 1.019  pre_exec 1.527
Linux RSS 0:    plain 0.183  pg 0.180  pre_exec 0.200
Linux RSS 1024: plain 0.171  pg 0.177  pre_exec 0.347
Linux RSS 4096: plain 0.174  pg 0.165  pre_exec 17.503
```

### 13.3 Per-call vs persistent (`05-loop`, `$H percall|persistent 05-loop <method> <arg> [swap|fd3|file]`)
```
macOS: (a) swap echo med 2.630 | fd3 3.086 | file 3.127 | noisy 5.287 | uname 3.865
       (b) echo med 0.046 | noisy 2.632 | uname 1.320 | incr 0.047 (last result "510")
       big plugin (7024 lines, 161645 bytes): (a) 8.833  (b) 0.056
       bigarg: (a) 1KB 2.806 10KB 2.732 100KB 4.128 1MB 20.146 | (b) 1KB 0.231 10KB 1.949 100KB 23.356 1MB 296.229
Linux: (a) swap echo 0.693 | fd3 0.721 | noisy 1.200 | uname 0.923 ; (b) echo 0.046 | noisy 0.547 | uname 0.297
       big plugin: (a) 6.394 (b) 0.045 ; bigarg (a) 10KB 0.778 100KB 1.743 | (b) 10KB 1.528 100KB 18.098
```
The persistent loop is 3.2-compatible: `printf 'echo\0001\000hello world\0count\0003\000a\000b c\000d\0incr\0000\0incr\0000\0noisy\0001\000x\0' | bash loop.sh plugin.sh` gave `R|hello world|D|0|R|3|D|0|R|1|D|0|R|2|D|0|R|ok:x|D|0|`, with the noise on stderr.

### 13.4 fd 3 and EOF (`02-fd3`)
```
$ /bin/bash inherit.sh 3>result.txt >stdout.txt 2>stderr.txt
result.txt: R main | R subshell | R inside $(...) | R pipeline element (piped) | R background job | R child /bin/sh | R child perl via fd 3
stdout.txt: main log line, "$(...) captured 'captured'", child program stdout, "child with fd 3 closed could not write (expected)"
$ python3 eof_hang.py   (script: printf "R\0done\0" >&3; echo log; sleep 3 & exit 0)
1. read fd3 to EOF: 3.01s | 2. communicate(): 3.01s | 3. wait exit + non-blocking drain: 0.00s | 4. result file: 0.00s (all four got R\0done\0)
```

### 13.5 Kill, containment and lifeline (`03-kill`)
```
$ python3 host.py           (macOS; tree.sh in its own group)
after killpg(TERM): sleep 1005 (own pgid, set -m), sleep 1004 (setsid), sleep 1006 (ignores TERM)
after killpg(KILL): sleep 1005, sleep 1004             bash exit status: -15
$ docker run --rm --init … python3 linux_contain.py plain|subreaper ; --privileged --cgroupns=private … cgroup
plain: 1005/1004 survive (ppid=1) | subreaper: reparented to supervisor, killed, final (none)
cgroup: cgroup.procs ['16','18'] -> cgroup.kill -> populated 0, final (none)
  (first cgroup attempt moved bash after Popen: escapees were outside the cgroup and survived)
$ bash run_crash.sh         (per-call lifeline pipe; host SIGKILLed)
members before: bash, watchdog, sleep 1001, 1002, 1000  ->  0.5s after host SIGKILL: count=0
$ watchdog cost: bash -c : med 1.020 ; with watchdog fork+kill med 1.339
$ python3 fifo_readers.py 3 10 / 8 5 (bash readers):    darwin 17/30, 35/40 never saw EOF ; linux 0/30, 0/40
$ python3 fifo_readers_raw.py (os.read readers):          darwin 15/30, 27/40
$ python3 shared_pipe.py (one anonymous pipe, 8 readers): darwin 0/80, linux 0/80
$ python3 fifo_host.py (FIFO lifeline, 3 calls): 12 processes before, 8 still alive 0.5 s after host SIGKILL
```

### 13.6 Limits and data (`06-limits`)
```
Linux (debian:bookworm-slim + strace):
  read -d "" from a pipe, 10000-byte field: 10006 read() calls ; from a regular file: 8 read() calls
  /bin/true <131071 bytes>: ok ; <131072 bytes>: "Argument list too long", exit 126 ; env X=<131070 bytes>: same
macOS: /usr/bin/true <1,040,000 bytes>: ok ; <1,048,576 bytes>: "argument list too long"
macOS bash 3.2 data.sh: $(printf a\0b) -> 2 bytes "a b" ; hostile argv round-trips exactly ;
  ${#s} for héllo✓: 9 under C, 6 under en_US.UTF-8 ; invalid UTF-8 kept as bytes 62 61 64 ff 62 79 74 65
jq: printf 'bad\377byte' | jq -R .  ->  "bad�byte" (shown as "bad�byte")
bash 5.2: "warning: command substitution: ignored null byte in input"
Input channels (macOS, med): argv 3KB 1.398 | NUL args file 3KB 3.312 (env baseline 2.149) | file-per-arg $(cat) 3KB 8.857
                             argv 200KB 4.615 | NUL args file 200KB 67.166 | file-per-arg $(cat) 200KB 8.742
```

### 13.7 Environment (`07-env`)
```
bash 3.2 and 5.2: BASH_ENV -> "BASH_ENV file was sourced (code ran before the plugin)" ;
  BASH_FUNC_ls%% -> "ls -> [hijacked ls]" ; CDPATH -> $(cd sub && pwd) returned two lines ;
  SHELLOPTS=xtrace -> full trace on stderr ; IFS=x -> ignored (IFS bytes: space \t \n)
bash -p (bash-p.txt): 3.2: only CDPATH-leaked remains (function import, BASH_ENV, xtrace blocked) ; 5.2: clean
```

### 13.8 Features and dialects (`08-features`)
The strict probe table is in §4.2. dash prints `read: Illegal option -d`. zsh treats `a='x y'` as 1 word, and as 2 with `emulate sh`; zsh `${a[1]}` is `first`, bash's is `second`. `HOME=fakehome zsh -c 'echo body'` printed `zshenv ran body`; with `zsh -f` it printed only `body`.

### 13.9 argc (`10-argc`, argc 1.24.0 installed under `tools/`)
```
$ argc --argc-export unknown-tag.sh   ->  @rutis.returns(line 3) is unknown tag
$ argc --argc-export meta-per-cmd.sh  ->  root metadata {'rutis.service': 'disk'}; subcommand usage metadata
   {'rutis.returns': 'record{used:int,total:int,mount:string}'}; positional path notation PATH required;
   --depth notations ['INT'] default '1'; --unit choice ['k','m','g'] default 'k'; --exclude multiple_occurs;
   -H/--human flag; envs DISK_ROOT required        (full JSON: export-meta-per-cmd.json)
$ DISK_ROOT=/ argc --argc-eval meta-per-cmd.sh usage /var --depth 2 --unit g --exclude '*.log' --exclude 'a b' -H
   argc_depth=2 argc_unit=g argc_exclude=( '*.log' 'a b' ) argc_human=1 argc_path=/var … usage /var
   (without DISK_ROOT: "error: the following required environments were not provided: DISK_ROOT", exit 1)
argc --argc-eval process cost: mean ≈2.5 ms
```

### 13.10 Traps (`04-trap/traps.py`): see §5.4 (macOS bash 3.2, zsh, dash; Linux bash 5.2, dash, zsh: identical except T5 and T6).

---

## 14. Open questions for the rutis design

1. **Runner location.** In-process Rust (recommended for v1) or a per-mount supervisor process (needed only for Linux subreaper or cgroup containment of escapees)?
2. Default TERM→KILL grace (proposal: 5 s) and whether `dispose` has its own deadline. Requirements §7 leaves unload deadlines to the application.
3. Default `rutis.concurrency`: 1 (safe for CLI-style scripts) or unlimited (closer to Cordis async semantics)?
4. Use the `argc` crate as a build dependency (exact argc semantics; check MSRV and size) or a vendored subset parser?
5. String vs `OsString`/bytes for `path`-typed results; whether to add a `bytes` (base64) type.
6. Whether to offer a `returns stdout` convenience (the method's stdout *is* its result) to wrap existing "print the answer" functions unmodified. It conflicts with the stdout swap and with events and host calls in that method.
7. Whether `@meta rutis.strict` should make the shim set `set -o pipefail` (or `set -euo pipefail`) for opted-in plugins. `set -e` has well-known traps (https://mywiki.wooledge.org/BashFAQ/105); host-side strict typing already catches many silent failures.
