// Static capability census of the baseline plugins' public service API.
// For every method and property of each service declared on Cordis
// `Context`, list the binding capabilities its signature needs, so gaps are
// counted from real plugins instead of guessed. `node classify.mjs [--json]`.
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import { readFileSync } from 'node:fs'
import { targets } from './scenarios.mjs'
import { generate } from '../node/src/generate.mjs'

const require = createRequire(import.meta.url)
const ts = require(require.resolve('typescript', { paths: [new URL('../node', import.meta.url).pathname] }))

// Capabilities, and whether the current generator / wire protocol has them.
export const CAPABILITIES = {
  primitive: ['number, string, boolean, void and their arrays', true, true],
  'data-object': ['plain data object / interface (incl. literal unions, nullable)', true, true],
  branded: ['branded string or number', true, true],
  dynamic: ['any / unknown', true, true],
  optional: ['optional or rest parameter', true, true],
  callback: ['function-typed parameter (a Rust closure)', true, true],
  'returns-function': ['returns a function (e.g. a disposer)', true, true],
  'live-object': ['object with methods or class instance (by reference)', true, true],
  'abort-signal': ['AbortSignal parameter (aborted when the Rust future is dropped)', true, true],
  bytes: ['Uint8Array / ArrayBuffer', false, false],
  'async-iterable': ['AsyncIterable / stream', false, false],
  property: ['public property (read live)', true, true],
  generic: ['generic method', false, false],
}

