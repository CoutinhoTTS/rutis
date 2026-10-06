# {{name}}

A [rutis](https://github.com/arcships/rutis) plugin.

```bash
uv sync
uv run python -m unittest   # unit tests, no host needed
uv run rutis-host dev       # run it in a local host; changes reload it
```

Publish with `uv build && uv publish` (the workflow in `.github/workflows/publish.yml` does it for a `v*` tag). A host installs it where its Python runtime runs and adds a row `{ "id": "{{id}}", "name": "py:{{id}}" }`.
