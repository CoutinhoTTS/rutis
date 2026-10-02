//! The subset of JavaScript dsh writes in `!!js` expressions, evaluated in
//! Rust so rutis-loader can decide `disabled` and build configs.
//!
//! Syntax: literals (numbers, strings, `true`, `false`, `null`, `undefined`,
//! arrays), member access with `.`, `?.` and `[..]`, calls, `!`, unary `-`
//! and `+`, `* / % + -`, comparisons, `===`, `!==`, `==`, `!=`, `&&`, `||`,
//! `??` and `?:`.
//!
//! Names in scope: `process.env`, `process.platform`, `process.cwd()`,
//! `process.getBuiltinModule('node:path')` (`resolve`, `join`), `JSON.parse`,
//! `Number`, `String`, `Boolean`, host functions such as `dshHomePath`, and
//! `ctx`: `ctx.get(name)` and `ctx.<name>` reach services through the
//! loader's [`ExprScope`] — any catalog name can be tested, only readable
//! services can be read. Anything else is an error, never a guess.

use std::collections::HashMap;
use std::sync::Arc;

use rutis_loader::{ExprScope, Expressions, LoaderError};
use serde_json::{Number, Value};

use super::paths;

pub type HostFn = Arc<dyn Fn(&[Value]) -> Result<Value, String> + Send + Sync>;

/// What `process` shows to expressions.
#[derive(Clone)]
pub struct Environment {
    /// `process.env`; `None` reads the live process environment.
    pub vars: Option<HashMap<String, String>>,
    /// `process.cwd()`; `None` uses the live working directory.
    pub cwd: Option<String>,
    /// `process.platform`, in Node's spelling.
    pub platform: String,
    /// Global functions by name.
    pub functions: HashMap<String, HostFn>,
}

impl Environment {
    /// The live process, with dsh's `dshHomePath`.
    pub fn current() -> Self {
        let mut functions: HashMap<String, HostFn> = HashMap::new();
        functions.insert(
            "dshHomePath".into(),
            Arc::new(|args: &[Value]| {
                let mut segments = vec![paths::dsh_home().to_string_lossy().into_owned()];
                for arg in args {
                    segments.push(to_js_string(&V::Json(arg.clone())));
                }
                Ok(Value::String(paths::join(&segments)))
            }),
        );
        Self {
            vars: None,
            cwd: None,
            platform: node_platform().into(),
            functions,
        }
    }

    fn var(&self, name: &str) -> Option<String> {
        match &self.vars {
            Some(vars) => vars.get(name).cloned(),
            None => std::env::var(name).ok(),
        }
    }

    fn cwd(&self) -> String {
        self.cwd.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".into())
        })
    }
}

/// Node's `process.platform` for this build.
pub fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// The evaluator to install as `LoaderOptions::expressions`.
pub struct JsSubset {
    env: Environment,
}

impl JsSubset {
    pub fn new(env: Environment) -> Self {
        Self { env }
    }
}

impl Expressions for JsSubset {
    fn evaluate(&self, expr: &str, scope: &ExprScope<'_>) -> Result<Value, LoaderError> {
        let ast = Parser::new(expr)
            .and_then(|mut p| p.parse())
            .map_err(|e| LoaderError::Expression(format!("{e} in `{expr}`")))?;
        let value = Eval {
            env: &self.env,
            scope,
        }
        .eval(&ast)
        .map_err(|e| match e {
            Error::Loader(e) => e,
            Error::Js(message) => LoaderError::Expression(format!("{message} in `{expr}`")),
        })?;
        to_json(value).map_err(|m| LoaderError::Expression(format!("{m} in `{expr}`")))
    }
}

// ── lexer ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    P(&'static str),
}

