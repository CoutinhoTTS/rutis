use std::collections::BTreeSet;
use std::path::Path;

use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{FnArg, GenericArgument, ImplItem, Item, PathArguments, ReturnType, Type, Visibility};

type Result<T> = std::result::Result<T, syn::Error>;

fn unsupported(node: &impl ToTokens, reason: &str) -> syn::Error {
    syn::Error::new_spanned(node, reason)
}

fn camel(name: &str) -> String {
    let mut words = name.split('_');
    let mut result = words.next().unwrap_or_default().to_owned();
    for word in words {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            result.extend(first.to_uppercase());
            result.extend(chars);
        }
    }
    result
}

fn ts_type(ty: &Type) -> Result<String> {
    match ty {
        Type::Tuple(tuple) if tuple.elems.is_empty() => Ok("void".into()),
        Type::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
            let segment = &path.path.segments[0];
            match segment.ident.to_string().as_str() {
                "f64" => Ok("number".into()),
                "bool" => Ok("boolean".into()),
                "String" => Ok("string".into()),
                "Vec" => match &segment.arguments {
                    PathArguments::AngleBracketed(args) if args.args.len() == 1 => {
                        match &args.args[0] {
                            GenericArgument::Type(inner) => Ok(format!("{}[]", ts_type(inner)?)),
                            _ => Err(unsupported(ty, "unsupported Vec element")),
                        }
                    }
                    _ => Err(unsupported(ty, "unsupported Vec type")),
                },
                _ => Err(unsupported(ty, "type binding not implemented")),
            }
        }
        _ => Err(unsupported(ty, "type binding not implemented")),
    }
}

fn validate(value: &str, ty: &str) -> String {
    if ty == "number" {
        format!("if !{value}.is_finite() {{ return Err(::rutis_interop::Error::Value(\"non-finite number\".into())); }}")
    } else if let Some(inner) = ty.strip_suffix("[]") {
        format!(
            "for item in {value}.iter() {{ {} }}",
            validate("item", inner)
        )
    } else {
        String::new()
    }
}

#[derive(Default)]
struct Registrations {
    services: BTreeSet<String>,
    error: Option<syn::Error>,
}

