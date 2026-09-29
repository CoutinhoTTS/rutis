import { AsyncLocalStorage } from 'node:async_hooks'
import { encode } from './wire.mjs'
import { encodeError, decodeError } from './errors.mjs'

// Values with behaviour cross by reference, so their identity and state stay
// with the owner: objects with methods and class instances become object
// references. Built-in value types are not objects of this kind.
const builtins = [Date, RegExp, Map, Set, WeakMap, WeakSet, ArrayBuffer, DataView, Error, Promise]
function isLive(value) {
  if (value === null || typeof value !== 'object' || Array.isArray(value) || ArrayBuffer.isView(value)) return false
  if (builtins.some(type => value instanceof type)) return false
  const prototype = Object.getPrototypeOf(value)
  if (prototype !== Object.prototype && prototype !== null) return true
  return Object.values(value).some(item => typeof item === 'function')
}
// A plain object is encoded field by field when a field needs a reference.
function needsRecord(value) {
  return Object.values(value).some(item => typeof item === 'function' || item instanceof Promise || isLive(item)
    || (Array.isArray(item) && item.some(element => typeof element === 'function' || element instanceof Promise || isLive(element)))
    || (item !== null && typeof item === 'object' && !Array.isArray(item) && !isLive(item) && Object.getPrototypeOf(item) === Object.prototype && needsRecord(item)))
}

function checkData(value, seen = new Set()) {
  if (typeof value === 'function' || typeof value === 'symbol' || typeof value === 'bigint') throw new TypeError('value requires an unsupported binding')
  if (value === null || typeof value !== 'object') return
  if (seen.has(value)) throw new TypeError('cyclic data requires an object binding')
  if (!Array.isArray(value) && ![Object.prototype, null].includes(Object.getPrototypeOf(value))) throw new TypeError('object binding not implemented')
  seen.add(value)
  for (const item of Object.values(value)) checkData(item, seen)
  seen.delete(value)
}

