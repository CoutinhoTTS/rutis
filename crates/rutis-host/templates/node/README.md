# {{name}}

A [rutis](https://github.com/arcships/rutis) plugin.

```bash
npm install
npm test          # unit tests, no host needed
npx rutis-host dev  # run it in a local host; changes reload it
```

Publish with `npm publish` (the workflow in `.github/workflows/publish.yml` does it for a `v*` tag). A host installs it next to `@arcships/rutis-runtime` and adds a row `{ "id": "{{id}}", "name": "{{name}}" }`.