impl<'ast> Visit<'ast> for Registrations {
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == "provide" {
            let ty = match node.args.first() {
                Some(syn::Expr::Call(call)) => match call.func.as_ref() {
                    syn::Expr::Path(path) if path.path.segments.len() == 2 => {
                        Some(path.path.segments[0].ident.to_string())
                    }
                    _ => None,
                },
                Some(syn::Expr::Struct(value)) => value.path.get_ident().map(ToString::to_string),
                _ => None,
            };
            if let Some(ty) = ty {
                self.services.insert(ty);
            } else {
                self.error = Some(unsupported(
                    node,
                    "service discovery currently requires a concrete constructor or struct literal",
                ));
            }
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

fn generate(
    source: &str,
    crate_name: &str,
    node_package: &Path,
) -> Result<(String, String, String)> {
    let file = syn::parse_file(source)?;
    for item in &file.items {
        if matches!(item, Item::Mod(_) | Item::Macro(_)) {
            return Err(unsupported(
                item,
                "module and macro expansion are required before discovering this interface",
            ));
        }
    }
    let implementations: Vec<_> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(item)
                if item.trait_.as_ref().is_some_and(|(_, path, _)| {
                    path.segments.last().is_some_and(|s| s.ident == "Plugin")
                }) =>
            {
                Some(item)
            }
            _ => None,
        })
        .collect();
    if implementations.len() != 1 {
        return Err(syn::Error::new(
            file.span(),
            "expected one native Plugin implementation in the entry source",
        ));
    }
    let implementation = implementations[0];
    let plugin_name = implementation.self_ty.to_token_stream().to_string();
    let plugin = file
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == plugin_name => Some(item),
            _ => None,
        })
        .ok_or_else(|| {
            unsupported(
                implementation,
                "plugin must be a concrete struct in the entry source",
            )
        })?;
    if !plugin.generics.params.is_empty() || !matches!(plugin.vis, Visibility::Public(_)) {
        return Err(unsupported(plugin, "plugin must be public and non-generic"));
    }
    let mut config_rust = String::new();
    let mut config_ts = String::new();
    let mut config_fields = Vec::new();
    for field in &plugin.fields {
        let name = field
            .ident
            .as_ref()
            .ok_or_else(|| unsupported(field, "named configuration fields required"))?;
        if !matches!(field.vis, Visibility::Public(_)) {
            return Err(unsupported(
                field,
                "configuration construction requires public fields",
            ));
        }
        let ty = ts_type(&field.ty)?;
        let key = camel(&name.to_string());
        config_ts.push_str(&format!("{key}: {ty};\n"));
        config_rust.push_str(&format!("let {name}: {} = ::rutis_interop::decode(config.get({key:?}).cloned().ok_or_else(|| ::rutis_interop::Error::Value({key:?}.into()))?)?; {}\n", field.ty.to_token_stream(), validate(&name.to_string(), &ty)));
        config_fields.push(name.to_string());
    }
    let mut registrations = Registrations::default();
    registrations.visit_item_impl(implementation);
    if let Some(error) = registrations.error {
        return Err(error);
    }
    if registrations.services.is_empty() {
        return Err(unsupported(
            implementation,
            "no native service registrations found",
        ));
    }
    let mut arms = String::new();
    let mut classes = String::new();
    let mut declarations = String::new();
    let mut provide = String::new();
    let mut context = String::new();
    let mut require = String::new();
    let mut dependencies = Vec::new();
    for service in registrations.services {
        let declaration = file
            .items
            .iter()
            .find_map(|item| match item {
                Item::Struct(item) if item.ident == service => Some(item),
                _ => None,
            })
            .ok_or_else(|| {
                unsupported(
                    implementation,
                    "service must be declared in the entry source",
                )
            })?;
        if !matches!(declaration.vis, Visibility::Public(_))
            || !declaration.generics.params.is_empty()
        {
            return Err(unsupported(
                declaration,
                "service must be public and non-generic",
            ));
        }
        if declaration
            .fields
            .iter()
            .any(|field| matches!(field.vis, Visibility::Public(_)))
        {
            return Err(unsupported(
                declaration,
                "public field binding not implemented",
            ));
        }
        let mut service_chars = service.chars();
        let key = service_chars.next().unwrap().to_lowercase().to_string() + service_chars.as_str();
        let mut methods_js = String::new();
        let mut methods_ts = String::new();
        let mut names = BTreeSet::new();
        for item in &file.items {
            let Item::Impl(item) = item else { continue };
            if item.trait_.is_some() || item.self_ty.to_token_stream().to_string() != service {
                continue;
            }
            for item in &item.items {
                if matches!(item, ImplItem::Macro(_)) {
                    return Err(unsupported(
                        item,
                        "macro-expanded service interfaces require further binding support",
                    ));
                }
                let ImplItem::Fn(method) = item else { continue };
                if !matches!(method.vis, Visibility::Public(_)) {
                    continue;
                }
                let sig = &method.sig;
                if !sig.generics.params.is_empty() || sig.unsafety.is_some() {
                    return Err(unsupported(
                        sig,
                        "generic or unsafe method binding not implemented",
                    ));
                }
                if !matches!(sig.inputs.first(), Some(FnArg::Receiver(receiver)) if receiver.reference.is_some() && receiver.mutability.is_none())
                {
                    return Err(unsupported(sig, "service methods currently require &self"));
                }
                let rust_name = sig.ident.to_string();
                let js_name = camel(&rust_name);
                if !names.insert(js_name.clone()) {
                    return Err(unsupported(sig, "method names collide in TypeScript"));
                }
                let mut args_rust = Vec::new();
                let mut args_js = Vec::new();
                let mut params_ts = Vec::new();
                let mut decode = String::new();
                for arg in sig.inputs.iter().skip(1) {
                    let FnArg::Typed(arg) = arg else {
                        unreachable!()
                    };
                    let syn::Pat::Ident(name) = arg.pat.as_ref() else {
                        return Err(unsupported(arg, "named parameters required"));
                    };
                    let name = &name.ident;
                    let js = camel(&name.to_string());
                    let ty = ts_type(&arg.ty)?;
                    decode.push_str(&format!(
                        "let {name}: {} = ::rutis_interop::decode(args[{}].clone())?; {}\n",
                        arg.ty.to_token_stream(),
                        args_rust.len(),
                        validate(&name.to_string(), &ty)
                    ));
                    args_rust.push(name.to_string());
                    args_js.push(js.clone());
                    params_ts.push(format!("{js}: {ty}"));
                }
                let unit: Type = syn::parse_quote!(());
                let mut output = match &sig.output {
                    ReturnType::Default => &unit,
                    ReturnType::Type(_, ty) => ty,
                };
                let mut fallible = false;
                if let Type::Path(path) = output {
                    if let Some(segment) = path
                        .path
                        .segments
                        .last()
                        .filter(|segment| segment.ident == "Result")
                    {
                        if let PathArguments::AngleBracketed(args) = &segment.arguments {
                            if let Some(GenericArgument::Type(ty)) = args.args.first() {
                                output = ty;
                                fallible = true;
                            }
                        }
                    }
                }
                let output_ts = ts_type(output)?;
                let wait = if sig.asyncness.is_some() {
                    ".await"
                } else {
                    ""
                };
                let error = if fallible {
                    ".map_err(::rutis_interop::server::native_error)?"
                } else {
                    ""
                };
                let wrong_arity = if args_rust.is_empty() {
                    "!args.is_empty()".to_owned()
                } else {
                    format!("args.len() != {}", args_rust.len())
                };
                let invoke = format!("service.{rust_name}({}){wait}{error}", args_rust.join(", "));
                let returned = if output_ts == "void" {
                    format!("{invoke}; Ok(::rutis_interop::serde_json::Value::Null)")
                } else {
                    format!("let result: {} = {invoke}; {} ::rutis_interop::serde_json::to_value(result).map_err(|error| ::rutis_interop::Error::Value(error.to_string()))", output.to_token_stream(), validate("result", &output_ts))
                };
                arms.push_str(&format!(r#"({key:?}, {rust_name:?}) => {{
                    let args = args.as_array().ok_or_else(|| ::rutis_interop::Error::Value("expected argument array".into()))?;
                    if {wrong_arity} {{ return Err(::rutis_interop::Error::Value("wrong argument count".into())); }}
                    {decode}
                    let service = self.0.require::<{crate_name}::{service}>().map_err(::rutis_interop::server::native_error)?;
                    {returned}
                }},"#));
                let async_js = if sig.asyncness.is_some() {
                    "async "
                } else {
                    ""
                };
                let call = if sig.asyncness.is_some() {
                    "callAsync"
                } else {
                    "call"
                };
                let expression = format!(
                    "this.#process.{call}({key:?}, {rust_name:?}, [{}])",
                    args_js.join(", ")
                );
                let returned = if output_ts == "void" {
                    format!(
                        "{}{expression};",
                        if sig.asyncness.is_some() {
                            "await "
                        } else {
                            ""
                        }
                    )
                } else {
                    format!("return {expression};")
                };
                methods_js.push_str(&format!(
                    "{async_js}{js_name}({}) {{ {returned} }}\n",
                    args_js.join(", ")
                ));
                let returned = if sig.asyncness.is_some() {
                    format!("Promise<{output_ts}>")
                } else {
                    output_ts
                };
                methods_ts.push_str(&format!(
                    "{js_name}({}): {returned};\n",
                    params_ts.join(", ")
                ));
            }
        }
        if names.is_empty() {
            return Err(unsupported(
                declaration,
                "service has no supported public methods",
            ));
        }
        classes.push_str(&format!("export class {service} {{ #process; constructor(process) {{ this.#process = process; }} {methods_js} }}\n"));
        declarations.push_str(&format!(
            "export declare class {service} {{ private constructor(); {methods_ts} }}\n"
        ));
        context.push_str(&format!("{key}: {service};\n"));
        provide.push_str(&format!(
            "yield ctx.provide({key:?}, new {service}(process));\n"
        ));
        require.push_str(&format!("ctx.require::<{crate_name}::{service}>().map_err(::rutis_interop::server::native_error)?;\n"));
        dependencies.push(format!("::rutis::TypeKey::of::<{crate_name}::{service}>()"));
    }
    let rust = format!(
        r#"
    struct Exports(::rutis::Ctx);
    struct Exporter {{
        dependencies: Vec<::rutis::TypeKey>,
        scope: ::std::sync::Arc<::std::sync::Mutex<Option<::rutis::Ctx>>>,
    }}
    impl ::rutis::Plugin for Exporter {{
        fn name(&self) -> &str {{ "interop-export" }}
        fn injects(&self) -> &[::rutis::TypeKey] {{ &self.dependencies }}
        fn apply<'a>(&'a self, ctx: &'a ::rutis::Ctx) -> ::rutis::BoxFuture<'a, Result<::rutis::Effect, ::rutis::CordisError>> {{
            Box::pin(async move {{
                {require}
                *self.scope.lock().unwrap() = Some(ctx.clone());
                Ok(::rutis::Effect::Done)
            }})
        }}
    }}
    impl ::rutis_interop::server::Dispatch for Exports {{
        fn call<'a>(&'a self, target: &'a str, method: &'a str, args: ::rutis_interop::serde_json::Value) -> ::rutis::BoxFuture<'a, Result<::rutis_interop::serde_json::Value, ::rutis_interop::Error>> {{
            Box::pin(async move {{ match (target, method) {{ {arms}
                _ => Err(::rutis_interop::Error::Value("unknown service method".into()))
            }} }})
        }}
    }}
    async fn run() -> Result<(), ::rutis_interop::Error> {{
        ::rutis_interop::server::serve(|ctx, config| async move {{
            {config_rust}
            let mounted = ctx.plugin({crate_name}::{plugin_name} {{ {} }});
            (&mounted).await.map_err(::rutis_interop::server::native_error)?;
            let scope = ::std::sync::Arc::new(::std::sync::Mutex::new(None));
            let exporter = ctx.plugin(Exporter {{ dependencies: vec![{}], scope: scope.clone() }});
            (&exporter).await.map_err(::rutis_interop::server::native_error)?;
            let scope = scope.lock().unwrap().take().ok_or_else(|| ::rutis_interop::Error::Value("native service dependencies are unresolved".into()))?;
            Ok(Exports(scope))
        }}).await
    }}"#,
        config_fields.join(", "),
        dependencies.join(", ")
    );
    let import =
        serde_json::to_string(&node_package.join("src/client.mjs").to_string_lossy()).unwrap();
    let js = format!(
        r#"import {{ Process }} from {import};
    {classes}
    export function plugin(executable) {{
        return {{ name: "rutis:{plugin_name}", async apply(ctx, config) {{
            const process = await Process.launch(executable, config);
            ctx.effect(function* () {{
                yield () => process.dispose();
                {provide}
            }});
        }} }};
    }}"#
    );
    let cordis_types = serde_json::to_string(
        &node_package
            .join("node_modules/@deepseek-ai/cordis/lib/types/index.d.ts")
            .to_string_lossy(),
    )
    .unwrap();
    let dts = format!(
        r#"import type {{ Context }} from {cordis_types};
    export interface Config {{ {config_ts} }}
    {declarations}
    declare module {cordis_types} {{ interface Context {{ {context} }} }}
    export declare function plugin(executable: string): {{ name: string; apply(ctx: Context, config: Config): Promise<void> }};
    "#
    );
    Ok((rust, js, dts))
}

