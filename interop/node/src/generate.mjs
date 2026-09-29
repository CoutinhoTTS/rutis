import ts from 'typescript'
import { existsSync, readFileSync, statSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { basename, dirname, join, resolve } from 'node:path'

const keywords = new Set('as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while'.split(' '))
function ident(name) {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name) || ['self', 'Self', 'super', 'crate'].includes(name)) {
    throw new Unsupported(`cannot represent identifier ${JSON.stringify(name)} in Rust`)
  }
  return keywords.has(name) ? `r#${name}` : name
}
const snake = name => name.replace(/[A-Z]/g, (letter, index) => (index ? '_' : '') + letter.toLowerCase()).replace(/[^A-Za-z0-9_]/g, '_')
// SCREAMING_CASE words are lowered first, so they read as PascalCase words.
const pascal = name => (/[a-z]/.test(name) ? name : name.toLowerCase()).replace(/(^|[^A-Za-z0-9])([a-z])/g, (_, __, letter) => letter.toUpperCase()).replace(/[^A-Za-z0-9]/g, '')
const literal = value => JSON.stringify(value)

/** A member or type the bindings cannot represent yet; reported, not fatal. */
class Unsupported extends Error {}

// Where the plugin is described and loaded. A TypeScript source file is
// analysed directly; a package directory is analysed through its declared
// `types` and loaded through its runtime entry.
function locate(pluginPath) {
  if (existsSync(pluginPath) && statSync(pluginPath).isDirectory()) {
    const manifest = JSON.parse(readFileSync(join(pluginPath, 'package.json'), 'utf8'))
    const root = manifest.exports?.['.'] ?? manifest.exports
    const types = manifest.types ?? manifest.typings ?? root?.types
    const entry = (typeof root === 'string' ? root : root?.import ?? root?.default) ?? manifest.main ?? 'index.js'
    if (!types) throw new Error(`${pluginPath}: package declares no types`)
    return { analysed: resolve(pluginPath, types), entry: resolve(pluginPath, entry), packageDir: resolve(pluginPath) }
  }
  return { analysed: pluginPath, entry: pluginPath, packageDir: dirname(pluginPath) }
}

