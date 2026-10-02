# Fixtures

`patch-expected.json` is generated from `patch-cases.json` with the JavaScript
original, `applyEntryPatches` from `@deepseek-ai/cordis-plugin-include`
(installed by `npm --prefix crates/rutis-dsh/dsh ci`):

```js
import { applyEntryPatches } from '<rutis>/crates/rutis-dsh/dsh/node_modules/@deepseek-ai/cordis-plugin-include/lib/index.js'
// for each case:
const warnings = []
const rows = applyEntryPatches([], structuredClone(c.layers.flat()), (m) => warnings.push(m))
// { name: c.name, rows, warnings: warnings.length }
```

Regenerate it after adding a case; never edit it by hand.