const PUNCTS: &[&str] = &[
    "===", "!==", "?.", "??", "==", "!=", "<=", ">=", "&&", "||", "!", "?", ":", ".", ",", "(",
    ")", "[", "]", "<", ">", "+", "-", "*", "/", "%",
];

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                // An exponent sign belongs to the number.
                if (chars[i] == 'e' || chars[i] == 'E')
                    && matches!(chars.get(i + 1), Some('+' | '-'))
                {
                    i += 1;
                }
                i += 1;
            }
            let text: String = chars[start..i].iter().filter(|c| **c != '_').collect();
            let value =
                if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                    i64::from_str_radix(hex, 16)
                        .map(|n| n as f64)
                        .map_err(|_| format!("bad number {text}"))?
                } else {
                    text.parse::<f64>()
                        .map_err(|_| format!("bad number {text}"))?
                };
            out.push(Tok::Num(value));
            continue;
        }
        if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            loop {
                let Some(&c) = chars.get(i) else {
                    return Err("unterminated string".into());
                };
                i += 1;
                if c == quote {
                    break;
                }
                if c == '\\' {
                    let Some(&e) = chars.get(i) else {
                        return Err("unterminated string".into());
                    };
                    i += 1;
                    s.push(match e {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '0' => '\0',
                        other => other,
                    });
                } else {
                    s.push(c);
                }
            }
            out.push(Tok::Str(s));
            continue;
        }
        if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            out.push(Tok::Ident(chars[start..i].iter().collect()));
            continue;
        }
        let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
        let Some(p) = PUNCTS.iter().find(|p| rest.starts_with(**p)) else {
            return Err(format!("unexpected `{c}`"));
        };
        // `?.` followed by a digit is `?` then a number.
        if *p == "?." && chars.get(i + 2).is_some_and(|d| d.is_ascii_digit()) {
            out.push(Tok::P("?"));
            i += 1;
            continue;
        }
        out.push(Tok::P(p));
        i += p.len();
    }
    Ok(out)
}

// ── parser ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Ast {
    Lit(Value),
    Undefined,
    Ident(String),
    Array(Vec<Ast>),
    /// object, property, optional
    Member(Box<Ast>, String, bool),
    Index(Box<Ast>, Box<Ast>, bool),
    /// callee, args, optional
    Call(Box<Ast>, Vec<Ast>, bool),
    Unary(&'static str, Box<Ast>),
    Binary(&'static str, Box<Ast>, Box<Ast>),
    Cond(Box<Ast>, Box<Ast>, Box<Ast>),
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn new(src: &str) -> Result<Self, String> {
        Ok(Self {
            toks: lex(src)?,
            pos: 0,
        })
    }

    fn parse(&mut self) -> Result<Ast, String> {
        let ast = self.expr()?;
        match self.toks.get(self.pos) {
            None => Ok(ast),
            Some(t) => Err(format!("unexpected {t:?}")),
        }
    }

    fn peek(&self, p: &str) -> bool {
        matches!(self.toks.get(self.pos), Some(Tok::P(q)) if *q == p)
    }

    fn eat(&mut self, p: &str) -> bool {
        let found = self.peek(p);
        if found {
            self.pos += 1;
        }
        found
    }

    fn expect(&mut self, p: &str) -> Result<(), String> {
        if self.eat(p) {
            Ok(())
        } else {
            Err(format!("expected `{p}`"))
        }
    }

    fn expr(&mut self) -> Result<Ast, String> {
        let test = self.binary(0)?;
        if self.eat("?") {
            let yes = self.expr()?;
            self.expect(":")?;
            let no = self.expr()?;
            return Ok(Ast::Cond(Box::new(test), Box::new(yes), Box::new(no)));
        }
        Ok(test)
    }

    fn precedence(op: &str) -> Option<u8> {
        Some(match op {
            "??" => 1,
            "||" => 2,
            "&&" => 3,
            "===" | "!==" | "==" | "!=" => 4,
            "<" | ">" | "<=" | ">=" => 5,
            "+" | "-" => 6,
            "*" | "/" | "%" => 7,
            _ => return None,
        })
    }

    fn binary(&mut self, min: u8) -> Result<Ast, String> {
        let mut left = self.unary()?;
        loop {
            let Some(Tok::P(op)) = self.toks.get(self.pos).cloned() else {
                return Ok(left);
            };
            let Some(prec) = Self::precedence(op) else {
                return Ok(left);
            };
            if prec <= min {
                return Ok(left);
            }
            self.pos += 1;
            let right = self.binary(prec)?;
            left = Ast::Binary(op, Box::new(left), Box::new(right));
        }
    }

    fn unary(&mut self) -> Result<Ast, String> {
        for op in ["!", "-", "+"] {
            if self.eat(op) {
                return Ok(Ast::Unary(op, Box::new(self.unary()?)));
            }
        }
        self.postfix()
    }

    fn args(&mut self) -> Result<Vec<Ast>, String> {
        let mut args = Vec::new();
        if self.eat(")") {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if self.eat(")") {
                return Ok(args);
            }
            self.expect(",")?;
        }
    }

    fn name(&mut self) -> Result<String, String> {
        match self.toks.get(self.pos).cloned() {
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                Ok(name)
            }
            other => Err(format!("expected a property name, found {other:?}")),
        }
    }

    fn postfix(&mut self) -> Result<Ast, String> {
        let mut node = self.primary()?;
        loop {
            if self.eat(".") {
                node = Ast::Member(Box::new(node), self.name()?, false);
            } else if self.eat("?.") {
                if self.eat("(") {
                    node = Ast::Call(Box::new(node), self.args()?, true);
                } else if self.eat("[") {
                    let index = self.expr()?;
                    self.expect("]")?;
                    node = Ast::Index(Box::new(node), Box::new(index), true);
                } else {
                    node = Ast::Member(Box::new(node), self.name()?, true);
                }
            } else if self.eat("(") {
                node = Ast::Call(Box::new(node), self.args()?, false);
            } else if self.eat("[") {
                let index = self.expr()?;
                self.expect("]")?;
                node = Ast::Index(Box::new(node), Box::new(index), false);
            } else {
                return Ok(node);
            }
        }
    }

    fn primary(&mut self) -> Result<Ast, String> {
        let tok = self
            .toks
            .get(self.pos)
            .cloned()
            .ok_or_else(|| "unexpected end".to_owned())?;
        self.pos += 1;
        Ok(match tok {
            Tok::Num(n) => Ast::Lit(js_number(n)),
            Tok::Str(s) => Ast::Lit(Value::String(s)),
            Tok::Ident(name) => match name.as_str() {
                "true" => Ast::Lit(Value::Bool(true)),
                "false" => Ast::Lit(Value::Bool(false)),
                "null" => Ast::Lit(Value::Null),
                "undefined" => Ast::Undefined,
                _ => Ast::Ident(name),
            },
            Tok::P("(") => {
                let inner = self.expr()?;
                self.expect(")")?;
                inner
            }
            Tok::P("[") => {
                let mut items = Vec::new();
                if !self.eat("]") {
                    loop {
                        items.push(self.expr()?);
                        if self.eat("]") {
                            break;
                        }
                        self.expect(",")?;
                    }
                }
                Ast::Array(items)
            }
            other => return Err(format!("unexpected {other:?}")),
        })
    }
}