class RemotePromise extends Promise {
  static get [Symbol.species]() { return Promise }
  #start
  constructor(start) {
    let resolve, reject
    super((a, b) => { resolve = a; reject = b })
    this.#start = () => { this.#start = undefined; start().then(resolve, reject) }
  }
  then(...args) { this.#start?.(); return super.then(...args) }
}

// One peer is used by both the Cordis runner and the application's Rust client.
// The transport owns I/O; every decoder, reference table and callback stays on
// the owning JS thread, including while a synchronous caller pumps its chain.
export class Peer {
  #send; #pump; #dispatch; #abort
  #next = 0; #received = 0; #ref = 0; #pending = new Map(); #exports = new Map(); #identities = new WeakMap()
  #imports = new Map(); #proxies = new WeakMap(); #finalizer
  #closed; #handshake = false; #resolveReady; #rejectReady
  #context = new AsyncLocalStorage(); #syncPath; #waiting = []; #queued = new Set()
  #active = 0; #draining = []
  // AbortControllers for incoming calls that received a signal argument.
  // A signal lives until the call's result settles: a returned Promise keeps
  // it (via its future export), and an `await` frame maps back to the call.
  #signals = new Map(); #decodingFor; #promiseCalls = new WeakMap(); #awaits = new Map()
  constructor({ send, pump, dispatch, abort }) {
    this.#send = send; this.#pump = pump; this.#dispatch = dispatch; this.#abort = abort
    this.ready = new Promise((resolve, reject) => { this.#resolveReady = resolve; this.#rejectReady = reject })
    this.#finalizer = new FinalizationRegistry(record => this.#release(record))
  }
  start() { this.#send(encode({ op: 'hello', version: 1 })) }
  close(error = new Error('session closed')) {
    if (this.#closed) return
    this.#closed = error; this.#rejectReady(error)
    for (const pending of this.#pending.values()) pending({ ok: false, value: error })
    this.#pending.clear(); this.#exports.clear(); this.#imports.clear(); this.#queued.clear()
  }
  #fault(error) { this.close(error); this.#abort?.(error) }
  #path() { return this.#syncPath ?? this.#context.getStore() ?? [] }
  #write(frame) { if (this.#closed) throw this.#closed; this.#send(encode(frame)) }
  #allocate() {
    if (this.#closed) throw this.#closed
    if (!this.#handshake) throw new Error('protocol handshake incomplete')
    if (!Number.isSafeInteger(++this.#next)) throw new Error('call identifiers exhausted')
    return `node:${this.#next}`
  }
  #encode(value, grants, business) {
    if (value === undefined) return { type: 'undefined' }
    const imported = value && (typeof value === 'function' || typeof value === 'object') && this.#proxies.get(value)
    if (imported) {
      if (imported.released) throw new Error('reference released')
      return { type: 'reference', value: { id: imported.id, kind: imported.kind, home: true, origin: imported.origin } }
    }
    if (typeof value === 'function' || value instanceof Promise || isLive(value)) {
      let id = this.#identities.get(value), entry = this.#exports.get(id)
      if (!entry) {
        id = ++this.#ref
        if (!Number.isSafeInteger(id)) throw new Error('reference identifiers exhausted')
        const kind = typeof value === 'function' ? 'function' : value instanceof Promise ? 'future' : 'object'
        entry = { origin: [...this.#path()], value, kind, grants: 0, business, call: kind === 'future' ? this.#promiseCalls.get(value) : undefined }
        this.#identities.set(value, id); this.#exports.set(id, entry)
        if (entry.kind === 'future') {
          if (business) this.#active++
          value.then(value => { entry.result = { ok: true, value } }, error => { entry.result = { ok: false, value: error } })
            .finally(() => { if (business) this.#finish() })
        }
      }
      if (!Number.isSafeInteger(entry.grants + 1)) throw new Error('reference grant overflow')
      entry.grants++; grants.push(id)
      return { type: 'reference', value: { id, kind: entry.kind, home: false, origin: entry.origin } }
    }
    if (Array.isArray(value)) return { type: 'list', value: value.map(value => this.#encode(value, grants, business)) }
    // An Error passed as a value (e.g. to a callback) crosses as data.
    if (value instanceof Error) return { type: 'data', value: { name: value.name, message: value.message, ...(typeof value.stack === 'string' ? { stack: value.stack } : {}) } }
    if (value !== null && typeof value === 'object' && needsRecord(value)) {
      return { type: 'record', value: Object.fromEntries(Object.entries(value).map(([key, item]) => [key, this.#encode(item, grants, business)])) }
    }
    checkData(value)
    return { type: 'data', value }
  }
  #rollback(grants) {
    for (const id of grants) { const entry = this.#exports.get(id); if (--entry.grants === 0) this.#exports.delete(id) }
  }
  #decode(wire) {
    switch (wire?.type) {
      case 'undefined': return undefined
      case 'data': return wire.value
      case 'list': return wire.value.map(value => this.#decode(value))
      case 'signal': {
        // The caller may cancel this call: the method sees a real AbortSignal.
        if (!this.#decodingFor) throw new Error('a signal is only valid as a call argument')
        let controller = this.#signals.get(this.#decodingFor)
        if (!controller) this.#signals.set(this.#decodingFor, controller = new AbortController())
        return controller.signal
      }
      case 'record': {
        if (wire.value === null || typeof wire.value !== 'object' || Array.isArray(wire.value)) throw new Error('invalid record')
        return Object.fromEntries(Object.entries(wire.value).map(([key, value]) => [key, this.#decode(value)]))
      }
      case 'reference': {
        const { id, home, kind, origin } = wire.value
        if (!Number.isSafeInteger(id) || id <= 0 || !['function', 'future', 'object'].includes(kind) || typeof home !== 'boolean' || !Array.isArray(origin) || origin.some(id => !/^(node|rust):[1-9][0-9]*$/.test(id))) throw new Error('invalid reference')
        if (home) return this.#export(id, kind).value
        // Rust does not export objects; only JS objects travel back home.
        if (kind === 'object') throw new Error('object references exported by Rust are not supported')
        let proxy = this.#imports.get(id)?.deref()
        if (proxy) {
          const record = this.#proxies.get(proxy)
          if (record.kind !== kind || JSON.stringify(record.origin) !== JSON.stringify(origin) || !Number.isSafeInteger(record.grants + 1)) throw new Error('invalid repeated grant')
          record.grants++; return proxy
        }
        const record = { id, kind, origin, grants: 1, released: false }
        const check = () => { if (record.released) throw new Error('reference released') }
        proxy = kind === 'function'
          ? (...args) => { check(); return this.#requestSync('call', { reference: id }, args) }
          : new RemotePromise(() => { check(); return this.#requestAsync('await', { reference: id }, undefined, origin) })
        record.weak = new WeakRef(proxy)
        this.#imports.set(id, record.weak); this.#proxies.set(proxy, record)
        this.#finalizer.register(proxy, record, record)
        return proxy
      }
      default: throw new Error('invalid wire value')
    }
  }
  #export(id, kind) {
    const entry = this.#exports.get(id)
    if (!entry || (kind && entry.kind !== kind)) throw new Error('unknown, released or mismatched reference')
    return entry
  }
  #release(record) {
    if (record.released) return
    record.released = true
    if (this.#imports.get(record.id) === record.weak) this.#imports.delete(record.id)
    if (!this.#closed) { try { this.#write({ op: 'release', reference: record.id, count: record.grants }) } catch (error) { this.#fault(error) } }
  }
  // Explicit release is an adapter/test operation. Session close does not wait
  // for JS GC; ordinary native functions are retained through their proxies.
  release(proxy) {
    const record = this.#proxies.get(proxy)
    if (!record) throw new Error('not an imported reference')
    this.#finalizer.unregister(record); this.#release(record)
  }
  #request(op, fields, args, finish, origin = []) {
    const id = this.#allocate(), grants = []
    try {
      const frame = { op, id, path: [...new Set([...this.#path(), ...origin])], ...fields, ...(op === 'await' ? {} : { args: this.#encode(args, grants, true) }) }
      const encoded = encode(frame)
      this.#pending.set(id, finish); this.#send(encoded)
    } catch (error) { this.#pending.delete(id); this.#rollback(grants); throw error }
    return id
  }
  #requestSync(op, fields, args) {
    let result
    const id = this.#request(op, fields, args, value => { result = value })
    this.#waiting.push(id)
    try {
      for (const job of this.#queued) this.#run(job)
      while (!result) this.#pump(() => !!result)
    }
    finally {
      this.#waiting.pop()
      if (!this.#waiting.length) for (const job of this.#queued) queueMicrotask(() => this.#run(job))
    }
    if (!result.ok) throw result.value
    return result.value
  }
  #requestAsync(op, fields, args, origin) {
    return new Promise((resolve, reject) => this.#request(op, fields, args, result => result.ok ? resolve(result.value) : reject(result.value), origin))
  }
  invoke(target, method, args) { return this.#requestSync('invoke', { target, method }, args) }
  // Promise assimilation here is intentional: this is the generated async
  // method API. Synchronous invoke returns the Promise reference itself.
  invokeAsync(target, method, args) { return this.#requestAsync('invoke', { target, method }, args) }
  drain() { return this.#active ? new Promise(resolve => this.#draining.push(resolve)) : Promise.resolve() }
  #finish() { if (--this.#active === 0) for (const resolve of this.#draining.splice(0)) resolve() }
  receive(frame) {
    if (this.#closed) return
    try {
      if (frame.op === 'hello') {
        if (frame.version !== 1 || this.#handshake) throw new Error('incompatible or duplicate protocol handshake')
        this.#handshake = true; this.#resolveReady(); return
      }
      if (!this.#handshake) throw new Error('request before protocol handshake')
      if (frame.op === 'return' || frame.op === 'throw') {
        // Decode/pin before admitting a subsequent release frame.
        const value = frame.op === 'return' ? this.#decode(frame.value) : decodeError(frame.error)
        const pending = this.#pending.get(frame.id)
        if (!pending) throw new Error('response for unknown call')
        this.#pending.delete(frame.id); pending({ ok: frame.op === 'return', value }); return
      }
      if (frame.op === 'cancel') {
        // The caller gave up; the method decides how to honour its signal.
        const call = this.#signals.has(frame.id) ? frame.id : this.#awaits.get(frame.id)
        this.#signals.get(call)?.abort(new DOMException('The operation was cancelled by the caller', 'AbortError'))
        return
      }
      if (frame.op === 'release') {
        const entry = this.#export(frame.reference)
        if (!Number.isSafeInteger(frame.count) || frame.count <= 0 || frame.count > entry.grants) throw new Error('invalid reference release count')
        if ((entry.grants -= frame.count) === 0) this.#exports.delete(frame.reference)
        return
      }
      if (!['invoke', 'call', 'get', 'await'].includes(frame.op) || typeof frame.id !== 'string' || !frame.id.startsWith('rust:') || !Array.isArray(frame.path) || frame.path.some(id => typeof id !== 'string') || frame.path.includes(frame.id)) throw new Error('invalid invocation')
      const sequence = Number(frame.id.slice(5))
      if (!/^rust:[1-9][0-9]*$/.test(frame.id) || !Number.isSafeInteger(sequence) || sequence <= this.#received) throw new Error('invalid or repeated invocation identity')
      this.#received = sequence
      if (frame.op === 'call' && frame.method !== undefined && typeof frame.method !== 'string') throw new Error('invalid method')
      if (frame.op === 'get' && typeof frame.property !== 'string') throw new Error('invalid property')
      let args
      this.#decodingFor = frame.id
      try { args = frame.op === 'await' || frame.op === 'get' ? undefined : this.#decode(frame.args) } finally { this.#decodingFor = undefined }
      const kind = frame.op === 'await' ? 'future' : frame.op === 'get' || frame.method !== undefined ? 'object' : 'function'
      const entry = frame.op === 'invoke' ? undefined : this.#export(frame.reference, kind)
      const business = entry?.business ?? frame.target !== ''
      if (business) this.#active++
      const job = { frame, args, entry, business }
      this.#queued.add(job)
      if (this.#waiting.length) this.#run(job)
      else queueMicrotask(() => this.#run(job))
    } catch (error) { this.#fault(error) }
  }
  #run(job) {
    if (!this.#queued.has(job)) return
    if (this.#waiting.length && !this.#waiting.some(id => job.frame.path.includes(id))) return
    this.#queued.delete(job)
    this.#execute(job.frame, job.args, job.entry, job.business)
  }
  #execute(frame, args, entry, business) {
    if (this.#closed) { if (business) this.#finish(); return }
    const path = [...frame.path, frame.id]
    const previous = this.#syncPath
    this.#syncPath = path
    try {
      this.#context.run(path, () => {
        if (frame.op === 'await') {
          if (entry.call) this.#awaits.set(frame.id, entry.call)
          if (entry.result) this.#respond(frame.id, entry.result, business)
          else if (this.#waiting.length) this.#respond(frame.id, { ok: false, value: Object.assign(new Error(`await requires the Node thread occupied by its parent synchronous call; path: ${path.join(' -> ')}`), { name: 'SyncWaitCycle' }) }, business)
          else entry.value.then(value => this.#respond(frame.id, { ok: true, value }, business), value => this.#respond(frame.id, { ok: false, value }, business))
          return
        }
        try {
          const value = frame.op === 'get' ? entry.value[frame.property]
            : frame.op === 'call' && frame.method !== undefined ? Reflect.apply(entry.value[frame.method], entry.value, args)
              : frame.op === 'call' ? Reflect.apply(entry.value, undefined, args)
                : this.#dispatch(frame.target, frame.method, args)
          // A returned Promise keeps the call's signal until it settles.
          if (value instanceof Promise && this.#signals.has(frame.id)) {
            const call = frame.id
            this.#promiseCalls.set(value, call)
            value.then(() => this.#signals.delete(call), () => this.#signals.delete(call))
          }
          this.#respond(frame.id, { ok: true, value }, business)
        } catch (value) { this.#respond(frame.id, { ok: false, value }, business) }
      })
    } finally { this.#syncPath = previous }
  }
  #respond(id, result, business) {
    this.#awaits.delete(id)
    if (!(result.ok && result.value instanceof Promise && this.#promiseCalls.has(result.value))) this.#signals.delete(id)
    const grants = []
    try {
      if (this.#closed) return
      let frame
      try {
        frame = result.ok ? { op: 'return', id, value: this.#encode(result.value, grants, business) } : { op: 'throw', id, error: encodeError(result.value) }
        frame = encode(frame)
      } catch (error) { this.#rollback(grants); grants.length = 0; frame = encode({ op: 'throw', id, error: encodeError(error) }) }
      this.#send(frame)
    } catch (error) { this.#fault(error) }
    finally { if (business) this.#finish() }
  }
}
