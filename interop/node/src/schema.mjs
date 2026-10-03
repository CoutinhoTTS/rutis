// A plugin's schemastery `Config` as JSON Schema, for hosts that show config
// forms (rutis-loader). Covers the shapes Cordis plugins use; anything else
// becomes `{}` (no constraint) rather than a guess. `meta.volatile` becomes
// `"x-volatile": true`, rutis-loader's marker for fields changed in place.

function text(value) {
  if (typeof value === 'string') return value
  // Localized descriptions: prefer English, else the first.
  if (value && typeof value === 'object') return value.en ?? value['en-US'] ?? Object.values(value).find(v => typeof v === 'string')
}

export function toJsonSchema(schema, depth = 0) {
  if (!schema || typeof schema !== 'object' || depth > 32) return {}
  const meta = schema.meta ?? {}
  let out
  switch (schema.type) {
    case 'string': out = { type: 'string' }; if (meta.pattern?.source) out.pattern = meta.pattern.source; break
    case 'number': {
      out = { type: meta.step === 1 ? 'integer' : 'number' }
      if (typeof meta.min === 'number') out.minimum = meta.min
      if (typeof meta.max === 'number') out.maximum = meta.max
      break
    }
    case 'natural': out = { type: 'integer', minimum: 0 }; break
    case 'percent': out = { type: 'number', minimum: 0, maximum: 1 }; break
    case 'boolean': out = { type: 'boolean' }; break
    case 'const': out = { const: schema.value }; break
    case 'object': {
      const properties = {}
      const required = []
      for (const [key, inner] of Object.entries(schema.dict ?? {})) {
        properties[key] = toJsonSchema(inner, depth + 1)
        if (inner?.meta?.required) required.push(key)
      }
      out = { type: 'object', properties }
      if (required.length) out.required = required
      break
    }
    case 'dict': out = { type: 'object', additionalProperties: toJsonSchema(schema.inner, depth + 1) }; break
    case 'array': out = { type: 'array', items: toJsonSchema(schema.inner, depth + 1) }; break
    case 'tuple': out = { type: 'array', prefixItems: (schema.list ?? []).map(s => toJsonSchema(s, depth + 1)) }; break
    case 'union': out = { anyOf: (schema.list ?? []).map(s => toJsonSchema(s, depth + 1)) }; break
    case 'intersect': out = { allOf: (schema.list ?? []).map(s => toJsonSchema(s, depth + 1)) }; break
    case 'transform': return toJsonSchema(schema.inner, depth + 1)
    default: out = {}
  }
  const description = text(meta.description)
  if (description) out.description = description
  if (meta.default !== undefined && typeof meta.default !== 'function') out.default = meta.default
  if (meta.volatile) out['x-volatile'] = true
  return out
}