// ── values and evaluation ───────────────────────────────────────

#[derive(Clone)]
enum V {
    Undefined,
    /// A number while evaluating, so NaN and infinities behave as in
    /// JavaScript; JSON results write them as null.
    Num(f64),
    Json(Value),
    /// A service that exists but cannot be read.
    Opaque,
    Builtin(Builtin),
    Method(Box<V>, String),
    Host(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Builtin {
    Process,
    Env,
    Cwd,
    GetBuiltinModule,
    Path,
    PathResolve,
    PathJoin,
    Ctx,
    CtxGet,
    Json,
    JsonParse,
    Number,
    String,
    Boolean,
}

enum Error {
    Js(String),
    Loader(LoaderError),
}

impl From<LoaderError> for Error {
    fn from(e: LoaderError) -> Self {
        Error::Loader(e)
    }
}

fn js(message: impl Into<String>) -> Error {
    Error::Js(message.into())
}

fn js_number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        Value::Number(Number::from(n as i64))
    } else {
        Number::from_f64(n).map_or(Value::Null, Value::Number)
    }
}

fn as_number(v: &V) -> Option<f64> {
    match v {
        V::Num(n) => Some(*n),
        V::Json(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

fn truthy(v: &V) -> bool {
    match v {
        V::Undefined => false,
        V::Num(n) => *n != 0.0 && !n.is_nan(),
        V::Json(value) => rutis_loader_truthy(value),
        _ => true,
    }
}

fn rutis_loader_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn nullish(v: &V) -> bool {
    matches!(v, V::Undefined | V::Json(Value::Null))
}

fn to_number(v: &V) -> f64 {
    match v {
        V::Undefined => f64::NAN,
        V::Num(n) => *n,
        V::Json(Value::Null) => 0.0,
        V::Json(Value::Bool(b)) => f64::from(u8::from(*b)),
        V::Json(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        V::Json(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                i64::from_str_radix(hex, 16).map_or(f64::NAN, |n| n as f64)
            } else {
                t.parse().unwrap_or(f64::NAN)
            }
        }
        _ => f64::NAN,
    }
}

fn number_text(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn to_js_string(v: &V) -> String {
    match v {
        V::Undefined => "undefined".into(),
        V::Num(n) => number_text(*n),
        V::Json(Value::Null) => "null".into(),
        V::Json(Value::Bool(b)) => b.to_string(),
        V::Json(Value::Number(n)) => number_text(n.as_f64().unwrap_or(f64::NAN)),
        V::Json(Value::String(s)) => s.clone(),
        V::Json(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::Null => String::new(),
                other => to_js_string(&V::Json(other.clone())),
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => "[object Object]".into(),
    }
}

fn strict_eq(a: &V, b: &V) -> bool {
    if let (Some(x), Some(y)) = (as_number(a), as_number(b)) {
        return x == y;
    }
    match (a, b) {
        (V::Undefined, V::Undefined) => true,
        (V::Json(x @ (Value::Null | Value::Bool(_) | Value::String(_))), V::Json(y)) => x == y,
        _ => false,
    }
}

fn to_json(v: V) -> Result<Value, String> {
    match v {
        V::Undefined => Ok(Value::Null),
        V::Num(n) => Ok(js_number(n)),
        V::Json(value) => Ok(value),
        _ => Err("the result is not a JSON value".into()),
    }
}

struct Eval<'a, 's> {
    env: &'a Environment,
    scope: &'a ExprScope<'s>,
}

impl Eval<'_, '_> {
    fn eval(&self, ast: &Ast) -> Result<V, Error> {
        Ok(match ast {
            Ast::Lit(value) => V::Json(value.clone()),
            Ast::Undefined => V::Undefined,
            Ast::Ident(name) => match name.as_str() {
                "process" => V::Builtin(Builtin::Process),
                "ctx" => V::Builtin(Builtin::Ctx),
                "JSON" => V::Builtin(Builtin::Json),
                "Number" => V::Builtin(Builtin::Number),
                "String" => V::Builtin(Builtin::String),
                "Boolean" => V::Builtin(Builtin::Boolean),
                other if self.env.functions.contains_key(other) => V::Host(other.to_owned()),
                other => return Err(js(format!("{other} is not defined"))),
            },
            Ast::Array(items) => {
                let mut out = Vec::new();
                for item in items {
                    out.push(to_json(self.eval(item)?).map_err(js)?);
                }
                V::Json(Value::Array(out))
            }
            Ast::Member(object, name, optional) => {
                let object = self.eval(object)?;
                if *optional && nullish(&object) {
                    return Ok(V::Undefined);
                }
                self.member(object, name)?
            }
            Ast::Index(object, index, optional) => {
                let object = self.eval(object)?;
                if *optional && nullish(&object) {
                    return Ok(V::Undefined);
                }
                let index = self.eval(index)?;
                match (&object, as_number(&index)) {
                    (V::Json(Value::Array(items)), Some(n)) if n >= 0.0 && n.fract() == 0.0 => {
                        items
                            .get(n as usize)
                            .map_or(V::Undefined, |v| V::Json(v.clone()))
                    }
                    _ => self.member(object, &to_js_string(&index))?,
                }
            }
            Ast::Call(callee, args, optional) => {
                let callee = self.eval(callee)?;
                if *optional && nullish(&callee) {
                    return Ok(V::Undefined);
                }
                let args = args
                    .iter()
                    .map(|a| self.eval(a))
                    .collect::<Result<Vec<_>, _>>()?;
                self.call(callee, args)?
            }
            Ast::Unary(op, operand) => {
                let v = self.eval(operand)?;
                match *op {
                    "!" => V::Json(Value::Bool(!truthy(&v))),
                    "-" => V::Num(-to_number(&v)),
                    _ => V::Num(to_number(&v)),
                }
            }
            Ast::Binary(op, left, right) => {
                let l = self.eval(left)?;
                match *op {
                    "&&" => return if truthy(&l) { self.eval(right) } else { Ok(l) },
                    "||" => return if truthy(&l) { Ok(l) } else { self.eval(right) },
                    "??" => return if nullish(&l) { self.eval(right) } else { Ok(l) },
                    _ => {}
                }
                let r = self.eval(right)?;
                let bool = |b: bool| V::Json(Value::Bool(b));
                match *op {
                    "===" => bool(strict_eq(&l, &r)),
                    "!==" => bool(!strict_eq(&l, &r)),
                    "==" => bool(strict_eq(&l, &r) || (nullish(&l) && nullish(&r))),
                    "!=" => bool(!(strict_eq(&l, &r) || (nullish(&l) && nullish(&r)))),
                    "<" | ">" | "<=" | ">=" => {
                        let ordering = match (&l, &r) {
                            (V::Json(Value::String(a)), V::Json(Value::String(b))) => {
                                Some(a.cmp(b))
                            }
                            _ => to_number(&l).partial_cmp(&to_number(&r)),
                        };
                        bool(match (*op, ordering) {
                            (_, None) => false,
                            ("<", Some(o)) => o.is_lt(),
                            (">", Some(o)) => o.is_gt(),
                            ("<=", Some(o)) => o.is_le(),
                            (_, Some(o)) => o.is_ge(),
                        })
                    }
                    "+" => match (&l, &r) {
                        (V::Json(Value::String(_)), _) | (_, V::Json(Value::String(_))) => {
                            V::Json(Value::String(to_js_string(&l) + &to_js_string(&r)))
                        }
                        _ => V::Num(to_number(&l) + to_number(&r)),
                    },
                    "-" => V::Num(to_number(&l) - to_number(&r)),
                    "*" => V::Num(to_number(&l) * to_number(&r)),
                    "/" => V::Num(to_number(&l) / to_number(&r)),
                    _ => V::Num(to_number(&l) % to_number(&r)),
                }
            }
            Ast::Cond(test, yes, no) => {
                if truthy(&self.eval(test)?) {
                    self.eval(yes)?
                } else {
                    self.eval(no)?
                }
            }
        })
    }

    fn member(&self, object: V, name: &str) -> Result<V, Error> {
        Ok(match &object {
            V::Undefined | V::Json(Value::Null) => {
                return Err(js(format!(
                    "cannot read properties of {} (reading '{name}')",
                    to_js_string(&object)
                )))
            }
            V::Builtin(Builtin::Process) => match name {
                "env" => V::Builtin(Builtin::Env),
                "platform" => V::Json(Value::String(self.env.platform.clone())),
                "cwd" => V::Builtin(Builtin::Cwd),
                "getBuiltinModule" => V::Builtin(Builtin::GetBuiltinModule),
                _ => return Err(js(format!("process.{name} is not available"))),
            },
            V::Builtin(Builtin::Env) => self
                .env
                .var(name)
                .map_or(V::Undefined, |v| V::Json(Value::String(v))),
            V::Builtin(Builtin::Path) => match name {
                "resolve" => V::Builtin(Builtin::PathResolve),
                "join" => V::Builtin(Builtin::PathJoin),
                "sep" => V::Json(Value::String("/".into())),
                _ => return Err(js(format!("path.{name} is not available"))),
            },
            V::Builtin(Builtin::Json) => match name {
                "parse" => V::Builtin(Builtin::JsonParse),
                _ => return Err(js(format!("JSON.{name} is not available"))),
            },
            V::Builtin(Builtin::Ctx) => match name {
                "get" => V::Builtin(Builtin::CtxGet),
                service => match self.scope.read(service)? {
                    Some(value) => V::Json(value),
                    None => V::Undefined,
                },
            },
            V::Json(Value::Object(map)) => {
                map.get(name).map_or(V::Undefined, |v| V::Json(v.clone()))
            }
            V::Json(Value::Array(items)) => match name {
                "length" => V::Json(Value::from(items.len())),
                "includes" | "indexOf" | "join" => V::Method(Box::new(object.clone()), name.into()),
                _ => V::Undefined,
            },
            V::Json(Value::String(s)) => match name {
                "length" => V::Json(Value::from(s.chars().count())),
                "includes" | "startsWith" | "endsWith" | "toLowerCase" | "toUpperCase" | "trim" => {
                    V::Method(Box::new(object.clone()), name.into())
                }
                _ => V::Undefined,
            },
            _ => V::Undefined,
        })
    }

    fn call(&self, callee: V, args: Vec<V>) -> Result<V, Error> {
        let arg = |i: usize| args.get(i).cloned().unwrap_or(V::Undefined);
        let strings = || args.iter().map(to_js_string).collect::<Vec<_>>();
        Ok(match callee {
            V::Builtin(Builtin::Cwd) => V::Json(Value::String(self.env.cwd())),
            V::Builtin(Builtin::GetBuiltinModule) => match to_js_string(&arg(0)).as_str() {
                "node:path" | "path" | "node:path/posix" | "path/posix" => {
                    V::Builtin(Builtin::Path)
                }
                other => return Err(js(format!("module {other} is not available"))),
            },
            V::Builtin(Builtin::PathResolve) => {
                V::Json(Value::String(paths::resolve(&self.env.cwd(), &strings())))
            }
            V::Builtin(Builtin::PathJoin) => V::Json(Value::String(paths::join(&strings()))),
            V::Builtin(Builtin::CtxGet) => {
                let name = to_js_string(&arg(0));
                match self.scope.read(&name) {
                    Ok(Some(value)) => V::Json(value),
                    Ok(None) => V::Undefined,
                    Err(LoaderError::NotReadable(_)) => {
                        if self.scope.has(&name)? {
                            V::Opaque
                        } else {
                            V::Undefined
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            V::Builtin(Builtin::JsonParse) => {
                let text = to_js_string(&arg(0));
                V::Json(serde_json::from_str(&text).map_err(|e| js(format!("JSON.parse: {e}")))?)
            }
            V::Builtin(Builtin::Number) => V::Num(if args.is_empty() {
                0.0
            } else {
                to_number(&arg(0))
            }),
            V::Builtin(Builtin::String) => V::Json(Value::String(if args.is_empty() {
                String::new()
            } else {
                to_js_string(&arg(0))
            })),
            V::Builtin(Builtin::Boolean) => V::Json(Value::Bool(truthy(&arg(0)))),
            V::Host(name) => {
                let values = args
                    .into_iter()
                    .map(|a| to_json(a).map_err(js))
                    .collect::<Result<Vec<_>, _>>()?;
                V::Json((self.env.functions[&name])(&values).map_err(js)?)
            }
            V::Method(receiver, name) => match (*receiver, name.as_str()) {
                (V::Json(Value::Array(items)), "includes") => {
                    let needle = arg(0);
                    V::Json(Value::Bool(
                        items
                            .iter()
                            .any(|i| strict_eq(&V::Json(i.clone()), &needle)),
                    ))
                }
                (V::Json(Value::Array(items)), "indexOf") => {
                    let needle = arg(0);
                    let index = items
                        .iter()
                        .position(|i| strict_eq(&V::Json(i.clone()), &needle))
                        .map_or(-1, |i| i as i64);
                    V::Json(Value::from(index))
                }
                (V::Json(Value::Array(items)), "join") => {
                    let sep = match arg(0) {
                        V::Undefined => ",".to_owned(),
                        other => to_js_string(&other),
                    };
                    V::Json(Value::String(
                        items
                            .iter()
                            .map(|i| to_js_string(&V::Json(i.clone())))
                            .collect::<Vec<_>>()
                            .join(&sep),
                    ))
                }
                (V::Json(Value::String(s)), method) => {
                    let other = to_js_string(&arg(0));
                    match method {
                        "includes" => V::Json(Value::Bool(s.contains(&other))),
                        "startsWith" => V::Json(Value::Bool(s.starts_with(&other))),
                        "endsWith" => V::Json(Value::Bool(s.ends_with(&other))),
                        "toLowerCase" => V::Json(Value::String(s.to_lowercase())),
                        "toUpperCase" => V::Json(Value::String(s.to_uppercase())),
                        _ => V::Json(Value::String(s.trim().to_owned())),
                    }
                }
                _ => return Err(js(format!("{name} is not a function"))),
            },
            _ => return Err(js("not a function")),
        })
    }
}
