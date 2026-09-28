// Error graphs preserve AggregateError order, shared causes, cycles and thrown
// values. They are data; decoding never selects an arbitrary global constructor.
const constructors = { Error, TypeError, RangeError, ReferenceError, SyntaxError, URIError, EvalError }

export function encodeError(thrown) {
  const nodes = [], seen = new Map()
  const visit = value => {
    if (typeof value === 'function' || typeof value === 'symbol') throw new TypeError('function or symbol in error graph is not supported')
    if (value === undefined) return { type: 'undefined' }
    if (typeof value === 'bigint') return { type: 'bigint', value: String(value) }
    if (typeof value === 'number' && !Number.isFinite(value)) return { type: 'number', value: String(value) }
    if (value === null || typeof value !== 'object') return { type: 'data', value }
    if (seen.has(value)) return { type: 'reference', value: seen.get(value) }
    const id = nodes.length
    seen.set(value, id); nodes.push(null)
    if (value instanceof Error) {
      nodes[id] = {
        type: 'error', name: value.name, message: value.message, stack: value.stack,
        ...('cause' in value ? { cause: visit(value.cause) } : {}),
        ...(value instanceof AggregateError ? { errors: [...value.errors].map(visit) } : {}),
      }
    } else if (Array.isArray(value)) nodes[id] = { type: 'array', values: value.map(visit) }
    else nodes[id] = { type: 'object', values: Object.entries(value).map(([key, value]) => [key, visit(value)]) }
    return { type: 'reference', value: id }
  }
  return { name: thrown?.name ?? 'ThrownValue', message: String(thrown?.message ?? thrown), graph: { root: visit(thrown), nodes } }
}

export function decodeError({ name, message, graph }) {
  const make = node => node.errors !== undefined ? new AggregateError([], node.message)
    : new (Object.hasOwn(constructors, node.name) ? constructors[node.name] : Error)(node.message)
  if (!graph) return Object.assign(make({ name, message }), { name })
  if (!Array.isArray(graph.nodes)) throw new TypeError('invalid error graph')
  const nodes = graph.nodes.map(node => {
    if (node.type === 'error') return Object.assign(make(node), { name: node.name, ...(node.stack === undefined ? {} : { stack: node.stack }) })
    if (node.type === 'array') return []
    if (node.type === 'object') return {}
    throw new TypeError('invalid error node')
  })
  const read = value => {
    switch (value.type) {
      case 'undefined': return undefined
      case 'data': return value.value
      case 'bigint': return BigInt(value.value)
      case 'number': return Number(value.value)
      case 'reference':
        if (!Number.isSafeInteger(value.value) || value.value < 0 || value.value >= nodes.length) throw new TypeError('invalid error reference')
        return nodes[value.value]
      default: throw new TypeError('invalid error value')
    }
  }
  graph.nodes.forEach((node, id) => {
    const target = nodes[id]
    if (node.type === 'error') {
      if ('cause' in node) Object.defineProperty(target, 'cause', { value: read(node.cause), configurable: true, writable: true })
      if (node.errors !== undefined) target.errors = node.errors.map(read)
    } else if (node.type === 'array') target.push(...node.values.map(read))
    else for (const [key, value] of node.values) Object.defineProperty(target, key, { value: read(value), enumerable: true, configurable: true, writable: true })
  })
  return read(graph.root)
}
