# M1.5 Minimum Corpus Matrix: 10-Repository Loading Experiment (2026-08-22)

> Acceptance criterion: [design-dsh-bridge-2026-08-21.md](design-dsh-bridge-2026-08-21.en.md), §9, M1.5: run L0/L1 against a 10-repository minimum corpus, including a sample with heavy value-level imports. Surface host npm-closure problems early, where they are cheapest to address.
> Experiment code: `../deepseek-harness/experiments/m1p5-corpus/` (untracked directory); corpus source: ecosystem server `100.121.215.57:dsh-ecosystem/` (9,398 shallow-cloned repositories).

## Results

| # | Sample (selection rationale) | L0 load | L1 registration effective | Finding |
|---|---|---|---|---|
| 1 | btspoony/mstar-harness (highest coupling, 1122.5 points; heaviest value-level imports) | ✗ | — | **npm closure**: `@mstar-harness/engine` (internal package in the community workspace) |
| 2 | btspoony/mstar-workflow (914.5 points, same family) | ✗ | — | **npm closure**: `@mstar-harness/engine` |
| 3 | jianxx/dsh-cc-plugins · cc-memory (member of a 16-package workspace) | ✗ | — | **npm closure**: `@jianxx/dsh-cc-tools` (community internal package) |
| 4 | andy8647/dsh-auto-approval (tool-oriented) | ✗ | — | **npm closure**: `zod` (third-party) |
| 5 | leeminjing/dsh-messages-sanitizer (standard zero-dependency shape) | ✓ | none | Pure event plugin, with no tool or prompt surface |
| 6 | leemiracle/deepseek-rust-harness | ✓ | **✓** | Four tool schemas registered: `r_fmt`, `r_lint`, `r_build`, `r_miri` |
| 7 | oowjzzoo/dsh-plugin-api (moderate coupling, 251 points) | ✓ | none | — |
| 8 | pwnky/dsh-session-link (lightweight) | ✓ | none | Depends on `@deepseek-ai/dsh-session-reference`, which is an official package and was covered by the closure |
| 9 | ayuanwong/deepseek-harness-ux | ✗ | — | **Shape**: a full fork of the dsh repository, not a plugin repository |
| 10 | 112gt/deepseek-harness-vscode | ✗ | — | **Shape**: a VS Code extension with no `package.json` |

**L0 succeeded for 4/10 repositories (4/8 with plugin shape). L1 exposed one tool surface and zero prompt surfaces.** The three `none` results were event/client-style plugins.

## Findings that affect host architecture

1. **The official closure satisfies every sample import.** Every `@deepseek-ai/*` import, including non-core packages such as `dsh-settings` and `dsh-session-reference`, was covered by source from the official 236 packages, with no gaps. This confirms §10.11's claim that the host npm closure is approximately a complete dsh distribution: the host is a full-stack composition, not a thin base with a few services.
2. **All closure gaps are outside the official package set**, in two categories: community workspace packages (`@mstar-harness/engine`, `@jianxx/dsh-cc-tools`, which depend on sibling packages not published or stable on npm) and third-party libraries (`zod`). **Implication:** the M2 host closure should include all 236 official packages plus per-plugin third-party dependencies. A missing community-internal package is a hard load failure and should be rejected with its name; the plugin/load injects-difference mechanism is a natural way to report it.
3. **The corpus needs a shape filter.** The 9,398 candidate repositories include full dsh forks and VS Code extensions, neither of which is plugin-shaped and both of which necessarily fail L0. The M7 weekly corpus pipeline should first filter for `package.json`, a plugin entry point, and non-fork status.
4. **Higher coupling correlates with lower load success, but the cause is addressable.** The highly coupled failures were all closure failures, not interface incompatibilities. With the official closure in place, there were no failures at the interface (`ctx` operations) layer.

## Experiment design

- **Host composition:** the real three dsh services (SystemPrompt + ToolRuntime + AgentRegistry) on vendor cordis in source mode. M1.5 tested package closure rather than the base; M0 had already established that the base was replaceable.
- **Closure simulation:** `gen-pkg-map.cjs` mapped names to source for the official 236 packages, then Vitest aliases exposed them to samples (equivalent to a host containing every official package). Community internal packages and third-party dependencies were not preinstalled, so missing imports surfaced. Samples were brought through Vite using `import.meta.glob`; native Node ESM resolution cannot see pnpm workspace links, which caused two rounds of infrastructure debugging.
- **L0 criterion:** import, extract the plugin surface (`default ?? module`), then call `ctx.plugin()` with a 20-second timeout to guard against an inject gate waiting forever. Failures were classified as npm closure, inject wait, apply error, or shape.
- **L1 criterion:** after loading, `tools.schemas()` is nonempty (tool surface), or the number of segments returned by `systemPrompt.assemble()` increases (prompt surface).
- **Sample composition:** three highly coupled repositories (the mstar family, with the heaviest value-level imports), one workspace member, one tool-oriented repository, three light/moderate repositories, and two special shapes (fork / VS Code) as expected failures.

## Limitations

1. Aliases simulate the official closure being present. M2 will use a real Node `node_modules` installation. The closure conclusion (official packages are satisfiable; gaps are outside that set) follows from the constructed map, but **installation size and duration** still need measurement in M2.
2. L1 did not measure the event surface because listeners cannot be enumerated through the public `ctx` API. The prompt check measured segment-count delta, not content fidelity.
3. This is a cross-sectional sample of 10 repositories. M7 will use stratified quotas for the service distribution; the deliverable here is the failure-classification list.
4. There were no inject-wait failures. The sample did not include a “missing host service” shape; with the fuller service surface planned for the M2 host, such failures are expected to remain uncommon.