// `plugins` is one plugin path, or a group [{ name, path }] mounted together
// in one Cordis Context (dependencies between them resolve natively). Group
// members get one `Config` field each, named by `name`.
// `options.provide` lists Cordis services the rutis application provides to
// the plugins; their interface comes from the plugins' Context declarations.
export function generate(plugins, nodePackage, { provide = [] } = {}) {
  const single = !Array.isArray(plugins)
  const group = (single ? [{ path: plugins }] : plugins).map(plugin => ({ ...plugin, ...locate(plugin.path) }))
  if (!group.length) throw new Error('a mount needs at least one plugin')
  if (!single) {
    const names = new Set()
    for (const { name } of group) {
      if (!/^[a-z][a-z0-9_]*$/.test(name ?? '') || keywords.has(name) || names.has(name)) throw new Error(`invalid or duplicate group member name ${JSON.stringify(name)}`)
      names.add(name)
    }
  }
  const program = ts.createProgram(group.map(plugin => plugin.analysed), {
    target: ts.ScriptTarget.ESNext, module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext,
    strict: true, skipLibCheck: true, noEmit: true,
  })
  const errors = ts.getPreEmitDiagnostics(program).filter(diagnostic => diagnostic.category === ts.DiagnosticCategory.Error)
  if (errors.length) {
    throw new Error(ts.formatDiagnosticsWithColorAndContext(errors, {
      getCanonicalFileName: value => value, getCurrentDirectory: () => process.cwd(), getNewLine: () => '\n',
    }))
  }
  const checker = program.getTypeChecker()
  for (const plugin of group) {
    plugin.source = program.getSourceFile(plugin.analysed)
    if (!plugin.source) throw new Error(`source not found: ${plugin.analysed}`)
  }
  const isCordis = node => node.getSourceFile().fileName.replaceAll('\\', '/').includes('/@deepseek-ai/cordis/')
  const location = node => {
    const { line, character } = node.getSourceFile().getLineAndCharacterOfPosition(node.getStart())
    return `${node.getSourceFile().fileName}:${line + 1}:${character + 1}`
  }
  function fail(node, reason) { throw new Error(`${location(node)}: ${reason}`) }
  const diagnostics = []

  // ---------------------------------------------------------------------
  // Rust type model: named data types are generated once per TS type.
  // ---------------------------------------------------------------------
  const items = []            // generated Rust item source
  const named = new Map()     // ts.Type -> Rust name
  const taken = new Set(['Config', 'Plugin'])
  const building = new Set()
  function claim(base) {
    let name = pascal(base) || 'Value'
    if (/^[0-9]/.test(name)) name = `T${name}`
    for (let i = 2; taken.has(name); i++) name = `${pascal(base)}${i}`
    taken.add(name)
    return name
  }
  const derive = '#[derive(Debug, Clone, PartialEq, ::rutis_interop::serde::Serialize, ::rutis_interop::serde::Deserialize)]\n#[serde(crate = "rutis_interop::serde")]'
  const typeName = (type, hint) => type.aliasSymbol?.getName() ?? (type.getSymbol()?.getName().startsWith('__') ? undefined : type.getSymbol()?.getName()) ?? hint

  function stripNullish(type) {
    if (!type.isUnion()) return { type, optional: false, members: [type] }
    const members = type.types.filter(member => !(member.flags & (ts.TypeFlags.Undefined | ts.TypeFlags.Null | ts.TypeFlags.Void)))
    return { type, optional: members.length !== type.types.length, members }
  }

  // Map a TypeScript type to a Rust type used for data (arguments, results,
  // fields). Throws Unsupported with the reason for non-data types.
  function rust(type, hint) {
    const flags = type.flags
    if (flags & (ts.TypeFlags.Any | ts.TypeFlags.Unknown)) return '::rutis_interop::serde_json::Value'
    if (flags & (ts.TypeFlags.Void | ts.TypeFlags.Undefined | ts.TypeFlags.Null)) return '()'
    if (flags & ts.TypeFlags.Never) throw new Unsupported('never type')
    if (flags & ts.TypeFlags.BigIntLike) throw new Unsupported('bigint')
    if (flags & ts.TypeFlags.ESSymbolLike) throw new Unsupported('symbol')
    if (type.isUnion()) {
      const { optional, members } = stripNullish(type)
      const inner = unionMembers(type, members, hint)
      return optional ? `Option<${inner}>` : inner
    }
    if (flags & ts.TypeFlags.BooleanLike) return 'bool'
    if (flags & ts.TypeFlags.NumberLike) return 'f64'
    if (flags & ts.TypeFlags.StringLike) return 'String'
    if (type.isIntersection()) {
      const primitive = type.types.find(member => member.flags & (ts.TypeFlags.String | ts.TypeFlags.Number))
      if (primitive) return brand(type, primitive, hint)
      throw new Unsupported(`intersection ${checker.typeToString(type)}`)
    }
    const symbol = type.getSymbol()?.getName()
    // JS errors passed as values cross as { name, message, stack }.
    const lib = type.getSymbol()?.declarations?.[0]?.getSourceFile()
    if (symbol?.endsWith('Error') && lib && program.isSourceFileDefaultLibrary(lib)) return '::rutis_interop::JsError'
    if (symbol === 'AbortSignal') throw new Unsupported('AbortSignal parameter (cancellation is not bound yet)')
    if (['Uint8Array', 'ArrayBuffer', 'Buffer', 'DataView'].includes(symbol)) throw new Unsupported(`${symbol} (binary data is not bound yet)`)
    if (['AsyncIterable', 'AsyncIterableIterator', 'AsyncGenerator', 'ReadableStream', 'Iterable'].includes(symbol)) throw new Unsupported(`${symbol} (streams are not bound yet)`)
    if (['Promise', 'PromiseLike'].includes(symbol)) throw new Unsupported('nested Promise')
    if (checker.isArrayType(type)) return `Vec<${rust(checker.getTypeArguments(type)[0], `${hint}Item`)}>`
    if (checker.isTupleType(type)) throw new Unsupported(`tuple ${checker.typeToString(type)}`)
    if (type.getCallSignatures().length || type.getConstructSignatures().length) throw new Unsupported('function value (callbacks are not bound yet)')
    return object(type, hint)
  }

  // What is being mapped, for diagnostics raised while mapping its types.
  let mapping
  const notes = new Set()
  function unionMembers(type, members, hint) {
    // Function members (e.g. `string | ((ctx) => string)`) cannot cross as
    // data: bind the data members and report the rest.
    const data = members.filter(member => !member.getCallSignatures().length)
    if (data.length !== members.length) {
      if (!data.length) throw new Unsupported('function value (callbacks are not bound yet)')
      const note = `${mapping ?? 'type'}: function values in ${checker.typeToString(type)} are not bound; only its data members are`
      if (!notes.has(note)) { notes.add(note); diagnostics.push(note) }
      members = data
    }
    if (!members.length) return '()'
    if (members.length === 1) return rust(members[0], hint)
    if (members.every(member => member.flags & ts.TypeFlags.BooleanLiteral)) return 'bool'
    if (members.every(member => member.flags & ts.TypeFlags.NumberLike)) return 'f64'
    if (members.every(member => member.flags & ts.TypeFlags.StringLiteral)) return literalEnum(type, members, hint)
    if (members.every(member => member.flags & ts.TypeFlags.StringLike)) return 'String'
    // Other unions (e.g. discriminated objects) stay dynamic JSON: the data
    // crosses unchanged, only its static Rust shape is not generated.
    return '::rutis_interop::serde_json::Value'
  }

  function literalEnum(type, members, hint) {
    if (named.has(type)) return named.get(type)
    const name = claim(typeName(type, hint))
    named.set(type, name)
    const variants = new Set()
    const body = members.map(member => {
      let variant = pascal(member.value) || 'Empty'
      if (/^[0-9]/.test(variant)) variant = `V${variant}`
      while (variants.has(variant)) variant += '_'
      variants.add(variant)
      return `#[serde(rename = ${literal(member.value)})] ${variant},`
    })
    items.push(`${derive.replace('PartialEq', 'PartialEq, Eq, Copy, Hash')}\npub enum ${name} { ${body.join(' ')} }`)
    return name
  }

  // Branded primitives become transparent newtypes that keep their name.
  function brand(type, primitive, hint) {
    if (named.has(type)) return named.get(type)
    const inner = primitive.flags & ts.TypeFlags.String ? 'String' : 'f64'
    const alias = type.aliasSymbol?.getName()
    if (!alias) return inner
    const name = claim(alias)
    named.set(type, name)
    const conversions = inner === 'String'
      ? `impl From<&str> for ${name} { fn from(value: &str) -> Self { Self(value.to_owned()) } }
         impl From<String> for ${name} { fn from(value: String) -> Self { Self(value) } }
         impl ::std::fmt::Display for ${name} { fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result { f.write_str(&self.0) } }`
      : `impl From<f64> for ${name} { fn from(value: f64) -> Self { Self(value) } }`
    items.push(`${derive}\n#[serde(transparent)]\npub struct ${name}(pub ${inner});\n${conversions}`)
    return name
  }

  function object(type, hint) {
    if (named.has(type)) {
      const name = named.get(type)
      return building.has(type) ? `Box<${name}>` : name
    }
    const properties = checker.getPropertiesOfType(type)
    const methods = properties.filter(property => {
      const declaration = property.valueDeclaration ?? property.declarations?.[0]
      return declaration && checker.getTypeOfSymbolAtLocation(property, declaration).getCallSignatures().length
    })
    const index = checker.getIndexInfoOfType(type, ts.IndexKind.String)
    if (index && !properties.length && !methods.length) return `::std::collections::BTreeMap<String, ${rust(index.type, `${hint}Value`)}>`
    // Built-in JS types (Map, Set, iterators, ...) are not plugin objects:
    // they neither cross as data nor as object references.
    const declared = type.getSymbol()?.declarations?.[0]?.getSourceFile()
    if (declared && program.isSourceFileDefaultLibrary(declared) && (methods.length || type.getSymbol()?.flags & ts.SymbolFlags.Class)) {
      throw new Unsupported(`${checker.typeToString(type)} (built-in type is not bound)`)
    }
    if (methods.length || (type.getSymbol()?.flags & ts.SymbolFlags.Class)) return liveObject(type, hint)
    const name = claim(typeName(type, hint))
    named.set(type, name)
    building.add(type)
    try {
      const fields = new Set()
      const body = properties.flatMap(property => {
        const declaration = property.valueDeclaration ?? property.declarations?.[0]
        const propertyType = checker.getTypeOfSymbolAtLocation(property, declaration)
        const { members: present } = stripNullish(propertyType)
        // Optional AbortSignal fields are left unset, as for parameters.
        if (property.flags & ts.SymbolFlags.Optional && present.length === 1 && present[0].getSymbol()?.getName() === 'AbortSignal') return []
        let field = snake(property.getName())
        field = keywords.has(field) ? `r#${field}` : field
        if (!/^(r#)?[a-z_][a-z0-9_]*$/.test(field) || fields.has(field)) throw new Unsupported(`field ${property.getName()} cannot be represented in Rust`)
        fields.add(field)
        const optional = !!(property.flags & ts.SymbolFlags.Optional)
        let fieldType = rust(propertyType, `${name}${pascal(property.getName())}`)
        const attributes = [`rename = ${literal(property.getName())}`]
        if (optional && !fieldType.startsWith('Option<')) fieldType = `Option<${fieldType}>`
        if (fieldType.startsWith('Option<')) attributes.push('default', 'skip_serializing_if = "Option::is_none"')
        return [`#[serde(${attributes.join(', ')})] pub ${field}: ${fieldType},`]
      })
      if (index) body.push(`#[serde(flatten)] pub extra: ::std::collections::BTreeMap<String, ${rust(index.type, `${name}Extra`)}>,`)
      items.push(`${derive}\npub struct ${name} { ${body.join('\n')} }`)
    } catch (error) {
      named.delete(type)
      taken.delete(name)
      throw error
    } finally {
      building.delete(type)
    }
    return name
  }

  // ---------------------------------------------------------------------
  // Plugin shape: a function plugin (`apply`) or a Service class export.
  // ---------------------------------------------------------------------
  // Context augmentations anywhere in the program: service name -> type.
  const augmentations = new Map()
  for (const file of program.getSourceFiles()) {
    ts.forEachChild(file, function visit(node) {
      if (ts.isModuleDeclaration(node) && ts.isStringLiteral(node.name) && node.name.text === '@deepseek-ai/cordis') {
        for (const statement of node.body?.statements ?? []) {
          if (!ts.isInterfaceDeclaration(statement) || statement.name.text !== 'Context') continue
          for (const member of statement.members) {
            if (member.name && member.type) augmentations.set(member.name.getText().replace(/^['"]|['"]$/g, ''), { type: checker.getTypeFromTypeNode(member.type), node: member, file: file.fileName })
          }
        }
      }
      ts.forEachChild(node, visit)
    })
  }

  function discover({ source, analysed, packageDir }) {
    const moduleSymbol = checker.getSymbolAtLocation(source)
    if (!moduleSymbol) throw new Error(`${analysed}: not a module`)
    const exports = checker.getExportsOfModule(moduleSymbol)
    const resolveAlias = symbol => symbol.flags & ts.SymbolFlags.Alias ? checker.getAliasedSymbol(symbol) : symbol
    const applyExport = exports.find(symbol => symbol.name === 'apply')
    const defaultExport = exports.find(symbol => symbol.name === 'default')
    const pluginClass = !applyExport && defaultExport && resolveAlias(defaultExport).flags & ts.SymbolFlags.Class ? resolveAlias(defaultExport) : undefined
    if (!applyExport && !pluginClass) throw new Error(`${analysed}: expected an apply function or a default-exported Service class`)

    const services = new Map() // name -> { type, node }
    let configType
    if (pluginClass) {
      // A Service class provides the Context members typed as itself or one of
      // its base classes (a seam declares `credentials: CredentialProvider`,
      // an implementation extends CredentialProvider).
      const instance = checker.getDeclaredTypeOfSymbol(pluginClass)
      const lineage = new Set()
      for (let queue = [instance]; queue.length;) {
        const current = queue.pop()
        const symbol = current.getSymbol()
        if (!symbol || lineage.has(symbol)) continue
        lineage.add(symbol)
        if (current.isClassOrInterface()) queue.push(...(checker.getBaseTypes(current) ?? []))
      }
      for (const [name, entry] of augmentations) {
        const symbol = entry.type.getSymbol()
        if (symbol && lineage.has(symbol) && !isCordis(symbol.declarations[0])) services.set(name, entry)
      }
      const construct = checker.getTypeOfSymbolAtLocation(pluginClass, pluginClass.valueDeclaration).getConstructSignatures()[0]
      const configParameter = construct?.parameters[1]
      if (configParameter) configType = checker.getTypeOfSymbolAtLocation(configParameter, configParameter.valueDeclaration)
    } else {
      const applyType = checker.getTypeOfSymbolAtLocation(applyExport, applyExport.valueDeclaration)
      const configParameter = applyType.getCallSignatures()[0]?.parameters[1]
      if (configParameter) configType = checker.getTypeOfSymbolAtLocation(configParameter, configParameter.valueDeclaration)
      // Source plugins: literal ctx.provide calls. Declaration-only packages:
      // the Context members the package itself declares.
      ts.forEachChild(source, function visit(node) {
        if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression) && node.expression.name.text === 'provide') {
          const declaration = checker.getResolvedSignature(node)?.declaration
          if (declaration && isCordis(declaration)) {
            const [name, value] = node.arguments
            if (!name || !ts.isStringLiteral(name) || !value) fail(node, 'service discovery requires a literal native service name and a value')
            if (services.has(name.text)) fail(node, `multiple declarations for service ${name.text} require further scope analysis`)
            services.set(name.text, augmentations.get(name.text) ?? { type: checker.getTypeAtLocation(value), node })
          }
        }
        ts.forEachChild(node, visit)
      })
      if (!services.size && source.isDeclarationFile) {
        for (const [name, entry] of augmentations) if (entry.file.startsWith(packageDir) && !provide.includes(name)) services.set(name, entry)
      }
    }
    return { services, configType }
  }

  const services = new Map() // name -> { type, node, plugin }
  for (const plugin of group) {
    const found = discover(plugin)
    plugin.configType = found.configType
    for (const [name, entry] of found.services) {
      if (services.has(name)) throw new Error(`service ${name} is provided by both ${services.get(name).plugin.path} and ${plugin.path}`)
      services.set(name, { ...entry, plugin })
    }
  }
  const provided = new Map() // host-provided service name -> { type, node }
  for (const name of provide) {
    const entry = augmentations.get(name)
    if (!entry) throw new Error(`no Cordis Context declaration found for the host-provided service ${name}`)
    if (services.has(name)) throw new Error(`service ${name} is provided both by the host and by ${services.get(name).plugin.path}`)
    provided.set(name, entry)
  }
  if (!services.size && !provided.size) throw new Error(`${group.map(plugin => plugin.analysed).join(', ')}: no native Cordis service registrations found`)

  // ---------------------------------------------------------------------
  // Services and their members.
  // ---------------------------------------------------------------------
  const serviceCode = []
  const structs = new Map() // service name -> Rust struct name
  const manifest = {}
  for (const [serviceName, { type, node }] of services) {
    const structName = claim(typeName(type, serviceName))
    structs.set(serviceName, structName)
    const { methods, unavailable } = bindMembers(type, serviceName, structName, { properties: false })
    manifest[serviceName] = methods.map(method => method.name)
    serviceCode.push(serviceStruct(serviceName, structName, methods, unavailable))
  }

  // Bind the members of a service or live object type. Methods keep their
  // sync / async shape; with `properties`, data properties become live
  // getters. Members that cannot be bound are reported, not dropped silently.
  function bindMembers(type, label, rustOwner, { properties }) {
    const methods = [], getters = [], unavailable = []
    const rustNames = new Set()
    for (const member of checker.getPropertiesOfType(type)) {
      const declaration = member.valueDeclaration ?? member.declarations?.[0]
      if (!declaration || isCordis(declaration)) continue
      const flags = ts.getCombinedModifierFlags(declaration)
      if (flags & (ts.ModifierFlags.Private | ts.ModifierFlags.Protected) || (declaration.name && ts.isPrivateIdentifier(declaration.name))) continue
      const memberName = member.getName()
      if (memberName.startsWith('_') || memberName.startsWith('__@')) continue
      const skip = reason => {
        unavailable.push([memberName, reason])
        diagnostics.push(`${location(declaration)}: ${label}.${memberName} is not bound: ${reason}`)
      }
      const memberType = checker.getTypeOfSymbolAtLocation(member, declaration)
      const signatures = memberType.getCallSignatures()
      const hint = `${rustOwner}${pascal(memberName)}`
      const saved = mapping
      mapping = `${location(declaration)}: ${label}.${memberName}`
      try {
        const rustName = ident(snake(memberName))
        if (rustNames.has(rustName)) throw new Unsupported(`member name collides with another as ${rustName}`)
        if (!signatures.length) {
          if (!properties) throw new Unsupported('property (properties are not bound yet)')
          const { members } = stripNullish(memberType)
          if (members.some(member => member.getCallSignatures().length)) throw new Unsupported('function-valued property')
          getters.push({ name: memberName, rustName, result: rust(memberType, hint) })
          rustNames.add(rustName)
          continue
        }
        if (signatures.length !== 1) throw new Unsupported('overloaded method')
        const signature = signatures[0]
        if (signature.typeParameters?.length) throw new Unsupported('generic method')
        const params = signature.parameters.map((parameter, index) => {
          const parameterDeclaration = parameter.valueDeclaration
          if (parameterDeclaration?.dotDotDotToken) throw new Unsupported('rest parameter')
          const parameterType = checker.getTypeOfSymbolAtLocation(parameter, parameterDeclaration)
          const optional = !!(parameterDeclaration?.questionToken || parameterDeclaration?.initializer)
          // An AbortSignal is not a Rust parameter: the method receives one
          // that aborts when the returned future is dropped (a cancellation).
          const { members } = stripNullish(parameterType)
          if (members.length === 1 && members[0].getSymbol()?.getName() === 'AbortSignal') {
            return { name: ident(snake(parameter.name)), js: parameter.name, signal: true, index }
          }
          // A function parameter takes a Rust closure; Cordis calls it back.
          if (members.length === 1 && members[0].getCallSignatures().length) {
            if (optional || stripNullish(parameterType).optional) throw new Unsupported('optional callback parameter')
            return { name: ident(snake(parameter.name)), js: parameter.name, callback: callbackSignature(members[0], `${hint}${pascal(parameter.name)}`), index }
          }
          let rustType = rust(parameterType, `${hint}${pascal(parameter.name)}`)
          if (optional && !rustType.startsWith('Option<')) rustType = `Option<${rustType}>`
          return { name: ident(snake(parameter.name)), js: parameter.name, rustType, index }
        })
        const returned = checker.getReturnTypeOfSignature(signature)
        const promised = checker.getPromisedTypeOfPromise(returned)
        const awaited = promised ?? returned
        // A returned function (e.g. a disposer) stays on the Cordis side.
        const result = !awaited.isUnion() && awaited.getCallSignatures().length
          ? '::rutis_interop::RemoteFunction'
          : rust(awaited, `${hint}Result`)
        rustNames.add(rustName)
        methods.push({ name: memberName, rustName, params, async: !!promised, result })
      } catch (error) {
        if (!(error instanceof Unsupported)) throw error
        skip(error.message)
      } finally {
        mapping = saved
      }
    }
    return { methods, getters, unavailable }
  }

  // The Rust closure type for a callback parameter. Arguments decode like
  // results; functions and AbortSignals the callback receives stay raw
  // protocol values. `void | Promise<void>` callbacks are asynchronous.
  function callbackSignature(type, hint) {
    const signatures = type.getCallSignatures()
    if (signatures.length !== 1) throw new Unsupported('overloaded callback')
    const signature = signatures[0]
    if (signature.typeParameters?.length) throw new Unsupported('generic callback')
    const params = signature.parameters.map(parameter => {
      const declaration = parameter.valueDeclaration
      if (declaration?.dotDotDotToken) throw new Unsupported('rest parameter in callback')
      const parameterType = checker.getTypeOfSymbolAtLocation(parameter, declaration)
      const { members } = stripNullish(parameterType)
      if (members.length === 1 && (members[0].getCallSignatures().length || members[0].getSymbol()?.getName() === 'AbortSignal')) {
        return { rustType: '::rutis_interop::rpc::Value', raw: true }
      }
      let rustType = rust(parameterType, `${hint}${pascal(parameter.name)}`)
      if (declaration?.questionToken && !rustType.startsWith('Option<')) rustType = `Option<${rustType}>`
      return { rustType }
    })
    const returned = checker.getReturnTypeOfSignature(signature)
    const parts = returned.isUnion() ? returned.types : [returned]
    const promise = parts.find(part => checker.getPromisedTypeOfPromise(part))
    const awaited = promise ? checker.getPromisedTypeOfPromise(promise) : returned
    if (promise && parts.some(part => part !== promise && !(part.flags & (ts.TypeFlags.Void | ts.TypeFlags.Undefined)))) {
      throw new Unsupported('callback returning a value or a Promise')
    }
    const nothing = awaited.flags & (ts.TypeFlags.Void | ts.TypeFlags.Undefined | ts.TypeFlags.Never)
    const result = nothing ? '()' : rust(awaited, `${hint}Result`)
    const output = promise
      ? `::rutis::BoxFuture<'static, Result<${result}, ::rutis_interop::Error>>`
      : `Result<${result}, ::rutis_interop::Error>`
    return { params, async: !!promise, result, rustFn: `impl Fn(${params.map(param => param.rustType).join(', ')}) -> ${output} + Send + Sync + 'static` }
  }

  // Wrap a Rust closure as a protocol callback the Cordis side can call.
  function callbackValue(name, callback) {
    const decode = callback.params.map((param, index) => param.raw
      ? `let a${index} = args.next().unwrap_or(::rutis_interop::rpc::Value::Undefined);`
      : `let a${index}: ${param.rustType} = ::rutis_interop::decode_value(args.next().unwrap_or(::rutis_interop::rpc::Value::Undefined))?;`).join(' ')
    const values = callback.params.map((_, index) => `a${index}`).join(', ')
    const encode = callback.result === '()' ? 'Ok(::rutis_interop::rpc::Value::Undefined)' : '::rutis_interop::arg(&result)'
    const call = callback.async
      ? `let pending = ${name}(${values}); Ok(::rutis_interop::rpc::Value::future(async move { #[allow(unused_variables)] let result = pending.await?; ${encode} }))`
      : `#[allow(unused_variables)] let result = ${name}(${values})?; ${encode}`
    return `let ${name} = ::rutis_interop::rpc::Value::callback(move |args| {
          #[allow(unused_mut, unused_variables)]
          let mut args = args.list()?.into_iter();
          ${decode}
          ${call}
        });`
  }

  // One generated method; `target(method, args)` is the call expression.
  function methodCode(label, method, target) {
    const exposed = method.params.filter(parameter => !parameter.signal)
    const signature = exposed.map(parameter => `${parameter.name}: ${parameter.callback ? parameter.callback.rustFn : borrowed(parameter.rustType)}`).join(', ')
    const callbacks = exposed.filter(parameter => parameter.callback).map(parameter => callbackValue(parameter.name, parameter.callback)).join('\n')
    const args = method.params.map(parameter => parameter.signal
      ? '::rutis_interop::rpc::Value::Signal'
      : parameter.callback ? parameter.name
      : parameter.rustType.startsWith('Option<')
        ? `::rutis_interop::optional(${parameter.name})?`
        : `::rutis_interop::arg(&${parameter.name})?`).join(', ')
    const cancellable = method.params.some(parameter => parameter.signal)
    const note = !cancellable ? ''
      : method.async ? '\n      ///\n      /// Cancellable: dropping the returned future (for example on a timeout)\n      /// aborts the AbortSignal the Cordis method receives.'
        : '\n      ///\n      /// The Cordis method receives an AbortSignal that is never aborted: a\n      /// synchronous call cannot be cancelled.'
    return `/// Calls \`${label}.${method.name}\` on the Cordis side.${note}
      pub ${method.async ? 'async ' : ''}fn ${method.rustName}(&self${signature ? ', ' + signature : ''}) -> Result<${method.result}, ::rutis_interop::Error> {
        ${exposed.filter(parameter => !parameter.callback).map(parameter => finite(parameter.name, parameter.rustType)).join('\n')}
        ${callbacks}
        ::rutis_interop::decode_value(${target(method, `vec![${args}]`)})
      }`
  }

  function unboundDocs(unavailable) {
    return unavailable.length
      ? `///\n/// Members not bound yet:\n${unavailable.map(([name, reason]) => `/// - \`${name}\`: ${reason}`).join('\n')}\n`
      : ''
  }

  // An object with methods, or a class instance, crosses by reference: the
  // proxy reads its properties live and calls its methods on the original.
  function liveObject(type, hint) {
    if (named.has(type)) return named.get(type)
    const label = typeName(type, hint)
    const name = claim(label)
    named.set(type, name)
    const { methods, getters, unavailable } = bindMembers(type, label, name, { properties: true })
    const getterCode = getters.map(getter => `/// Reads \`${label}.${getter.name}\` from the live object.
      pub fn ${getter.rustName}(&self) -> Result<${getter.result}, ::rutis_interop::Error> {
        ::rutis_interop::decode_value(self.0.get(${literal(getter.name)})?)
      }`).join('\n')
    const code = methods.map(method => methodCode(label, method, (method, args) => method.async
      ? `self.0.call_async(${literal(method.name)}, ${args}).await?`
      : `self.0.call(${literal(method.name)}, ${args})?`)).join('\n')
    items.push(`/// Live Cordis object \`${label}\`: property reads and method calls reach the
    /// original object; passing it back hands Cordis that same object. Two
    /// proxies are equal when they address the same object.
    ${unboundDocs(unavailable)}#[derive(Debug, Clone, PartialEq, ::rutis_interop::serde::Serialize, ::rutis_interop::serde::Deserialize)]
    #[serde(crate = "rutis_interop::serde", transparent)]
    pub struct ${name}(pub ::rutis_interop::ObjectRef);
    impl ${name} { ${getterCode}
    ${code} }`)
    return name
  }

  function borrowed(rustType) {
    if (rustType === 'String') return '&str'
    if (rustType === 'f64' || rustType === 'bool') return rustType
    if (rustType.startsWith('Vec<')) return `&[${rustType.slice(4, -1)}]`
    if (rustType.startsWith('Option<')) {
      const inner = rustType.slice(7, -1)
      return inner === 'f64' || inner === 'bool' ? rustType : `Option<${borrowed(inner).replace(/^&?/, '&')}>`
    }
    return `&${rustType}`
  }
  function finite(name, rustType) {
    if (rustType === 'f64') return `if !${name}.is_finite() { return Err(::rutis_interop::Error::Value("non-finite number".into())); }`
    if (rustType === 'Option<f64>') return `if ${name}.is_some_and(|value| !value.is_finite()) { return Err(::rutis_interop::Error::Value("non-finite number".into())); }`
    if (rustType === 'Vec<f64>') return `if ${name}.iter().any(|value| !value.is_finite()) { return Err(::rutis_interop::Error::Value("non-finite number".into())); }`
    return ''
  }
  function serviceStruct(serviceName, structName, methods, unavailable) {
    const code = methods.map(method => methodCode(serviceName, method, (method, args) => method.async
      ? `self.process.invoke_async(&self.handle, ${literal(method.name)}, ${args}).await?`
      : `self.process.invoke(&self.handle, ${literal(method.name)}, ${args})?`)).join('\n')
    const missing = unboundDocs(unavailable)
    // One proxy per handle: it keeps addressing the object it was created
    // for, and releases that object when the last Arc snapshot is dropped.
    return `/// Native proxy for the Cordis service \`ctx.${serviceName}\`.
    ${missing}pub struct ${structName} { process: ::std::sync::Arc<::rutis_interop::Process>, handle: String }
    impl ${structName} { ${code} }
    impl Drop for ${structName} { fn drop(&mut self) { self.process.release(&self.handle); } }`
  }

  // ---------------------------------------------------------------------
  // Host-provided services: a trait the rutis application implements, a
  // dispatcher for calls from Node, and a registration helper.
  // ---------------------------------------------------------------------
  const hostCode = []
  const hosts = [] // { name, trait, dispatch, manifest }
  for (const [serviceName, { type }] of provided) {
    const traitName = claim(`${typeName(type, serviceName)}Host`)
    const dispatchName = claim(`${traitName}Dispatch`)
    const methods = [], unavailable = []
    const rustNames = new Set()
    for (const member of checker.getPropertiesOfType(type)) {
      const declaration = member.valueDeclaration ?? member.declarations?.[0]
      if (!declaration || isCordis(declaration)) continue
      const flags = ts.getCombinedModifierFlags(declaration)
      if (flags & (ts.ModifierFlags.Private | ts.ModifierFlags.Protected) || (declaration.name && ts.isPrivateIdentifier(declaration.name))) continue
      const memberName = member.getName()
      if (memberName.startsWith('_') || memberName.startsWith('__@')) continue
      const skip = reason => {
        unavailable.push([memberName, reason])
        diagnostics.push(`${location(declaration)}: host ${serviceName}.${memberName} is not bound: ${reason}`)
      }
      const signatures = checker.getTypeOfSymbolAtLocation(member, declaration).getCallSignatures()
      if (!signatures.length) { skip('property (properties are not bound yet)'); continue }
      if (signatures.length !== 1) { skip('overloaded method'); continue }
      const signature = signatures[0]
      if (signature.typeParameters?.length) { skip('generic method'); continue }
      const hint = `${traitName}${pascal(memberName)}`
      mapping = `${location(declaration)}: host ${serviceName}.${memberName}`
      try {
        const rustName = ident(snake(memberName))
        if (rustNames.has(rustName)) throw new Unsupported(`method name collides with another as ${rustName}`)
        const params = signature.parameters.map(parameter => {
          const parameterDeclaration = parameter.valueDeclaration
          if (parameterDeclaration?.dotDotDotToken) throw new Unsupported('rest parameter')
          const parameterType = checker.getTypeOfSymbolAtLocation(parameter, parameterDeclaration)
          const optional = !!(parameterDeclaration?.questionToken || parameterDeclaration?.initializer)
          const { optional: nullable, members } = stripNullish(parameterType)
          const name = ident(snake(parameter.name))
          if ((optional || nullable) && members.length === 1 && members[0].getSymbol()?.getName() === 'AbortSignal') return { name, omitted: true }
          // A function argument arrives as a callable protocol reference.
          if (members.length === 1 && members[0].getCallSignatures().length) {
            return { name, raw: true, rustType: '::rutis_interop::rpc::Value' }
          }
          let rustType = rust(parameterType, `${hint}${pascal(parameter.name)}`)
          if (optional && !rustType.startsWith('Option<')) rustType = `Option<${rustType}>`
          return { name, rustType }
        })
        const returned = checker.getReturnTypeOfSignature(signature)
        const promised = checker.getPromisedTypeOfPromise(returned)
        const awaited = promised ?? returned
        // Returning a function (e.g. a disposer) means building a callable
        // protocol value, e.g. `rpc::Value::callback(...)`.
        const rawResult = !awaited.isUnion() && awaited.getCallSignatures().length > 0
        const result = rawResult ? '::rutis_interop::rpc::Value' : rust(awaited, `${hint}Result`)
        rustNames.add(rustName)
        methods.push({ name: memberName, rustName, params, async: !!promised, result, rawResult })
      } catch (error) {
        if (!(error instanceof Unsupported)) throw error
        skip(error.message)
      } finally {
        mapping = undefined
      }
    }
    hosts.push({ name: serviceName, trait: traitName, dispatch: dispatchName, manifest: Object.fromEntries(methods.map(method => [method.name, method.async ? 'async' : 'sync'])) })
    hostCode.push(hostTrait(serviceName, traitName, dispatchName, methods, unavailable))
  }

  function hostTrait(serviceName, traitName, dispatchName, methods, unavailable) {
    const unimplemented = method => `::rutis_interop::Error::Value(${literal(`${serviceName}.${method.name} is not implemented by the rutis host`)}.into())`
    const signatureOf = method => method.params.filter(parameter => !parameter.omitted).map(parameter => `${parameter.name}: ${parameter.rustType}`).join(', ')
    const declarations = methods.map(method => {
      const signature = signatureOf(method)
      return method.async
        ? `fn ${method.rustName}(&self${signature ? ', ' + signature : ''}) -> ::rutis::BoxFuture<'static, Result<${method.result}, ::rutis_interop::Error>> { Box::pin(async { Err(${unimplemented(method)}) }) }`
        : `fn ${method.rustName}(&self${signature ? ', ' + signature : ''}) -> Result<${method.result}, ::rutis_interop::Error> { Err(${unimplemented(method)}) }`
    }).join('\n')
    const encode = method => method.rawResult ? 'Ok(result)'
      : method.result === '()' ? 'Ok(::rutis_interop::rpc::Value::Undefined)'
        : '::rutis_interop::arg(&result)'
    const arms = methods.map(method => {
      const decode = method.params.map(parameter => parameter.omitted ? 'args.next();'
        : parameter.raw ? `let ${parameter.name} = args.next().unwrap_or(::rutis_interop::rpc::Value::Undefined);`
          : `let ${parameter.name}: ${parameter.rustType} = ::rutis_interop::decode_value(args.next().unwrap_or(::rutis_interop::rpc::Value::Undefined))?;`).join('\n')
      const call = `${method.rustName}(${method.params.filter(parameter => !parameter.omitted).map(parameter => parameter.name).join(', ')})`
      return method.async
        ? `${literal(method.name)} => { ${decode} let host = self.0.clone(); Ok(::rutis_interop::rpc::Value::future(async move { let result = host.${call}.await?; ${encode(method)} })) }`
        : `${literal(method.name)} => { ${decode} let result = self.0.${call}?; ${encode(method)} }`
    }).join('\n')
    const missing = unavailable.length
      ? `///\n/// Members not bound yet (calls from Cordis report an error):\n${unavailable.map(([name, reason]) => `/// - \`${name}\`: ${reason}`).join('\n')}\n`
      : ''
    return `/// The rutis implementation of the Cordis service \`ctx.${serviceName}\` that the
    /// mounted plugins depend on. Every method defaults to an error: implement
    /// the ones the plugins use, then register it with [\`provide_${snake(serviceName)}\`].
    ${missing}#[allow(unused_variables)]
    pub trait ${traitName}: Send + Sync + 'static { ${declarations} }
    /// Serves calls from the Cordis side to a [\`${traitName}\`].
    pub struct ${dispatchName}(pub ::std::sync::Arc<dyn ${traitName}>);
    impl ::rutis_interop::HostDispatch for ${dispatchName} {
      fn invoke(&self, method: &str, args: ::rutis_interop::rpc::Value) -> ::rutis_interop::rpc::Reply {
        #[allow(unused_mut, unused_variables)]
        let mut args = args.list()?.into_iter();
        match method { ${arms}
          _ => Err(::rutis_interop::Error::Value(format!("${serviceName}.{method} is not bound"))),
        }
      }
    }
    /// Register \`host\` as the rutis provider of \`ctx.${serviceName}\` for mounted Cordis plugins.
    pub fn provide_${snake(serviceName)}(ctx: &::rutis::Ctx, host: impl ${traitName}) -> Result<::rutis::Disposer, ::rutis::CordisError> {
      ctx.provide_as::<dyn ${traitName}>(::rutis::TypeKey::of::<dyn ${traitName}>(), ::std::sync::Arc::new(host))
    }`
  }

  // ---------------------------------------------------------------------
  // Configuration.
  // ---------------------------------------------------------------------
  function configFor(configType, structName, accessor) {
    const empty = { code: `#[derive(Debug, Clone, Default, ::rutis_interop::serde::Serialize)]\n#[serde(crate = "rutis_interop::serde")]\npub struct ${structName} {}`, defaultable: true, checks: [] }
    if (!configType) return empty
    const stripped = stripNullish(configType).members
    const configObject = stripped.length === 1 ? stripped[0] : undefined
    if (!configObject || !(configObject.flags & ts.TypeFlags.Object) || checker.isArrayType(configObject)) {
      return { code: `pub type ${structName} = ::rutis_interop::serde_json::Value;`, defaultable: true, checks: [] }
    }
    const fields = [], checks = []
    let defaultable = true
    for (const property of checker.getPropertiesOfType(configObject)) {
      const declaration = property.valueDeclaration ?? property.declarations?.[0]
      const field = ident(snake(property.getName()))
      let fieldType
      try {
        fieldType = rust(checker.getTypeOfSymbolAtLocation(property, declaration), `${structName}${pascal(property.getName())}`)
      } catch (error) {
        if (!(error instanceof Unsupported)) throw error
        fieldType = '::rutis_interop::serde_json::Value'
        diagnostics.push(`${location(declaration)}: config.${property.getName()} is dynamic JSON: ${error.message}`)
      }
      const optional = !!(property.flags & ts.SymbolFlags.Optional)
      if (optional && !fieldType.startsWith('Option<')) fieldType = `Option<${fieldType}>`
      if (!fieldType.startsWith('Option<')) defaultable = false
      const attributes = [`rename = ${literal(property.getName())}`]
      if (fieldType.startsWith('Option<')) attributes.push('skip_serializing_if = "Option::is_none"')
      fields.push(`#[serde(${attributes.join(', ')})] pub ${field}: ${fieldType},`)
      const check = finite(`${accessor}.${field}`, fieldType)
      if (check) checks.push(check.replace('::rutis_interop::Error::Value("non-finite number".into())', '::rutis_interop::Error::Value("non-finite number".into()).into()'))
    }
    return {
      code: `#[derive(Debug, Clone, ${defaultable ? 'Default, ' : ''}::rutis_interop::serde::Serialize)]\n#[serde(crate = "rutis_interop::serde")]\npub struct ${structName} { ${fields.join('\n')} }`,
      defaultable, checks,
    }
  }
  let configCode, configChecks, launched
  const toValue = accessor => `::rutis_interop::serde_json::to_value(&${accessor}).map_err(|e| ::rutis::CordisError::PluginFailed(Box::new(e)))?`
  if (single) {
    const config = configFor(group[0].configType, 'Config', 'self.config')
    configCode = config.code
    configChecks = config.checks.join('\n')
    launched = [`(::std::path::Path::new(${literal(group[0].entry)}), ${toValue('self.config')})`]
  } else {
    // One Config field per group member, each with that plugin's own type.
    const parts = group.map(plugin => ({ plugin, ...configFor(plugin.configType, claim(`${plugin.name}Config`), `self.config.${plugin.name}`) }))
    parts.forEach(part => { part.structName = part.code.match(/pub (?:struct|type) (\w+)/)[1] })
    const defaultable = parts.every(part => part.defaultable)
    configCode = `${parts.map(part => part.code).join('\n')}
  /// Configuration for each plugin of the group, in load order.
  #[derive(Debug, Clone${defaultable ? ', Default' : ''})]
  pub struct Config { ${parts.map(part => `pub ${part.plugin.name}: ${part.structName},`).join(' ')} }`
    configChecks = parts.flatMap(part => part.checks).join('\n')
    launched = group.map(plugin => `(::std::path::Path::new(${literal(plugin.entry)}), ${toValue(`self.config.${plugin.name}`)})`)
  }

  const serviceNames = [...services.keys()]
  const label = serviceNames.length ? serviceNames.join(',') : group.map(plugin => basename(plugin.entry)).join(',')
  const hostKeys = hosts.map(host => `::rutis::TypeKey::of::<dyn ${host.trait}>()`)
  const rust_ = `// Generated from the original Cordis plugin. Do not edit.
  ${configCode}
  ${items.join('\n')}
  ${serviceCode.join('\n')}
  ${hostCode.join('\n')}
  pub struct Plugin { config: Config, injects: Vec<::rutis::TypeKey> }
  impl Plugin { pub fn new(config: Config) -> Self { Self { config, injects: vec![${hostKeys.join(', ')}] } } }
  impl ::rutis::Plugin for Plugin {
    fn name(&self) -> &str { "cordis:${label}" }
    /// Host-provided services: the mount waits for them natively.
    fn injects(&self) -> &[::rutis::TypeKey] { &self.injects }
    fn validate(&self) -> Result<(), ::rutis::CordisError> {
      ${configChecks}
      Ok(())
    }
    fn apply<'a>(&'a self, ctx: &'a ::rutis::Ctx) -> ::rutis::BoxFuture<'a, Result<::rutis::Effect, ::rutis::CordisError>> {
      Box::pin(async move {
        let projection = ::rutis_interop::Projection::new();
        ${[...structs].map(([name, struct]) => `projection.service::<${struct}>(${literal(name)}, |process, handle| ${struct} { process, handle });`).join('\n')}
        let hosts = vec![${hosts.map(host => `::rutis_interop::Host {
          name: ${literal(host.name)}.into(),
          methods: ::rutis_interop::serde_json::json!(${JSON.stringify(host.manifest)}),
          dispatch: ::std::sync::Arc::new(${host.dispatch}(ctx.require_as::<dyn ${host.trait}>(::rutis::TypeKey::of::<dyn ${host.trait}>())?)),
        }`).join(', ')}];
        let process = ::rutis_interop::Process::launch_mount(
          ::std::path::Path::new(${literal(nodePackage)}),
          &[${launched.join(',\n            ')}],
          ::rutis_interop::serde_json::json!(${JSON.stringify(manifest)}),
          Some(projection.clone()),
          hosts,
        ).await?;
        // Registered before any service binding, so native cleanup withdraws
        // the services and runs their consumers' disposers first.
        let owner = process.clone();
        let followed = projection.clone();
        ctx.effect(move || ::rutis::Effect::AsyncDisposer(Box::new(move || Box::pin(async move {
          followed.close();
          owner.dispose().await.map_err(Into::into)
        }))))?;
        projection.attach(ctx, process)?;
        Ok(::rutis::Effect::Done)
      })
    }
  }
  `
  return { rust: rust_, inputs: program.getSourceFiles().map(file => file.fileName), diagnostics }
}

// `node generate.mjs <node package> [--provide=<service>...] <plugin>` or,
// for a group, `... <name>=<plugin> ...`.
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const [nodePackage, ...rest] = process.argv.slice(2)
    const provide = rest.filter(arg => arg.startsWith('--provide=')).map(arg => arg.slice('--provide='.length))
    const members = rest.filter(arg => !arg.startsWith('--provide='))
    const plugins = members.length === 1 && !members[0].includes('=')
      ? resolve(members[0])
      : members.map(member => { const at = member.indexOf('='); return { name: member.slice(0, at), path: resolve(member.slice(at + 1)) } })
    process.stdout.write(JSON.stringify(generate(plugins, resolve(nodePackage), { provide })))
  } catch (error) { console.error(error.message); process.exitCode = 1 }
}