/// Generate a native Cordis mount and a compiled dispatcher for a Rust entry source.
/// This initial source adapter supports concrete public plugins and value methods;
/// Cargo checks every generated call against the original crate's actual types.
pub fn rutis_plugin(
    source: impl AsRef<Path>,
    crate_name: &str,
    node_package: impl AsRef<Path>,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let source = source.as_ref().canonicalize()?;
    let node_package = node_package.as_ref().canonicalize()?;
    syn::parse_str::<syn::Ident>(crate_name)?;
    println!("cargo:rerun-if-changed={}", source.display());
    let (rust, js, dts) = generate(
        &std::fs::read_to_string(&source)?,
        crate_name,
        &node_package,
    )
    .map_err(|error| {
        let start = error.span().start();
        format!(
            "{}:{}:{}: {error}",
            source.display(),
            start.line,
            start.column + 1
        )
    })?;
    let output = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(output.join("rutis.rs"), rust)?;
    std::fs::write(output.join("rutis.mjs"), js)?;
    std::fs::write(output.join("rutis.d.mts"), dts)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLUGIN: &str = include_str!("../../../../examples/native-mount/src/lib.rs");

    #[test]
    fn rejects_borrowed_results_instead_of_copying_remote_state() {
        let source = PLUGIN.replace(
            "pub fn current(&self) -> f64",
            "pub fn current(&self) -> &f64",
        );
        let error = generate(&source, "fixture", Path::new("/node")).unwrap_err();
        assert!(error.to_string().contains("type binding not implemented"));
        assert!(error.span().start().line > 1);
    }

    #[test]
    fn does_not_silently_drop_public_fields_or_macro_methods() {
        let field = PLUGIN.replace("value: Mutex<f64>", "pub value: Mutex<f64>");
        assert!(generate(&field, "fixture", Path::new("/node"))
            .unwrap_err()
            .to_string()
            .contains("public field"));
        let methods = PLUGIN.replace("impl Counter {", "impl Counter { generated_methods!();");
        assert!(generate(&methods, "fixture", Path::new("/node"))
            .unwrap_err()
            .to_string()
            .contains("macro-expanded"));
        for hidden in ["mod extra;", "more_service_methods!();"] {
            let source = format!("{PLUGIN}\n{hidden}");
            assert!(generate(&source, "fixture", Path::new("/node"))
                .unwrap_err()
                .to_string()
                .contains("expansion"));
        }
    }
}