function census(packageName) {
  const packageJson = require.resolve(`${packageName}/package.json`)
  const dir = dirname(packageJson)
  const manifest = JSON.parse(readFileSync(packageJson))
  const typesPath = join(dir, manifest.types ?? manifest.exports?.['.']?.types)
  const program = ts.createProgram([typesPath], {
    target: ts.ScriptTarget.ESNext, module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext, strict: true, skipLibCheck: true, noEmit: true,
  })
  const checker = program.getTypeChecker()

  function classify(type, caps, seen = new Set()) {
    if (seen.has(type)) return
    seen.add(type)
    const flags = type.flags
    if (flags & (ts.TypeFlags.Any | ts.TypeFlags.Unknown)) return caps.add('dynamic')
    if (flags & (ts.TypeFlags.StringLike | ts.TypeFlags.NumberLike | ts.TypeFlags.BooleanLike | ts.TypeFlags.Void | ts.TypeFlags.Undefined | ts.TypeFlags.Null | ts.TypeFlags.Never)) {
      if (flags & (ts.TypeFlags.StringLiteral | ts.TypeFlags.NumberLiteral)) caps.add('data-object')
      return caps.add('primitive')
    }
    if (type.isUnion()) return type.types.forEach(member => classify(member, caps, seen))
    if (type.isIntersection()) {
      if (type.types.some(member => member.flags & (ts.TypeFlags.String | ts.TypeFlags.Number))) return caps.add('branded')
      return type.types.forEach(member => classify(member, caps, seen))
    }
    const name = type.getSymbol()?.getName()
    if (name === 'AbortSignal') return caps.add('abort-signal')
    if (['Uint8Array', 'ArrayBuffer', 'Buffer'].includes(name)) return caps.add('bytes')
    if (['AsyncIterable', 'AsyncIterableIterator', 'AsyncGenerator', 'ReadableStream'].includes(name)) return caps.add('async-iterable')
    if (checker.isArrayType(type) || checker.isTupleType(type)) {
      return checker.getTypeArguments(type).forEach(element => classify(element, caps, seen))
    }
    if (type.getCallSignatures().length) return caps.add('callback')
    const properties = checker.getPropertiesOfType(type)
    const methods = properties.filter(property => {
      const declaration = property.valueDeclaration ?? property.declarations?.[0]
      return declaration && checker.getTypeOfSymbolAtLocation(property, declaration).getCallSignatures().length
    })
    if (methods.length || type.getSymbol()?.flags & ts.SymbolFlags.Class) return caps.add('live-object')
    caps.add('data-object')
    for (const property of properties) {
      const declaration = property.valueDeclaration ?? property.declarations?.[0]
      if (declaration) classify(checker.getTypeOfSymbolAtLocation(property, declaration), caps, seen)
    }
  }

  const services = [], events = []
  for (const file of program.getSourceFiles()) {
    if (!file.fileName.startsWith(dir)) continue
    ts.forEachChild(file, function visit(node) {
      if (ts.isModuleDeclaration(node) && ts.isStringLiteral(node.name) && node.name.text === '@deepseek-ai/cordis') {
        for (const statement of node.body?.statements ?? []) {
          if (!ts.isInterfaceDeclaration(statement)) continue
          for (const member of statement.members) {
            if (statement.name.text === 'Context' && member.type) services.push([member.name.getText(), checker.getTypeFromTypeNode(member.type)])
            if (statement.name.text === 'Events') events.push(member)
          }
        }
      }
      ts.forEachChild(node, visit)
    })
  }

  const members = []
  for (const [service, type] of services) {
    for (const property of checker.getPropertiesOfType(type)) {
      const declaration = property.valueDeclaration ?? property.declarations?.[0]
      if (!declaration || declaration.getSourceFile().fileName.includes('/cordis/')) continue
      if (ts.getCombinedModifierFlags(declaration) & (ts.ModifierFlags.Private | ts.ModifierFlags.Protected)) continue
      if (property.getName().startsWith('_') || property.getName().startsWith('__@')) continue
      const caps = new Set()
      const propertyType = checker.getTypeOfSymbolAtLocation(property, declaration)
      const signatures = propertyType.getCallSignatures()
      if (!signatures.length) {
        caps.add('property')
        classify(propertyType, caps)
      }
      for (const signature of signatures) {
        if (signature.typeParameters?.length) caps.add('generic')
        for (const parameter of signature.parameters) {
          const parameterDeclaration = parameter.valueDeclaration
          if (parameterDeclaration && (parameterDeclaration.questionToken || parameterDeclaration.dotDotDotToken || parameterDeclaration.initializer)) caps.add('optional')
          classify(checker.getTypeOfSymbolAtLocation(parameter, parameterDeclaration), caps)
        }
        const returned = checker.getReturnTypeOfSignature(signature)
        const awaited = checker.getPromisedTypeOfPromise(returned) ?? returned
        if (awaited.getCallSignatures().length) caps.add('returns-function')
        else classify(awaited, caps)
      }
      members.push({ service, member: property.getName(), caps: [...caps].sort() })
    }
  }
  const eventKinds = events.map(member => {
    const text = member.getText()
    const kind = /\bnext\s*:/.test(text) ? 'waterfall' : /\)\s*:\s*void\s*;?$/.test(text.trim()) ? 'notify' : 'returns-value'
    return { event: member.name.getText().replace(/'/g, ''), kind }
  })
  return { package: packageName, version: manifest.version, services: services.map(([name]) => name), members, events: eventKinds }
}

const report = targets.map(target => {
  // Abstract seams declare the service; the concrete package implements it.
  const seam = target.package.replace(/-local$/, '')
  const entry = census(seam)
  // Ground truth for binding coverage: what the generator actually binds.
  const packageDir = dirname(require.resolve(`${target.package}/package.json`))
  const { diagnostics } = generate(packageDir, new URL('../node', import.meta.url).pathname)
  const unbound = new Set(diagnostics.map(line => line.match(/: ([\w$]+\.[\w$]+) is not bound/)?.[1]).filter(Boolean))
  for (const member of entry.members) member.bound = !unbound.has(`${member.service}.${member.member}`)
  return entry
})

if (process.argv.includes('--json')) {
  process.stdout.write(JSON.stringify(report, null, 2))
} else {
  const counts = Object.fromEntries(Object.keys(CAPABILITIES).map(cap => [cap, 0]))
  let total = 0, bound = 0, protocolReady = 0
  for (const entry of report) {
    console.log(`\n## ${entry.package}@${entry.version} (ctx.${entry.services.join(', ctx.')})`)
    for (const { member, caps } of entry.members) {
      total++
      caps.forEach(cap => counts[cap]++)
      if (entry.members.find(item => item.member === member)?.bound) bound++
      if (caps.every(cap => CAPABILITIES[cap][2])) protocolReady++
      const mark = entry.members.find(item => item.member === member)?.bound ? 'bound  ' : 'missing'
      console.log(`  ${mark} ${member.padEnd(26)} ${caps.join(', ')}`)
    }
    for (const { event, kind } of entry.events) console.log(`  event ${event.padEnd(20)} ${kind}`)
  }
  console.log(`\n## Totals: ${total} members; bound by the generator ${bound}; protocol-ready ${protocolReady}`)
  for (const [cap, [description, generator, protocol]] of Object.entries(CAPABILITIES)) {
    console.log(`  ${cap.padEnd(18)} ${String(counts[cap]).padStart(3)}  generator:${generator ? 'yes' : 'no '} protocol:${protocol ? 'yes' : 'no '}  ${description}`)
  }
}
