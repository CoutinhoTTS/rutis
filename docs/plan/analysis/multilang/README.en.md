# Multilingual Runtime Research Reports

The reports investigate the feasibility and design of adding language runtimes:

| Report | Focus |
| --- | --- |
| [runner-contract.md](runner-contract.md) | Existing runner and protocol assumptions |
| [python.md](python.md) | Python runtime and plugin conventions |
| [powershell.md](powershell.md) | PowerShell runspaces, process behavior, and cancellation |
| [bash.md](bash.md) | Process-per-call Bash execution |
| [applescript.md](applescript.md) | OSAKit state, cancellation, TCC permissions, and annotations |
| [precedents.md](precedents.md) | Lessons from about 40 multilingual plugin systems, including Neovim, Azure Functions, Pulumi, and Nushell |

Experiment scripts mentioned by the reports (`scratchpad/experiments/...`, `X/...`) were not committed. The reports retain the commands and key output.

Two findings also matter to Cordis mounting:

- `runner-contract` §12.2 lists implicit runner requirements: Rust futures are lazy, event acknowledgement must be awaited, and an incorrect `path` causes a deadlock.
- The request ID prefix is hard-coded as `node:` on the Rust side (`rpc.rs:822,1165,1194`).
