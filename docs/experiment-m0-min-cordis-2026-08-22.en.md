# M0 Foundation Experiment Results: tool-bash on min-cordis (2026-08-22)

> Decision criteria: §6 of [design-dsh-bridge-2026-08-21](design-dsh-bridge-2026-08-21.md) (both questions pass → min-cordis; either fails → vendor Cordis). Question details: §B of [review-dsh-bridge-2026-08-21](review-dsh-bridge-2026-08-21.md).
> Experiment code and reproduction scripts: `../deepseek-harness/experiments/m0-min-cordis/` (untracked directory in the dsh repo, with its own README). This file records the results.

## Conclusion

**Both questions passed → choose min-cordis as the foundation.**

Additional evidence from the same day: the official dsh `tool-pwsh-persistent/tests/loader-composition.spec.ts` was included unchanged, with only the foundation swapped. It passed using a real PowerShell process. The execution layer was therefore not a stub, and the test was written by the project itself rather than recreated equivalently for this experiment.

The top risk in review §B did **not** materialize: “loader / include / group are real divergence points in min-cordis.” The loader is an independent plugin package in dsh (`vendor/loader`), not part of Cordis core. min-cordis simply does not bundle its own loader ecosystem; it carried the official `cordis-plugin-loader` and `cordis-plugin-include` unchanged.

## Results

| Question | Scenario | Vendor Cordis baseline | min-cordis experiment |
|---|---|---|---|
| Q1 | Load tool-bash directly, print Bash schema through `tools.schemas()`, invoke pipeline (stub shell) | ✅ 3/3 | ✅ 3/3 |
| Q2 | Load real `cordis.yml` composition (Loader + include + `internal.import` module table), schema, invoke | ✅ 2/2 | ✅ 2/2 |
| Q3 | Official PowerShell loader-composition spec unchanged: real process, persistent session, cwd/env across calls, large-output truncation, exit reset | ✅ 12.7s | ✅ 13.6s |
| Purity | Module-graph audit (file-level assertion + final `globalTeardown` check) | ✅ | ✅ No vendor Cordis loaded in the experiment group |

Q2 matches the structure of the official 16 loader-composition specs (same `new Context() → plugin(Loader) → builtins.include → internal.import → loader.create('cordis:include') → loader.await()` chain). tool-bash had no official loader-composition spec, so Q2 supplies an equivalent test. Q3 is the official file with no changes.

## Experimental controls

- **Only variable was the foundation:** a Vite alias changed only the `@deepseek-ai/cordis` specifier. dsh packages, schemastery, and loader/include plugins resolved in repository-source mode through `tsconfig.base.json` paths, reusing the official `standardDecoratorPlugin`.
- **win32 isolation (Q1/Q2):** the official project excludes Bash tests on Windows because POSIX shells are absent. The experiment used `StubShellExecutor` for shell execution, capturing spawned processes while running the full `tool-bash → tools.execute → shell seam → render` path. Q3 (PowerShell) did not need this and used a real process.
- **Guard against mixed-foundation false positives:** the `m0-module-audit` Vite plugin recorded each transformed module ID. `assertBaseIdentity()` (Context identity) and `assertModuleGraphPurity()` (file level) ran alongside final graph verification in `globalTeardown`; loading vendor Cordis in min mode failed the run. This prevents a test from swapping only its own layer while package code still uses vendor Cordis.
- **Agent substitute:** copied the smallest `Agent` object from the official persistent spec (Inbox / Session / registry registration), not a fabricated interface.

## Findings

1. **min-cordis and vendor Cordis 4.0.1 are close relatives.** `service.ts` is structurally identical (only cosmokit's `defineProperty` is inlined into utils); `registry.ts` / `reflect.ts` have the same line counts. min-cordis has no external dependencies and is self-contained, so aliasing introduces no transitive resolution issue. The rc.7 ↔ 4.0.1 drift noted in review did not appear in this sample.
2. **Local `node_modules` links were incomplete.** Package links were missing for koffi (native dependency) and deep paths under `@deepseek-ai/dsh-pwsh-local/src/*`, though both existed in pnpm store. Experiment config added two path-equivalent aliases without changing tested semantics. A complete install does not need them.
3. The min-cordis snapshot was from 2026-08-22 (HEAD `80afa89`, derived from Cordis 4.0.0-rc.7).

## Limits (not covered)

1. Real bash-local / bash-sandbox execution was not tested (unavailable on win32). Q3's real PowerShell process covered the combination “real execution × min-cordis foundation”; Bash can be rechecked in a Linux lane.
2. Of the official 16 loader-composition specs, only one was run unchanged (`pwsh-persistent`).
3. min-cordis ran through Vite transpilation. Native Node loading of its direct TS exports by the bridge host remained bridge v1 work.

## Reproduce

```bash
# From the deepseek-harness repo root; Node >=22.19 (local fnm v24.18.1)
node node_modules/vitest/vitest.mjs run --config experiments/m0-min-cordis/vitest.config.ts                 # baseline
M0_BASE=min-cordis node node_modules/vitest/vitest.mjs run --config experiments/m0-min-cordis/vitest.config.ts  # experiment
# Override the min-cordis clone location with M0_MIN_CORDIS_ROOT (default D:/code/min-cordis)
```
