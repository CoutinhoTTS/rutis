import ts from 'typescript'
import { fileURLToPath } from 'node:url'
import { resolve } from 'node:path'

const keywords = new Set('as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while'.split(' '))
function ident(name) {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name) || ['self', 'Self', 'super', 'crate'].includes(name)) {
    throw new Error(`cannot represent identifier ${JSON.stringify(name)} in Rust`)
  }
  return keywords.has(name) ? `r#${name}` : name
}
const snake = name => name.replace(/[A-Z]/g, letter => `_${letter.toLowerCase()}`)
const literal = value => JSON.stringify(value)

export function generate(pluginFile, nodePackage) {
  const program = ts.createProgram([pluginFile], {
    target: ts.ScriptTarget.ESNext, module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext,
    strict: true, skipLibCheck: true, noEmit: true,
  })
  const diagnostics = ts.getPreEmitDiagnostics(program)
  if (diagnostics.length) {
    throw new Error(ts.formatDiagnosticsWithColorAndContext(diagnostics, {
      getCanonicalFileName: value => value, getCurrentDirectory: () => process.cwd(), getNewLine: () => '\n',
    }))
  }
  const checker = program.getTypeChecker()
  const source = program.getSourceFile(pluginFile)
  if (!source) throw new Error(`source not found: ${pluginFile}`)
  function fail(node, reason) {
    const { line, character } = node.getSourceFile().getLineAndCharacterOfPosition(node.getStart())
    throw new Error(`${node.getSourceFile().fileName}:${line + 1}:${character + 1}: ${reason}`)
  }
  function rustType(type, node) {
    if (type.flags & ts.TypeFlags.NumberLike) return 'f64'
    if (type.flags & ts.TypeFlags.StringLike) return 'String'
    if (type.flags & ts.TypeFlags.BooleanLike) return 'bool'
    if (type.flags & ts.TypeFlags.Void) return '()'
    if (checker.isArrayType(type)) return `Vec<${rustType(checker.getTypeArguments(type)[0], node)}>`
    return fail(node, `binding not implemented for ${checker.typeToString(type)}`)
  }
  const services = new Map()
  function visit(node) {
    if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression) && node.expression.name.text === 'provide') {
      const declaration = checker.getResolvedSignature(node)?.declaration
      if (declaration?.getSourceFile().fileName.replaceAll('\\', '/').includes('/@deepseek-ai/cordis/')) {
        const [name, value] = node.arguments
        if (!name || !ts.isStringLiteral(name) || !value) fail(node, 'service discovery requires a literal native service name and a value')
        const type = checker.getTypeAtLocation(value)
        if (services.has(name.text)) fail(node, `multiple declarations for service ${name.text} require further scope analysis`)
        const methods = []
        for (const member of checker.getPropertiesOfType(type)) {
          const decl = member.valueDeclaration ?? member.declarations?.[0]
          if (!decl) continue
          const flags = ts.getCombinedModifierFlags(decl)
          if (flags & (ts.ModifierFlags.Private | ts.ModifierFlags.Protected) || ts.isPrivateIdentifier(decl.name)) continue
          if (!(ts.isMethodDeclaration(decl) || ts.isMethodSignature(decl))) {
            fail(decl, `property binding not implemented for ${member.name}`)
          }
          const signatures = checker.getTypeOfSymbolAtLocation(member, decl).getCallSignatures()
          if (signatures.length !== 1) fail(decl, 'overloaded methods require further binding support')
          const signature = signatures[0]
          if (signature.typeParameters?.length) fail(decl, 'generic methods require further binding support')
          const params = signature.parameters.map(parameter => {
            const declaration = parameter.valueDeclaration
            if (!declaration || declaration.questionToken || declaration.dotDotDotToken || declaration.initializer) {
              fail(decl, 'optional, default and rest parameters require further binding support')
            }
            return { name: ident(parameter.name), type: rustType(checker.getTypeOfSymbolAtLocation(parameter, declaration), declaration) }
          })
          const result = checker.getReturnTypeOfSignature(signature)
          const promised = checker.getPromisedTypeOfPromise(result)
          methods.push({ name: member.name, rustName: ident(snake(member.name)), params, async: !!promised, result: rustType(promised ?? result, decl) })
        }
        if (!methods.length) fail(node, `no public methods found for ${name.text}`)
        if (new Set(methods.map(method => method.rustName)).size !== methods.length) fail(node, 'method names collide in Rust')
        services.set(name.text, { name: name.text, type: ident(name.text[0].toUpperCase() + name.text.slice(1)), methods })
      }
    }
    ts.forEachChild(node, visit)
  }
  visit(source)
  if (!services.size) throw new Error('no native Cordis service registrations found')
  const exports = checker.getExportsOfModule(checker.getSymbolAtLocation(source))
  const apply = exports.find(symbol => symbol.name === 'apply')
  if (!apply) throw new Error('this initial binding generator requires a native apply entrypoint')
  const applyType = checker.getTypeOfSymbolAtLocation(apply, apply.valueDeclaration)
  const config = applyType.getCallSignatures()[0]?.parameters[1]
  const configType = config && checker.getTypeOfSymbolAtLocation(config, config.valueDeclaration)
  const configFields = configType ? checker.getPropertiesOfType(configType) : []
  const fields = configFields.map(field => {
    const decl = field.valueDeclaration ?? field.declarations[0]
    if (field.flags & ts.SymbolFlags.Optional) fail(decl, 'optional configuration fields require further binding support')
    return `pub ${ident(field.name)}: ${rustType(checker.getTypeOfSymbolAtLocation(field, decl), decl)},`
  }).join('\n')
  const methodManifest = Object.fromEntries([...services.values()].map(service => [service.name, service.methods.map(method => method.name)]))
  function validateNumber(name, type, cordisError = false) {
    if (type === 'f64') return `if !${name}.is_finite() { return Err(::rutis_interop::Error::Value("non-finite number".into())${cordisError ? '.into()' : ''}); }`
    if (type.startsWith('Vec<') && type.includes('f64')) return `for value in ${name}.iter() { ${validateNumber('value', type.slice(4, -1), cordisError)} }`
    return ''
  }
  const serviceCode = [...services.values()].map(service => {
    const methods = service.methods.map(method => {
      const args = method.params.map(parameter => `${parameter.name}: ${parameter.type}`).join(', ')
      const values = method.params.map(parameter => parameter.name).join(', ')
      return `pub ${method.async ? 'async ' : ''}fn ${method.rustName}(&self${args ? ', ' + args : ''}) -> Result<${method.result}, ::rutis_interop::Error> {
        ${method.params.map(parameter => validateNumber(parameter.name, parameter.type)).join('\n')}
        ::rutis_interop::decode(self.process.${method.async ? 'call_async' : 'call'}(&self.handle, ${literal(method.name)}, ::rutis_interop::serde_json::json!([${values}]))${method.async ? '.await' : ''}?)
      }`
    }).join('\n')
    // One proxy per handle: it keeps addressing the object it was created
    // for, and releases that object when the last Arc snapshot is dropped.
    return `pub struct ${service.type} { process: ::std::sync::Arc<::rutis_interop::Process>, handle: String }
    impl ${service.type} { ${methods} }
    impl Drop for ${service.type} { fn drop(&mut self) { self.process.release(&self.handle); } }`
  }).join('\n')
  const rust = `// Generated from the original Cordis plugin. Do not edit.
  #[derive(Clone, ::rutis_interop::serde::Serialize)]
  #[serde(crate = "rutis_interop::serde")]
  pub struct Config { ${fields} }
  ${serviceCode}
  pub struct Plugin { config: Config }
  impl Plugin { pub fn new(config: Config) -> Self { Self { config } } }
  impl ::rutis::Plugin for Plugin {
    fn name(&self) -> &str { "cordis:${[...services.keys()].join(',')}" }
    fn validate(&self) -> Result<(), ::rutis::CordisError> {
      ${configFields.map(field => {
        const decl = field.valueDeclaration ?? field.declarations[0]
        return validateNumber(`self.config.${ident(field.name)}`, rustType(checker.getTypeOfSymbolAtLocation(field, decl), decl), true)
      }).join('\n')}
      Ok(())
    }
    fn apply<'a>(&'a self, ctx: &'a ::rutis::Ctx) -> ::rutis::BoxFuture<'a, Result<::rutis::Effect, ::rutis::CordisError>> {
      Box::pin(async move {
        let projection = ::rutis_interop::Projection::new();
        ${[...services.values()].map(service => `projection.service::<${service.type}>(${literal(service.name)}, |process, handle| ${service.type} { process, handle });`).join('\n')}
        let process = ::rutis_interop::Process::launch_observed(
          ::std::path::Path::new(${literal(nodePackage)}), ::std::path::Path::new(${literal(pluginFile)}),
          ::rutis_interop::serde_json::to_value(&self.config).map_err(|e| ::rutis::CordisError::PluginFailed(Box::new(e)))?,
          ::rutis_interop::serde_json::json!(${JSON.stringify(methodManifest)}),
          Some(projection.clone()),
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
  return { rust, inputs: program.getSourceFiles().map(source => source.fileName) }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { process.stdout.write(JSON.stringify(generate(resolve(process.argv[2]), resolve(process.argv[3])))) }
  catch (error) { console.error(error.message); process.exitCode = 1 }
}
