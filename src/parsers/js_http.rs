//! Backend calls a JS/TS/Vue script makes: `fetch`, `axios`, an `axios.create()` instance, a
//! project wrapper around one of those, jQuery `$.ajax`/`$.post`/`$.get`, `BX.ajax`,
//! `BX.ajax.runComponentAction`, `BX.ajax.runAction` — with the method, the address folded from
//! literals, template strings and constants (the rest as `{…}`), the keys sent, and the keys the
//! code reads from the answer. Also `window.X = …` writes and `window.X.key` reads, which carry
//! data from a PHP template to scripts.

use std::cell::RefCell;
use std::collections::HashSet;

use tree_sitter::{Node, Parser, Tree};

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

fn parse(script: &str) -> Option<Tree> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        let parser = match slot.as_mut() {
            Some(p) => p,
            None => {
                let mut p = Parser::new();
                p.set_language(&tree_sitter_typescript::LANGUAGE_TSX.into()).ok()?;
                slot.insert(p)
            }
        };
        parser.parse(script, None)
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpCall {
    pub line: usize,
    /// `fetch`, `axios`, `axios-instance`, `wrapper`, `jquery`, `bx.ajax`,
    /// `bx.runComponentAction`, `bx.runAction`.
    pub client: &'static str,
    /// The instance or wrapper called (`api.get`, `request.post`).
    pub via: Option<String>,
    pub method: Option<String>,
    /// The address with what does not fold as `{expression}`.
    pub url: Option<String>,
    /// Nothing of the address folds: the expression as written.
    pub url_expr: Option<String>,
    pub component: Option<String>,
    pub action: Option<String>,
    pub mode: Option<String>,
    /// Keys of the parameters or body sent.
    pub params: Vec<String>,
    /// Paths read from the answer, from the variable holding it (`data.items`).
    pub reads: Vec<String>,
    /// The same with the client's envelope removed: axios' `data`, the `data` of a Bitrix
    /// action result.
    pub response_keys: Vec<String>,
}

/// A function that forwards one of its parameters as the address of a backend call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapper {
    /// The export it is reachable by: `get`, or `default.get` for a default-exported object.
    pub name: String,
    pub url_param: usize,
    pub data_param: Option<usize>,
    pub method: Option<String>,
    /// The wrapper returns the answer's body (`return resp.data`), so its callers read no
    /// envelope.
    pub returns_body: bool,
    /// Not a function but an `axios.create()` instance: its methods are axios methods.
    pub instance: bool,
    /// A wrapper around `BX.ajax.runComponentAction` / `runAction`: that client, with the
    /// component fixed or taken from a parameter, the action from a parameter.
    pub action: Option<ActionWrapper>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionWrapper {
    /// `bx.runComponentAction` or `bx.runAction`.
    pub client: &'static str,
    pub component: Option<String>,
    pub component_param: Option<usize>,
    pub action_param: Option<usize>,
    pub mode: Option<String>,
    pub method: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralUrl {
    pub line: usize,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalWrite {
    pub line: usize,
    pub name: String,
    pub keys: Option<Vec<String>>,
    /// The value as written when its keys are not visible (a PHP placeholder included).
    pub expr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalRead {
    pub line: usize,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Default)]
pub struct ScriptFacts {
    pub calls: Vec<HttpCall>,
    pub wrappers: Vec<Wrapper>,
    /// Exported string constants: name → value.
    pub constants: Vec<(String, String)>,
    pub literals: Vec<LiteralUrl>,
    pub global_writes: Vec<GlobalWrite>,
    pub global_reads: Vec<GlobalRead>,
}

/// What the script's imports bring, as resolved by the caller.
pub trait Imports {
    /// The string value of an imported constant bound to `local`.
    fn constant(&self, local: &str) -> Option<String>;
    /// The wrapper (or instance) an imported `local` is, or its `member` for an imported
    /// object (`request.get`).
    fn wrapper(&self, local: &str, member: Option<&str>) -> Option<Wrapper>;
}

pub struct NoImports;

impl Imports for NoImports {
    fn constant(&self, _: &str) -> Option<String> {
        None
    }
    fn wrapper(&self, _: &str, _: Option<&str>) -> Option<Wrapper> {
        None
    }
}

const AXIOS_METHODS: [&str; 8] = ["get", "delete", "head", "options", "post", "put", "patch", "request"];

/// Calls a script makes. `placeholders` stand for PHP islands of a template (`__php0__` …):
/// each is shown as its value in addresses. `globals` are the `window.X` names to report reads
/// of; empty to skip reads.
pub fn analyze(
    script: &str,
    imports: &dyn Imports,
    placeholders: &[String],
    globals: &HashSet<String>,
) -> ScriptFacts {
    let Some(tree) = parse(script) else {
        return ScriptFacts::default();
    };
    let root = tree.root_node();
    let ctx = Ctx {
        src: script,
        root,
        imports,
        placeholders,
        local_wrappers: RefCell::new(Vec::new()),
        declared_names: RefCell::new(None),
    };
    let mut facts = ScriptFacts::default();
    let instances = ctx.instances();
    // Local wrappers first: calls through them in the same file are calls too.
    let wrappers = ctx.wrappers(&instances);
    *ctx.local_wrappers.borrow_mut() = wrappers.clone();
    let mut in_call: HashSet<(usize, usize)> = HashSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression" {
            if let Some(call) = ctx.http_call(node, &instances) {
                in_call.insert((node.start_byte(), node.end_byte()));
                // A wrapper's own inner call forwards its parameter: reported at the callers.
                let in_wrapper = enclosing_function_name(node, script)
                    .is_some_and(|f| wrappers.iter().any(|w| !w.instance && w.name.rsplit('.').next() == Some(f.as_str())));
                if !in_wrapper {
                    facts.calls.push(call);
                }
            }
        }
        if matches!(node.kind(), "string" | "template_string") {
            let inside_call = in_call
                .iter()
                .any(|(s, e)| *s <= node.start_byte() && node.end_byte() <= *e);
            if !inside_call {
                if let Some(url) = ctx.literal_url(node) {
                    facts.literals.push(LiteralUrl {
                        line: line(node),
                        url,
                    });
                }
            }
        }
        if node.kind() == "assignment_expression" {
            if let Some(write) = ctx.global_write(node) {
                facts.global_writes.push(write);
            }
        }
        if !globals.is_empty() && node.kind() == "member_expression" {
            if let Some(read) = ctx.global_read(node, globals) {
                facts.global_reads.push(read);
            }
        }
        if !globals.is_empty() && node.kind() == "variable_declarator" {
            facts.global_reads.extend(ctx.global_destructuring(node, globals));
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    facts.calls.sort_by_key(|c| c.line);
    facts.literals.sort_by_key(|l| l.line);
    facts.global_reads.dedup();
    facts.wrappers = ctx.exported_wrappers(&wrappers, &instances);
    facts.constants = ctx.exported_constants();
    facts
}

fn line(node: Node) -> usize {
    node.start_position().row + 1
}

fn named(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| c.kind() != "comment")
        .collect()
}

struct Ctx<'a> {
    src: &'a str,
    root: Node<'a>,
    imports: &'a dyn Imports,
    placeholders: &'a [String],
    local_wrappers: RefCell<Vec<Wrapper>>,
    /// Functions and classes declared anywhere in the script, collected on first use.
    declared_names: RefCell<Option<HashSet<String>>>,
}

enum Part {
    Lit(String),
    Hole(String),
}

impl<'a> Ctx<'a> {
    fn text(&self, node: Node) -> &'a str {
        node.utf8_text(self.src.as_bytes()).unwrap_or("")
    }

    fn with_placeholders(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (i, value) in self.placeholders.iter().enumerate().rev() {
            out = out.replace(&format!("__php{i}__"), value);
        }
        out
    }

    /// Names bound to `axios.create(...)` at any level of the script.
    fn instances(&self) -> HashSet<String> {
        let mut found = HashSet::new();
        let mut stack = vec![self.root];
        while let Some(node) = stack.pop() {
            if node.kind() == "variable_declarator" {
                if let (Some(name), Some(value)) =
                    (node.child_by_field_name("name"), node.child_by_field_name("value"))
                {
                    if name.kind() == "identifier" && self.is_axios_create(value) {
                        found.insert(self.text(name).to_string());
                    }
                }
            }
            push_children(&mut stack, node);
        }
        found
    }

    fn is_axios_create(&self, value: Node) -> bool {
        value.kind() == "call_expression"
            && value
                .child_by_field_name("function")
                .is_some_and(|f| self.text(f).replace(char::is_whitespace, "") == "axios.create")
    }

    /// The value expression a name is bound to by `const`/`let`/`var` visible at `at`.
    fn binding(&self, name: &str, at: Node<'a>) -> Option<Node<'a>> {
        let mut scope = at.parent();
        while let Some(s) = scope {
            if matches!(s.kind(), "statement_block" | "program" | "class_body") {
                for statement in named(s) {
                    let declaration = if statement.kind() == "export_statement" {
                        match statement.child_by_field_name("declaration") {
                            Some(d) => d,
                            None => continue,
                        }
                    } else {
                        statement
                    };
                    if !matches!(declaration.kind(), "lexical_declaration" | "variable_declaration") {
                        continue;
                    }
                    for declarator in named(declaration) {
                        if declarator.kind() != "variable_declarator" {
                            continue;
                        }
                        let fits = declarator
                            .child_by_field_name("name")
                            .is_some_and(|n| n.kind() == "identifier" && self.text(n) == name);
                        if fits {
                            return declarator.child_by_field_name("value");
                        }
                    }
                }
            }
            scope = s.parent();
        }
        None
    }

    /// A declaration, a parameter of an enclosing function, or a declared function or class.
    fn locally_bound(&self, name: &str, at: Node<'a>) -> bool {
        if self.binding(name, at).is_some() {
            return true;
        }
        let mut current = at.parent();
        while let Some(n) = current {
            if let Some(params) = n.child_by_field_name("parameters") {
                if self.text(params).split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).any(|w| w == name) {
                    return true;
                }
            }
            if n.child_by_field_name("parameter").is_some_and(|p| self.text(p) == name) {
                return true;
            }
            if matches!(n.kind(), "for_in_statement" | "catch_clause")
                && n.child_by_field_name("left")
                    .or_else(|| n.child_by_field_name("parameter"))
                    .is_some_and(|l| self.text(l).split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w == name))
            {
                return true;
            }
            current = n.parent();
        }
        self.declared_names
            .borrow_mut()
            .get_or_insert_with(|| {
                let mut names = HashSet::new();
                let mut stack = vec![self.root];
                while let Some(node) = stack.pop() {
                    if matches!(node.kind(), "function_declaration" | "class_declaration") {
                        if let Some(n) = node.child_by_field_name("name") {
                            names.insert(self.text(n).to_string());
                        }
                    }
                    push_children(&mut stack, node);
                }
                names
            })
            .contains(name)
    }

    fn parts(&self, node: Node<'a>, depth: usize) -> Vec<Part> {
        if depth > 6 {
            return vec![Part::Hole(self.text(node).to_string())];
        }
        match node.kind() {
            "string" => vec![Part::Lit(self.string_value(node))],
            "template_string" => {
                let mut out = Vec::new();
                for child in named(node) {
                    match child.kind() {
                        "string_fragment" | "escape_sequence" => {
                            out.push(Part::Lit(self.text(child).to_string()))
                        }
                        "template_substitution" => {
                            let Some(expr) = named(child).into_iter().next() else {
                                continue;
                            };
                            let inner = self.parts(expr, depth + 1);
                            if inner.iter().all(|p| matches!(p, Part::Lit(_))) {
                                out.extend(inner);
                            } else {
                                out.push(Part::Hole(self.text(expr).to_string()));
                            }
                        }
                        _ => {}
                    }
                }
                out
            }
            "binary_expression" => {
                let op = node.child_by_field_name("operator").map(|o| self.text(o));
                match (op, node.child_by_field_name("left"), node.child_by_field_name("right")) {
                    (Some("+"), Some(l), Some(r)) => {
                        let mut out = self.parts(l, depth + 1);
                        out.extend(self.parts(r, depth + 1));
                        out
                    }
                    _ => vec![Part::Hole(self.text(node).to_string())],
                }
            }
            "parenthesized_expression" => match named(node).into_iter().next() {
                Some(inner) => self.parts(inner, depth + 1),
                None => Vec::new(),
            },
            "identifier" => {
                let name = self.text(node);
                if let Some(value) = self.binding(name, node) {
                    let inner = self.parts(value, depth + 1);
                    if inner.iter().all(|p| matches!(p, Part::Lit(_))) {
                        return inner;
                    }
                }
                match self.imports.constant(name) {
                    Some(value) => vec![Part::Lit(value)],
                    None => vec![Part::Hole(name.to_string())],
                }
            }
            _ => vec![Part::Hole(self.text(node).to_string())],
        }
    }

    fn string_value(&self, node: Node) -> String {
        let raw = self.text(node);
        let inner = raw
            .get(1..raw.len().saturating_sub(1))
            .unwrap_or_default();
        inner.replace("\\/", "/").replace("\\'", "'").replace("\\\"", "\"")
    }

    /// (address, nothing folded → the expression as written).
    fn url(&self, node: Node<'a>) -> (Option<String>, Option<String>) {
        let parts = self.parts(node, 0);
        if !parts.iter().any(|p| matches!(p, Part::Lit(s) if !s.is_empty())) {
            return (None, Some(self.with_placeholders(self.text(node))));
        }
        let joined: String = parts
            .iter()
            .map(|p| match p {
                Part::Lit(s) => s.clone(),
                Part::Hole(h) => format!("{{{h}}}"),
            })
            .collect();
        (Some(self.with_placeholders(&joined)), None)
    }

    fn string_of(&self, node: Node<'a>) -> Option<String> {
        let parts = self.parts(node, 0);
        let mut out = String::new();
        for p in parts {
            match p {
                Part::Lit(s) => out.push_str(&s),
                Part::Hole(_) => return None,
            }
        }
        Some(self.with_placeholders(&out))
    }

    /// The value of `key` in an object literal (or a name bound to one).
    fn property(&self, object: Node<'a>, key: &str) -> Option<Node<'a>> {
        let object = self.object_literal(object)?;
        for member in named(object) {
            match member.kind() {
                "pair" => {
                    let k = member.child_by_field_name("key")?;
                    let k_text = match k.kind() {
                        "string" => self.string_value(k),
                        _ => self.text(k).to_string(),
                    };
                    if k_text == key {
                        return member.child_by_field_name("value");
                    }
                }
                "shorthand_property_identifier" if self.text(member) == key => {
                    return self.binding(key, member).or(Some(member));
                }
                "method_definition" => {
                    let fits = member
                        .child_by_field_name("name")
                        .is_some_and(|n| self.text(n) == key);
                    if fits {
                        return Some(member);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn object_literal(&self, node: Node<'a>) -> Option<Node<'a>> {
        match node.kind() {
            "object" => Some(node),
            "identifier" => self
                .binding(self.text(node), node)
                .and_then(|v| (v.kind() == "object").then_some(v)),
            "parenthesized_expression" => named(node).into_iter().next().and_then(|n| self.object_literal(n)),
            _ => None,
        }
    }

    /// Keys of the data sent: an object literal, `JSON.stringify(obj)`, a `FormData` filled
    /// with `append`, `new URLSearchParams(obj)`.
    fn keys(&self, node: Node<'a>) -> Vec<String> {
        if let Some(object) = self.object_literal(node) {
            let mut out = Vec::new();
            for member in named(object) {
                match member.kind() {
                    "pair" => {
                        if let Some(k) = member.child_by_field_name("key") {
                            out.push(match k.kind() {
                                "string" => self.string_value(k),
                                _ => self.text(k).to_string(),
                            });
                        }
                    }
                    "shorthand_property_identifier" => out.push(self.text(member).to_string()),
                    "spread_element" => out.push(self.text(member).replace(char::is_whitespace, "")),
                    _ => {}
                }
            }
            return out;
        }
        match node.kind() {
            "call_expression" | "new_expression" => {
                let callee = node
                    .child_by_field_name("function")
                    .or_else(|| node.child_by_field_name("constructor"))
                    .map(|f| self.text(f).to_string())
                    .unwrap_or_default();
                let first = node
                    .child_by_field_name("arguments")
                    .and_then(|a| named(a).into_iter().next());
                match (callee.as_str(), first) {
                    ("JSON.stringify" | "URLSearchParams", Some(arg)) => self.keys(arg),
                    // `buildParams(root)`: a function of this script returning an object literal.
                    _ => self
                        .function_named(&callee)
                        .and_then(|f| self.returned_object(f))
                        .map(|o| self.keys(o))
                        .unwrap_or_default(),
                }
            }
            "identifier" => {
                let name = self.text(node);
                let value = self.binding(name, node);
                if value.is_some_and(|v| v.kind() == "new_expression" && self.text(v).contains("FormData")) {
                    return self.form_data_keys(name, node);
                }
                value.map(|v| self.keys(v)).unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    fn form_data_keys(&self, var: &str, at: Node<'a>) -> Vec<String> {
        let scope = enclosing_function(at).unwrap_or(self.root);
        let mut out = Vec::new();
        let mut stack = vec![scope];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                let target = node.child_by_field_name("function").map(|f| self.text(f).replace(char::is_whitespace, ""));
                if target.as_deref() == Some(&format!("{var}.append")) {
                    if let Some(first) = node.child_by_field_name("arguments").and_then(|a| named(a).into_iter().next()) {
                        if let Some(k) = self.string_of(first) {
                            if !out.contains(&k) {
                                out.push(k);
                            }
                        }
                    }
                }
            }
            push_children(&mut stack, node);
        }
        out
    }

    fn args(call: Node<'a>) -> Vec<Node<'a>> {
        call.child_by_field_name("arguments")
            .map(named)
            .unwrap_or_default()
    }

    /// `obj.prop` → ("obj", "prop") with the object as written.
    fn member_parts(&self, node: Node) -> Option<(String, String)> {
        if node.kind() != "member_expression" {
            return None;
        }
        let object = node.child_by_field_name("object")?;
        let property = node.child_by_field_name("property")?;
        Some((
            self.text(object).replace(char::is_whitespace, ""),
            self.text(property).to_string(),
        ))
    }

    fn http_call(&self, call: Node<'a>, instances: &HashSet<String>) -> Option<HttpCall> {
        let function = call.child_by_field_name("function")?;
        let args = Self::args(call);
        // `axios\n  .get(…)`: the call is where the method is named.
        let call_line = function
            .child_by_field_name("property")
            .map(line)
            .unwrap_or_else(|| line(call));
        let mut out = HttpCall {
            line: call_line,
            ..HttpCall::default()
        };
        let callee = self.text(function).replace(char::is_whitespace, "");
        let member = self.member_parts(function);
        match (callee.as_str(), member.as_ref()) {
            ("fetch" | "window.fetch", _) => {
                out.client = "fetch";
                let (url, expr) = self.url(*args.first()?);
                out.url = url;
                out.url_expr = expr;
                let options = args.get(1).copied();
                out.method = Some(
                    options
                        .and_then(|o| self.property(o, "method"))
                        .and_then(|m| self.string_of(m))
                        .map(|m| m.to_ascii_uppercase())
                        .unwrap_or_else(|| "GET".into()),
                );
                if let Some(body) = options.and_then(|o| self.property(o, "body")) {
                    out.params = self.keys(body);
                }
            }
            ("axios", _) => {
                out.client = "axios";
                self.axios_config(&args, &mut out)?;
            }
            (_, Some((object, method)))
                if (object == "axios" || instances.contains(object))
                    && AXIOS_METHODS.contains(&method.as_str()) =>
            {
                out.client = if object == "axios" { "axios" } else { "axios-instance" };
                if object != "axios" {
                    out.via = Some(format!("{object}.{method}"));
                }
                self.axios_method(method, &args, &mut out)?;
            }
            (_, Some((object, method)))
                if matches!(object.as_str(), "$" | "jQuery")
                    && matches!(method.as_str(), "ajax" | "post" | "get" | "getJSON") =>
            {
                out.client = "jquery";
                self.jquery(method, &args, &mut out)?;
            }
            ("BX.ajax", _) => {
                out.client = "bx.ajax";
                let config = *args.first()?;
                let (url, expr) = self.url(self.property(config, "url")?);
                out.url = url;
                out.url_expr = expr;
                out.method = Some(
                    self.property(config, "method")
                        .and_then(|m| self.string_of(m))
                        .map(|m| m.to_ascii_uppercase())
                        .unwrap_or_else(|| "GET".into()),
                );
                if let Some(data) = self.property(config, "data") {
                    out.params = self.keys(data);
                }
            }
            ("BX.ajax.post" | "BX.ajax.get" | "BX.ajax.loadJSON", _) => {
                out.client = "bx.ajax";
                let (url, expr) = self.url(*args.first()?);
                out.url = url;
                out.url_expr = expr;
                out.method = Some(if callee.ends_with("post") { "POST" } else { "GET" }.into());
                if let Some(data) = args.get(1) {
                    out.params = self.keys(*data);
                }
            }
            ("BX.ajax.runComponentAction", _) => {
                out.client = "bx.runComponentAction";
                out.component = args.first().and_then(|a| self.string_of(*a));
                out.action = args.get(1).and_then(|a| self.string_of(*a));
                let config = args.get(2).copied();
                out.mode = Some(
                    config
                        .and_then(|c| self.property(c, "mode"))
                        .and_then(|m| self.string_of(m))
                        .unwrap_or_else(|| "ajax".into()),
                );
                out.method = Some(
                    config
                        .and_then(|c| self.property(c, "method"))
                        .and_then(|m| self.string_of(m))
                        .map(|m| m.to_ascii_uppercase())
                        .unwrap_or_else(|| "POST".into()),
                );
                if let Some(data) = config.and_then(|c| self.property(c, "data")) {
                    out.params = self.keys(data);
                }
            }
            ("BX.ajax.runAction", _) => {
                out.client = "bx.runAction";
                out.action = args.first().and_then(|a| self.string_of(*a));
                let config = args.get(1).copied();
                out.method = Some(
                    config
                        .and_then(|c| self.property(c, "method"))
                        .and_then(|m| self.string_of(m))
                        .map(|m| m.to_ascii_uppercase())
                        .unwrap_or_else(|| "POST".into()),
                );
                if let Some(data) = config.and_then(|c| self.property(c, "data")) {
                    out.params = self.keys(data);
                }
            }
            _ => {
                let wrapper = self.wrapper_for(function, member.as_ref())?;
                if let Some(action) = &wrapper.action {
                    out.client = action.client;
                    out.via = Some(callee.clone());
                    out.component = action
                        .component
                        .clone()
                        .or_else(|| action.component_param.and_then(|i| args.get(i)).and_then(|a| self.string_of(*a)));
                    out.action = action.action_param.and_then(|i| args.get(i)).and_then(|a| self.string_of(*a));
                    out.mode = action.mode.clone().or_else(|| (action.client == "bx.runComponentAction").then(|| "ajax".to_string()));
                    out.method = Some(action.method.clone().unwrap_or_else(|| "POST".into()));
                    if let Some(data) = wrapper.data_param.and_then(|i| args.get(i)) {
                        out.params = self.keys(*data);
                    }
                    self.reads(call, &mut out, if wrapper.returns_body { "" } else { "data" });
                    return Some(out);
                }
                if wrapper.instance {
                    let (object, method) = member?;
                    if !AXIOS_METHODS.contains(&method.as_str()) {
                        return None;
                    }
                    out.client = "axios-instance";
                    out.via = Some(format!("{object}.{method}"));
                    self.axios_method(&method, &args, &mut out)?;
                } else {
                    out.client = "wrapper";
                    out.via = Some(callee.clone());
                    let (url, expr) = self.url(*args.get(wrapper.url_param)?);
                    out.url = url;
                    out.url_expr = expr;
                    out.method = wrapper.method.clone();
                    if let Some(data) = wrapper.data_param.and_then(|i| args.get(i)) {
                        out.params = self.keys(*data);
                    }
                }
                self.parse_ajax_url(&mut out);
                self.reads(call, &mut out, if wrapper.returns_body { "" } else { "data" });
                return Some(out);
            }
        }
        self.parse_ajax_url(&mut out);
        let envelope = match out.client {
            "axios" | "axios-instance" | "bx.runComponentAction" | "bx.runAction" => "data",
            _ => "",
        };
        self.reads(call, &mut out, envelope);
        Some(out)
    }

    fn wrapper_for(&self, function: Node, member: Option<&(String, String)>) -> Option<Wrapper> {
        match member {
            Some((object, method)) => self.imports.wrapper(object, Some(method)).or_else(|| {
                self.local_wrappers
                    .borrow()
                    .iter()
                    .find(|w| w.name == format!("{object}.{method}"))
                    .cloned()
            }),
            None => {
                let name = self.text(function);
                self.local_wrappers
                    .borrow()
                    .iter()
                    .find(|w| !w.instance && w.name == name)
                    .cloned()
                    .or_else(|| self.imports.wrapper(name, None))
            }
        }
    }

    fn axios_config(&self, args: &[Node<'a>], out: &mut HttpCall) -> Option<()> {
        let first = *args.first()?;
        let config = if self.object_literal(first).is_some() {
            first
        } else {
            let (url, expr) = self.url(first);
            out.url = url;
            out.url_expr = expr;
            *args.get(1)?
        };
        if out.url.is_none() && out.url_expr.is_none() {
            let (url, expr) = self.url(self.property(config, "url")?);
            out.url = url;
            out.url_expr = expr;
        }
        out.method = Some(
            self.property(config, "method")
                .and_then(|m| self.string_of(m))
                .map(|m| m.to_ascii_uppercase())
                .unwrap_or_else(|| "GET".into()),
        );
        for key in ["data", "params"] {
            if let Some(v) = self.property(config, key) {
                out.params.extend(self.keys(v));
            }
        }
        Some(())
    }

    fn axios_method(&self, method: &str, args: &[Node<'a>], out: &mut HttpCall) -> Option<()> {
        if method == "request" {
            return self.axios_config(args, out);
        }
        let (url, expr) = self.url(*args.first()?);
        out.url = url;
        out.url_expr = expr;
        out.method = Some(method.to_ascii_uppercase());
        let with_body = matches!(method, "post" | "put" | "patch");
        let config = if with_body {
            if let Some(data) = args.get(1) {
                out.params = self.keys(*data);
            }
            args.get(2).copied()
        } else {
            args.get(1).copied()
        };
        if let Some(config) = config {
            for key in ["params", "data"] {
                if let Some(v) = self.property(config, key) {
                    out.params.extend(self.keys(v));
                }
            }
        }
        Some(())
    }

    fn jquery(&self, method: &str, args: &[Node<'a>], out: &mut HttpCall) -> Option<()> {
        let first = *args.first()?;
        if method == "ajax" {
            let settings = if self.object_literal(first).is_some() {
                first
            } else {
                let (url, expr) = self.url(first);
                out.url = url;
                out.url_expr = expr;
                *args.get(1)?
            };
            if out.url.is_none() && out.url_expr.is_none() {
                let (url, expr) = self.url(self.property(settings, "url")?);
                out.url = url;
                out.url_expr = expr;
            }
            out.method = Some(
                self.property(settings, "type")
                    .or_else(|| self.property(settings, "method"))
                    .and_then(|m| self.string_of(m))
                    .map(|m| m.to_ascii_uppercase())
                    .unwrap_or_else(|| "GET".into()),
            );
            if let Some(data) = self.property(settings, "data") {
                out.params = self.keys(data);
            }
            return Some(());
        }
        let (url, expr) = self.url(first);
        out.url = url;
        out.url_expr = expr;
        out.method = Some(if method == "post" { "POST" } else { "GET" }.into());
        if let Some(data) = args.get(1).filter(|d| !is_function(**d)) {
            out.params = self.keys(*data);
        }
        Some(())
    }

    /// `…/ajax.php?mode=class&c=vendor:name&action=x`: a component (or controller) action.
    fn parse_ajax_url(&self, out: &mut HttpCall) {
        let Some(url) = out.url.as_deref() else {
            return;
        };
        let Some((path, query)) = url.split_once('?') else {
            return;
        };
        if !path.ends_with("ajax.php") {
            return;
        }
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "c" if out.component.is_none() => out.component = Some(v.to_string()),
                "action" if out.action.is_none() => out.action = Some(v.to_string()),
                "mode" if out.mode.is_none() => out.mode = Some(v.to_string()),
                _ => {}
            }
        }
    }

    /// Paths the code reads from the answer of `call`: `.then(cb)` on it (through `.json()` for
    /// fetch), `await` into a variable or a destructuring, a variable `.then`-ed later, and the
    /// success callback of jQuery and `BX.ajax`.
    fn reads(&self, call: Node<'a>, out: &mut HttpCall, envelope: &str) {
        let mut holders: Vec<(Node<'a>, Node<'a>)> = Vec::new(); // (binding pattern, scope)
        // `.then(cb)` chains, `fetch().then(r => r.json()).then(data => …)` included.
        let mut current = call;
        let mut body_envelope = envelope.to_string();
        while let Some(member) = current.parent().filter(|p| p.kind() == "member_expression") {
            let Some(then_call) = member.parent().filter(|p| p.kind() == "call_expression") else {
                break;
            };
            let prop = member.child_by_field_name("property").map(|p| self.text(p));
            if prop != Some("then") {
                break;
            }
            if let Some(cb) = Self::args(then_call).into_iter().next().filter(|c| is_function(*c)) {
                if let Some(param) = first_param(cb) {
                    if out.client == "fetch" && returns_json(cb, self.src) {
                        body_envelope.clear();
                    } else {
                        holders.push((param, cb));
                    }
                }
            }
            current = then_call;
        }
        // `const r = await call` / `const { data } = await call` / `let request = call`.
        let mut up = call;
        while let Some(parent) = up.parent() {
            match parent.kind() {
                "await_expression" | "parenthesized_expression" => up = parent,
                "variable_declarator" => {
                    if let Some(name) = parent.child_by_field_name("name") {
                        let scope = enclosing_function(parent).unwrap_or(self.root);
                        holders.push((name, scope));
                        // `request.then(cb)` later.
                        if name.kind() == "identifier" {
                            for cb in self.then_callbacks(self.text(name), scope) {
                                if let Some(param) = first_param(cb) {
                                    holders.push((param, cb));
                                }
                            }
                        }
                    }
                    break;
                }
                _ => break,
            }
        }
        // Success callbacks.
        let args = Self::args(call);
        let options = args.iter().copied().find(|a| self.object_literal(*a).is_some());
        for key in ["success", "onsuccess"] {
            if let Some(cb) = options.and_then(|o| self.property(o, key)).filter(|c| is_function(*c)) {
                if let Some(param) = first_param(cb) {
                    holders.push((param, cb));
                }
            }
        }
        if matches!(out.client, "jquery" | "bx.ajax") {
            if let Some(cb) = args.last().filter(|c| is_function(**c)) {
                if let Some(param) = first_param(*cb) {
                    holders.push((param, *cb));
                }
            }
        }
        let mut reads = Vec::new();
        for (pattern, scope) in holders {
            match pattern.kind() {
                "identifier" => self.member_paths(self.text(pattern), scope, &mut reads),
                "object_pattern" => {
                    for (key, local) in pattern_keys(pattern, self.src) {
                        if !reads.contains(&key) {
                            reads.push(key.clone());
                        }
                        if let Some(local) = local {
                            let mut nested = Vec::new();
                            self.member_paths(&local, scope, &mut nested);
                            for n in nested {
                                let path = format!("{key}.{n}");
                                if !reads.contains(&path) {
                                    reads.push(path);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out.response_keys = reads
            .iter()
            .filter_map(|r| {
                if body_envelope.is_empty() {
                    Some(r.clone())
                } else {
                    r.strip_prefix(&format!("{body_envelope}."))
                        .map(str::to_string)
                }
            })
            .collect();
        out.reads = reads;
    }

    fn then_callbacks(&self, var: &str, scope: Node<'a>) -> Vec<Node<'a>> {
        let mut out = Vec::new();
        let mut stack = vec![scope];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                let target = node
                    .child_by_field_name("function")
                    .map(|f| self.text(f).replace(char::is_whitespace, ""));
                if target.as_deref() == Some(&format!("{var}.then")) {
                    if let Some(cb) = Self::args(node).into_iter().next().filter(|c| is_function(*c)) {
                        out.push(cb);
                    }
                }
            }
            push_children(&mut stack, node);
        }
        out
    }

    /// `var.a.b` chains in `scope`, as `a.b` (up to four levels, stopping at calls and indexes).
    fn member_paths(&self, var: &str, scope: Node<'a>, out: &mut Vec<String>) {
        let mut stack = vec![scope];
        while let Some(node) = stack.pop() {
            if node.kind() == "identifier" && self.text(node) == var {
                let mut path: Vec<String> = Vec::new();
                let mut current = node;
                while let Some(parent) = current.parent() {
                    if parent.kind() != "member_expression"
                        || parent.child_by_field_name("object") != Some(current)
                    {
                        break;
                    }
                    let Some(property) = parent.child_by_field_name("property") else {
                        break;
                    };
                    path.push(self.text(property).to_string());
                    current = parent;
                    if path.len() >= 4 {
                        break;
                    }
                }
                // A method on the end (`.map`, `.forEach`) is no key.
                if current
                    .parent()
                    .is_some_and(|p| p.kind() == "call_expression" && p.child_by_field_name("function") == Some(current))
                {
                    path.pop();
                }
                // `const { a } = var.data` reads data.a.
                if let Some(declarator) = current.parent().filter(|p| {
                    p.kind() == "variable_declarator" && p.child_by_field_name("value") == Some(current)
                }) {
                    if let Some(pattern) = declarator.child_by_field_name("name").filter(|n| n.kind() == "object_pattern") {
                        for (key, _) in pattern_keys(pattern, self.src) {
                            let mut full = path.clone();
                            full.push(key);
                            let joined = full.join(".");
                            if !out.contains(&joined) {
                                out.push(joined);
                            }
                        }
                        continue;
                    }
                }
                if path.is_empty() {
                    // `const payload = getPayload(response)`: one level into a local helper
                    // that returns a path of its parameter.
                    for (prefix, holder, holder_scope) in self.helper_results(node) {
                        let mut nested = Vec::new();
                        self.member_paths(&holder, holder_scope, &mut nested);
                        for n in nested {
                            let joined = if prefix.is_empty() { n } else { format!("{prefix}.{n}") };
                            if !out.contains(&joined) {
                                out.push(joined);
                            }
                        }
                    }
                }
                if !path.is_empty() {
                    let joined = path.join(".");
                    if !out.contains(&joined) {
                        out.push(joined);
                    }
                }
            }
            push_children(&mut stack, node);
        }
    }

    /// `const holder = helper(var)` where `helper` is a function of this script returning a path
    /// of its first parameter: (that path, holder name, holder scope).
    fn helper_results(&self, var_use: Node<'a>) -> Vec<(String, String, Node<'a>)> {
        let Some(arguments) = var_use.parent().filter(|p| p.kind() == "arguments") else {
            return Vec::new();
        };
        if named(arguments).first() != Some(&var_use) {
            return Vec::new();
        }
        let Some(call) = arguments.parent().filter(|p| p.kind() == "call_expression") else {
            return Vec::new();
        };
        let Some(callee) = call.child_by_field_name("function").filter(|f| f.kind() == "identifier") else {
            return Vec::new();
        };
        let Some(helper) = self.function_named(self.text(callee)) else {
            return Vec::new();
        };
        let Some(param) = first_param(helper).filter(|p| p.kind() == "identifier") else {
            return Vec::new();
        };
        let Some(prefix) = self.returned_path(helper, self.text(param)) else {
            return Vec::new();
        };
        let Some(declarator) = call.parent().filter(|p| p.kind() == "variable_declarator") else {
            return Vec::new();
        };
        let Some(holder) = declarator.child_by_field_name("name").filter(|n| n.kind() == "identifier") else {
            return Vec::new();
        };
        let scope = enclosing_function(declarator).unwrap_or(self.root);
        vec![(prefix, self.text(holder).to_string(), scope)]
    }

    fn function_named(&self, name: &str) -> Option<Node<'a>> {
        let mut stack = vec![self.root];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "function_declaration"
                    if node.child_by_field_name("name").is_some_and(|n| self.text(n) == name) =>
                {
                    return Some(node);
                }
                "variable_declarator" => {
                    if let (Some(n), Some(v)) = (node.child_by_field_name("name"), node.child_by_field_name("value")) {
                        if self.text(n) == name && is_function(v) {
                            return Some(v);
                        }
                    }
                }
                _ => {}
            }
            push_children(&mut stack, node);
        }
        None
    }

    /// The object literal a function returns: `() => ({ a })` or `return { a }`.
    fn returned_object(&self, function: Node<'a>) -> Option<Node<'a>> {
        let body = function.child_by_field_name("body")?;
        let expr = if body.kind() == "statement_block" {
            named(body)
                .into_iter()
                .find(|s| s.kind() == "return_statement")
                .and_then(|r| named(r).into_iter().next())?
        } else {
            body
        };
        self.object_literal(expr)
    }

    /// The member path of `param` a function returns: `return p?.data?.data ?? p?.data` → `data.data`.
    fn returned_path(&self, function: Node<'a>, param: &str) -> Option<String> {
        let body = function.child_by_field_name("body")?;
        let mut expr = if body.kind() == "statement_block" {
            named(body)
                .into_iter()
                .find(|s| s.kind() == "return_statement")
                .and_then(|r| named(r).into_iter().next())?
        } else {
            body
        };
        // The first alternative of `a ?? b || c`.
        while expr.kind() == "binary_expression" {
            expr = expr.child_by_field_name("left")?;
        }
        let mut path = Vec::new();
        let mut current = expr;
        while current.kind() == "member_expression" {
            path.push(self.text(current.child_by_field_name("property")?).to_string());
            current = current.child_by_field_name("object")?;
        }
        (current.kind() == "identifier" && self.text(current) == param).then(|| {
            path.reverse();
            path.join(".")
        })
    }

    /// A string that looks like a backend address outside any recognised call.
    fn literal_url(&self, node: Node<'a>) -> Option<String> {
        let (url, _) = self.url(node);
        let url = url?;
        let looks = url.starts_with("/api/") || url.contains("/api/v") || url.contains("ajax.php");
        looks.then_some(url)
    }

    /// `window.X = value`.
    fn global_write(&self, node: Node<'a>) -> Option<GlobalWrite> {
        let left = node.child_by_field_name("left")?;
        let (object, name) = self.member_parts(left)?;
        if object != "window" {
            return None;
        }
        let right = node.child_by_field_name("right")?;
        // Data handed to scripts: an object or array, a PHP value, parsed JSON — not functions,
        // libraries or `window.X || {}` namespaces.
        let text = self.text(right);
        let is_data = matches!(right.kind(), "object" | "array")
            || (right.kind() == "identifier" && text.starts_with("__php"))
            || (matches!(right.kind(), "string" | "template_string") && text.contains("__php"))
            || text.replace(char::is_whitespace, "").starts_with("JSON.parse(");
        // `window.handler<?= $id ?>`: a name built by PHP.
        if !is_data || name.contains("__php") {
            return None;
        }
        let keys = self
            .object_literal(right)
            .map(|_| self.keys(right).iter().map(|k| self.with_placeholders(k)).collect());
        Some(GlobalWrite {
            line: line(node),
            name,
            expr: if keys.is_none() {
                let shown = self.with_placeholders(text);
                Some(if shown.chars().count() > 200 {
                    format!("{}…", shown.chars().take(200).collect::<String>())
                } else {
                    shown
                })
            } else {
                None
            },
            keys,
        })
    }

    /// `window.X.a.b` or `X.a.b` (a name not declared in the script) for X in `globals`.
    fn global_read(&self, node: Node<'a>, globals: &HashSet<String>) -> Option<GlobalRead> {
        // Only the outermost member expression of a chain.
        if node
            .parent()
            .is_some_and(|p| p.kind() == "member_expression" && p.child_by_field_name("object") == Some(node))
        {
            return None;
        }
        let mut chain: Vec<String> = Vec::new();
        let mut current = node;
        loop {
            match current.kind() {
                "member_expression" => {
                    chain.push(self.text(current.child_by_field_name("property")?).to_string());
                    current = current.child_by_field_name("object")?;
                }
                "identifier" => {
                    chain.push(self.text(current).to_string());
                    break;
                }
                _ => return None,
            }
        }
        chain.reverse();
        let (name, path) = if chain.first().map(String::as_str) == Some("window") {
            (chain.get(1)?.clone(), chain[2..].to_vec())
        } else {
            let name = chain.first()?.clone();
            if !globals.contains(&name) || self.locally_bound(&name, node) {
                return None;
            }
            (name, chain[1..].to_vec())
        };
        if !globals.contains(&name) || path.is_empty() {
            return None;
        }
        // An assignment target is a write.
        if node
            .parent()
            .is_some_and(|p| p.kind() == "assignment_expression" && p.child_by_field_name("left") == Some(node))
        {
            return None;
        }
        let mut path = path;
        if node
            .parent()
            .is_some_and(|p| p.kind() == "call_expression" && p.child_by_field_name("function") == Some(node))
        {
            path.pop();
        }
        if path.is_empty() {
            return None;
        }
        Some(GlobalRead {
            line: line(node),
            name,
            path: path.join("."),
        })
    }

    /// `const { a, b } = window.X` (or `= X.sub`) for X in `globals`.
    fn global_destructuring(&self, declarator: Node<'a>, globals: &HashSet<String>) -> Vec<GlobalRead> {
        let (Some(pattern), Some(value)) = (
            declarator.child_by_field_name("name"),
            declarator.child_by_field_name("value"),
        ) else {
            return Vec::new();
        };
        if pattern.kind() != "object_pattern" {
            return Vec::new();
        }
        let chain: Vec<&str> = self.text(value).split('.').map(str::trim).collect();
        let (name, prefix) = match chain.as_slice() {
            ["window", name, rest @ ..] => (name.to_string(), rest.to_vec()),
            [name, rest @ ..] if globals.contains(*name) && !self.locally_bound(name, declarator) => {
                (name.to_string(), rest.to_vec())
            }
            _ => return Vec::new(),
        };
        if !globals.contains(&name) || prefix.iter().any(|p| !p.chars().all(|c| c.is_alphanumeric() || c == '_')) {
            return Vec::new();
        }
        pattern_keys(pattern, self.src)
            .into_iter()
            .map(|(key, _)| {
                let mut path: Vec<String> = prefix.iter().map(|p| p.to_string()).collect();
                path.push(key);
                GlobalRead {
                    line: line(declarator),
                    name: name.clone(),
                    path: path.join("."),
                }
            })
            .collect()
    }

    /// Functions forwarding a parameter as the address of a backend call, and instances.
    fn wrappers(&self, instances: &HashSet<String>) -> Vec<Wrapper> {
        let mut out: Vec<Wrapper> = instances
            .iter()
            .map(|name| Wrapper {
                name: name.clone(),
                url_param: 0,
                data_param: None,
                method: None,
                returns_body: false,
                instance: true,
                action: None,
            })
            .collect();
        let mut stack = vec![self.root];
        while let Some(node) = stack.pop() {
            let function = match node.kind() {
                "function_declaration" => node.child_by_field_name("name").map(|n| (self.text(n).to_string(), node)),
                "variable_declarator" => match (node.child_by_field_name("name"), node.child_by_field_name("value")) {
                    (Some(n), Some(v)) if n.kind() == "identifier" && is_function(v) => Some((self.text(n).to_string(), v)),
                    _ => None,
                },
                _ => None,
            };
            if let Some((name, function)) = function {
                if let Some(w) = self.wrapper_of(&name, function, instances) {
                    out.push(w);
                }
            }
            push_children(&mut stack, node);
        }
        out
    }

    fn wrapper_of(&self, name: &str, function: Node<'a>, instances: &HashSet<String>) -> Option<Wrapper> {
        let params: Vec<String> = function
            .child_by_field_name("parameters")
            .map(named)
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                let target = p.child_by_field_name("pattern").unwrap_or(p);
                self.text(target).to_string()
            })
            .collect();
        if params.is_empty() {
            return None;
        }
        let body = function.child_by_field_name("body")?;
        if let Some(w) = self.action_wrapper(name, body, &params) {
            return Some(w);
        }
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                if let Some(inner) = self.http_call_shape(node, instances) {
                    let args = Self::args(node);
                    let url_arg = args.first()?;
                    if url_arg.kind() == "identifier" {
                        if let Some(url_param) = params.iter().position(|p| p == self.text(*url_arg)) {
                            // The data parameter: passed as is, or inside the config object
                            // (`api.get(url, { params })`).
                            let data_param = args.get(1).and_then(|d| {
                                let candidates: Vec<Node> = if d.kind() == "object" {
                                    named(*d)
                                        .into_iter()
                                        .filter_map(|m| match m.kind() {
                                            "shorthand_property_identifier" => Some(m),
                                            "pair" => m.child_by_field_name("value"),
                                            _ => None,
                                        })
                                        .collect()
                                } else {
                                    vec![*d]
                                };
                                candidates.iter().find_map(|c| {
                                    matches!(c.kind(), "identifier" | "shorthand_property_identifier")
                                        .then(|| params.iter().position(|p| p == self.text(*c)))
                                        .flatten()
                                })
                            });
                            return Some(Wrapper {
                                name: name.to_string(),
                                url_param,
                                data_param,
                                method: inner,
                                returns_body: returns_data(body, self.src),
                                instance: false,
                                action: None,
                            });
                        }
                    }
                }
            }
            push_children(&mut stack, node);
        }
        None
    }

    /// A function passing a parameter as the action (or component) of `BX.ajax.runComponentAction`
    /// or `BX.ajax.runAction`.
    fn action_wrapper(&self, name: &str, body: Node<'a>, params: &[String]) -> Option<Wrapper> {
        let param_of = |n: Node| -> Option<usize> {
            matches!(n.kind(), "identifier" | "shorthand_property_identifier")
                .then(|| params.iter().position(|p| p == self.text(n)))
                .flatten()
        };
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                let callee = node
                    .child_by_field_name("function")
                    .map(|f| self.text(f).replace(char::is_whitespace, ""))
                    .unwrap_or_default();
                let args = Self::args(node);
                let (client, component_arg, action_arg, config) = match callee.as_str() {
                    "BX.ajax.runComponentAction" => {
                        ("bx.runComponentAction", args.first().copied(), args.get(1).copied(), args.get(2).copied())
                    }
                    "BX.ajax.runAction" => ("bx.runAction", None, args.first().copied(), args.get(1).copied()),
                    _ => {
                        push_children(&mut stack, node);
                        continue;
                    }
                };
                let component_param = component_arg.and_then(param_of);
                let action_param = action_arg.and_then(param_of);
                if component_param.is_none() && action_param.is_none() {
                    return None;
                }
                let data_param = config.and_then(|c| self.property(c, "data")).and_then(param_of);
                let config_string = |key: &str| config.and_then(|c| self.property(c, key)).and_then(|v| self.string_of(v));
                return Some(Wrapper {
                    name: name.to_string(),
                    url_param: 0,
                    data_param,
                    method: config_string("method").map(|m| m.to_ascii_uppercase()),
                    returns_body: returns_data(body, self.src),
                    instance: false,
                    action: Some(ActionWrapper {
                        client,
                        component: if component_param.is_none() {
                            component_arg.and_then(|c| self.string_of(c))
                        } else {
                            None
                        },
                        component_param,
                        action_param,
                        mode: config_string("mode"),
                        method: config_string("method").map(|m| m.to_ascii_uppercase()),
                    }),
                });
            }
            push_children(&mut stack, node);
        }
        None
    }

    /// The method of a client call by shape alone (`axios.post`, `api.get`, `fetch`), if it is one.
    fn http_call_shape(&self, call: Node<'a>, instances: &HashSet<String>) -> Option<Option<String>> {
        let function = call.child_by_field_name("function")?;
        let callee = self.text(function).replace(char::is_whitespace, "");
        if callee == "fetch" {
            return Some(None);
        }
        let (object, method) = self.member_parts(function)?;
        let client = object == "axios" || instances.contains(&object) || matches!(object.as_str(), "$" | "jQuery");
        (client && (AXIOS_METHODS.contains(&method.as_str()) || matches!(method.as_str(), "ajax" | "getJSON")))
            .then(|| (method != "request" && method != "ajax").then(|| method.to_ascii_uppercase()))
    }

    /// Wrappers reachable from other modules: exported functions, members of a default-exported
    /// object, exported instances.
    fn exported_wrappers(&self, wrappers: &[Wrapper], instances: &HashSet<String>) -> Vec<Wrapper> {
        let mut out = Vec::new();
        for statement in named(self.root) {
            if statement.kind() != "export_statement" {
                continue;
            }
            if let Some(declaration) = statement.child_by_field_name("declaration") {
                let names: Vec<String> = match declaration.kind() {
                    "function_declaration" => declaration
                        .child_by_field_name("name")
                        .map(|n| vec![self.text(n).to_string()])
                        .unwrap_or_default(),
                    "lexical_declaration" | "variable_declaration" => named(declaration)
                        .into_iter()
                        .filter_map(|d| d.child_by_field_name("name"))
                        .map(|n| self.text(n).to_string())
                        .collect(),
                    _ => Vec::new(),
                };
                for name in names {
                    if let Some(w) = wrappers.iter().find(|w| w.name == name) {
                        out.push(w.clone());
                    }
                }
            }
            if let Some(value) = statement.child_by_field_name("value") {
                if value.kind() == "identifier" && instances.contains(self.text(value)) {
                    out.push(Wrapper {
                        name: "default".into(),
                        url_param: 0,
                        data_param: None,
                        method: None,
                        returns_body: false,
                        instance: true,
                        action: None,
                    });
                }
                if value.kind() == "object" {
                    for member in named(value) {
                        let (key, local) = match member.kind() {
                            "shorthand_property_identifier" => (self.text(member).to_string(), self.text(member).to_string()),
                            "pair" => match (member.child_by_field_name("key"), member.child_by_field_name("value")) {
                                (Some(k), Some(v)) if v.kind() == "identifier" => {
                                    (self.text(k).to_string(), self.text(v).to_string())
                                }
                                _ => continue,
                            },
                            _ => continue,
                        };
                        if let Some(w) = wrappers.iter().find(|w| w.name == local) {
                            out.push(Wrapper {
                                name: format!("default.{key}"),
                                ..w.clone()
                            });
                        }
                    }
                }
            }
        }
        out
    }

    fn exported_constants(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for statement in named(self.root) {
            if statement.kind() != "export_statement" {
                continue;
            }
            let Some(declaration) = statement.child_by_field_name("declaration") else {
                continue;
            };
            if declaration.kind() != "lexical_declaration" {
                continue;
            }
            for declarator in named(declaration) {
                if let (Some(n), Some(v)) = (declarator.child_by_field_name("name"), declarator.child_by_field_name("value")) {
                    if let Some(s) = self.string_of(v) {
                        out.push((self.text(n).to_string(), s));
                    }
                }
            }
        }
        out
    }
}

/// Children pushed so that a stack pops them in source order.
fn push_children<'t>(stack: &mut Vec<Node<'t>>, node: Node<'t>) {
    let mut children = named(node);
    children.reverse();
    stack.extend(children);
}

fn is_function(node: Node) -> bool {
    matches!(node.kind(), "arrow_function" | "function_expression" | "function")
}

fn first_param(function: Node) -> Option<Node> {
    if let Some(single) = function.child_by_field_name("parameter") {
        return Some(single); // `r => …`
    }
    let params = function.child_by_field_name("parameters")?;
    let first = named(params).into_iter().next()?;
    Some(first.child_by_field_name("pattern").unwrap_or(first))
}

/// `r => r.json()` / `function (r) { return r.json() }`.
fn returns_json(function: Node, src: &str) -> bool {
    function
        .child_by_field_name("body")
        .is_some_and(|b| b.utf8_text(src.as_bytes()).unwrap_or("").contains(".json()"))
}

/// A wrapper body that returns `resp.data` (or `.then(r => r.data)`).
fn returns_data(body: Node, src: &str) -> bool {
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node.kind() == "return_statement" {
            let text = node.utf8_text(src.as_bytes()).unwrap_or("");
            if text.trim_end_matches(';').trim_end().ends_with(".data") {
                return true;
            }
        }
        push_children(&mut stack, node);
    }
    false
}

/// `{ a, b: c, d = 1 }` → (key, local name).
fn pattern_keys(pattern: Node, src: &str) -> Vec<(String, Option<String>)> {
    let text = |n: Node| n.utf8_text(src.as_bytes()).unwrap_or("").to_string();
    let mut out = Vec::new();
    for member in named(pattern) {
        match member.kind() {
            "shorthand_property_identifier_pattern" => out.push((text(member), Some(text(member)))),
            "pair_pattern" => {
                if let Some(k) = member.child_by_field_name("key") {
                    let local = member
                        .child_by_field_name("value")
                        .filter(|v| v.kind() == "identifier")
                        .map(text);
                    out.push((text(k), local));
                }
            }
            "object_assignment_pattern" => {
                if let Some(left) = member.child_by_field_name("left") {
                    out.push((text(left), Some(text(left))));
                }
            }
            _ => {}
        }
    }
    out
}

fn enclosing_function(node: Node) -> Option<Node> {
    let mut current = node.parent();
    while let Some(n) = current {
        if matches!(
            n.kind(),
            "function_declaration" | "function_expression" | "arrow_function" | "method_definition" | "function"
        ) {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

fn enclosing_function_name(node: Node, src: &str) -> Option<String> {
    let function = enclosing_function(node)?;
    let name = match function.kind() {
        "function_declaration" | "method_definition" => function.child_by_field_name("name"),
        _ => function
            .parent()
            .filter(|p| p.kind() == "variable_declarator")
            .and_then(|p| p.child_by_field_name("name")),
    }?;
    Some(name.utf8_text(src.as_bytes()).unwrap_or("").to_string())
}

// ---------------------------------------------------------------------------
// Inline scripts of PHP templates
// ---------------------------------------------------------------------------

/// The `<script>` blocks of a PHP template as one JS document (everything else blanked, lines
/// kept), PHP islands replaced by `__phpN__`, and what each island shows: `{php:$expr}` for an
/// echo, `{php}` for other code.
pub fn template_scripts(content: &str) -> Option<(String, Vec<String>)> {
    if !content.contains("<script") {
        return None;
    }
    let mut masked = String::with_capacity(content.len());
    let mut placeholders = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<?") {
        masked.push_str(&rest[..start]);
        let island = &rest[start..];
        let end = island.find("?>").map(|e| e + 2).unwrap_or(island.len());
        let code = &island[..end];
        let shown = echo_expression(code)
            .map(|e| format!("{{php:{e}}}"))
            .unwrap_or_else(|| "{php}".to_string());
        masked.push_str(&format!("__php{}__", placeholders.len()));
        placeholders.push(shown);
        for _ in 0..code.matches('\n').count() {
            masked.push('\n');
        }
        rest = &island[end..];
    }
    masked.push_str(rest);

    let lower = masked.to_ascii_lowercase();
    let mut script = String::with_capacity(masked.len());
    let mut position = 0;
    let mut found = false;
    while let Some(open) = lower[position..].find("<script") {
        let open = position + open;
        let Some(tag_end) = lower[open..].find('>').map(|e| open + e + 1) else {
            break;
        };
        let tag = &lower[open..tag_end];
        let close = lower[tag_end..].find("</script").map(|c| tag_end + c).unwrap_or(lower.len());
        let is_js = !tag.contains(" src=")
            && (!tag.contains("type=") || tag.contains("javascript") || tag.contains("module"));
        blank_into(&mut script, &masked[position..tag_end]);
        if is_js {
            script.push_str(&masked[tag_end..close]);
            found = true;
        } else {
            blank_into(&mut script, &masked[tag_end..close]);
        }
        position = close;
    }
    blank_into(&mut script, &masked[position..]);
    found.then_some((script, placeholders))
}

fn blank_into(out: &mut String, text: &str) {
    for c in text.chars() {
        out.push(if c == '\n' { '\n' } else { ' ' });
    }
}

/// `<?= $x ?>`, `<?php echo $x; ?>` → `$x`.
fn echo_expression(code: &str) -> Option<String> {
    let inner = code.strip_prefix("<?").unwrap_or(code);
    let inner = inner.strip_suffix("?>").unwrap_or(inner);
    let expr = if let Some(e) = inner.strip_prefix('=') {
        e
    } else {
        let t = inner.trim_start();
        let t = t.strip_prefix("php").unwrap_or(t).trim_start();
        t.strip_prefix("echo ")?
    };
    let expr = expr.trim().trim_end_matches(';').trim();
    (!expr.is_empty() && !expr.contains('\n')).then(|| expr.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls(script: &str) -> Vec<HttpCall> {
        analyze(script, &NoImports, &[], &HashSet::new()).calls
    }

    #[test]
    fn axios_get_with_params_and_response_keys() {
        let script = r#"
export default {
  methods: {
    getSalonsRequest(offerId) {
      axios
        .get('/api/v1/delivery/reserve-salons/', {
          params: { offerId: offerId, userLat: this.latitude },
        })
        .then((response) => {
          this.salons = response.data.data ?? []
        })
    },
  },
}
"#;
        let found = calls(script);
        assert_eq!(found.len(), 1);
        let call = &found[0];
        assert_eq!(call.line, 6, "the line of `.get(`");
        assert_eq!(call.client, "axios");
        assert_eq!(call.method.as_deref(), Some("GET"));
        assert_eq!(call.url.as_deref(), Some("/api/v1/delivery/reserve-salons/"));
        assert_eq!(call.params, vec!["offerId", "userLat"]);
        assert_eq!(call.reads, vec!["data.data"]);
        assert_eq!(call.response_keys, vec!["data"]);
    }

    #[test]
    fn axios_config_with_constant_and_template_address() {
        let script = r#"
const BASKET_ITEMS_ENDPOINT = '/api/v1/basket/items'
async function remove(basketItemId) {
  const response = await axios({
    method: 'DELETE',
    url: `${BASKET_ITEMS_ENDPOINT}/${basketItemId}`,
  })
  return response.data.summary
}
async function add(payload) {
  const { data } = await axios({ method: 'POST', url: BASKET_ITEMS_ENDPOINT, data: { offerId: payload.id, quantity: 1 } })
  return data.basket
}
"#;
        let found = calls(script);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].method.as_deref(), Some("DELETE"));
        assert_eq!(found[0].url.as_deref(), Some("/api/v1/basket/items/{basketItemId}"));
        assert_eq!(found[0].response_keys, vec!["summary"]);
        assert_eq!(found[1].method.as_deref(), Some("POST"));
        assert_eq!(found[1].url.as_deref(), Some("/api/v1/basket/items"));
        assert_eq!(found[1].params, vec!["offerId", "quantity"]);
        assert_eq!(found[1].reads, vec!["data", "data.basket"]);
        assert_eq!(found[1].response_keys, vec!["basket"]);
    }

    #[test]
    fn bitrix_component_action_and_ajax_php_address() {
        let script = r#"
let request = BX.ajax.runComponentAction('orteka:users.list', 'getFilteredLists', {
  data: {filters: filters},
  mode:'class',
  method: 'POST'
});
request.then(function(response) {
  for (let key in response.data.RESULT) {}
});
$.post('/bitrix/services/main/ajax.php?mode=ajax&c=orteka:catalog.element&action=getAvailable', {offerId: 1}, function (res) { res.status; });
"#;
        let found = calls(script);
        assert_eq!(found.len(), 2);
        let bx = &found[0];
        assert_eq!(bx.client, "bx.runComponentAction");
        assert_eq!(bx.component.as_deref(), Some("orteka:users.list"));
        assert_eq!(bx.action.as_deref(), Some("getFilteredLists"));
        assert_eq!(bx.mode.as_deref(), Some("class"));
        assert_eq!(bx.params, vec!["filters"]);
        assert_eq!(bx.response_keys, vec!["RESULT"]);
        let jq = &found[1];
        assert_eq!(jq.client, "jquery");
        assert_eq!(jq.method.as_deref(), Some("POST"));
        assert_eq!(jq.component.as_deref(), Some("orteka:catalog.element"));
        assert_eq!(jq.action.as_deref(), Some("getAvailable"));
        assert_eq!(jq.reads, vec!["status"]);
    }

    #[test]
    fn local_wrapper_and_instance_are_followed() {
        let script = r#"
const api = axios.create({ baseURL: '' })
export async function post(url, data) {
  const resp = await api.post(url, data)
  return resp.data
}
export default { api, post }
post('/api/v1/share/file/upload/', { file: 1 })
"#;
        let facts = analyze(script, &NoImports, &[], &HashSet::new());
        assert_eq!(facts.calls.len(), 1, "{:#?}", facts.calls);
        let call = &facts.calls[0];
        assert_eq!(call.client, "wrapper");
        assert_eq!(call.url.as_deref(), Some("/api/v1/share/file/upload/"));
        assert_eq!(call.method.as_deref(), Some("POST"));
        assert_eq!(call.params, vec!["file"]);
        let names: Vec<&str> = facts.wrappers.iter().map(|w| w.name.as_str()).collect();
        assert!(names.contains(&"post") && names.contains(&"default.post"), "{names:?}");
        assert!(facts.wrappers.iter().all(|w| w.name != "default.post" || w.returns_body));
    }

    #[test]
    fn reads_go_one_level_into_a_local_payload_helper() {
        let script = r#"
function getApiPayload(response) {
  return response?.data?.data ?? response?.data ?? {}
}
async function add() {
  const response = await axios({ method: 'post', url: '/api/v1/basket/items' })
  const payload = getApiPayload(response)
  if (payload.success === true) { apply(payload.summary) }
}
"#;
        let found = calls(script);
        assert_eq!(found[0].reads, vec!["data.data.success", "data.data.summary"]);
        assert_eq!(found[0].response_keys, vec!["data.success", "data.summary"]);
    }

    #[test]
    fn functions_and_namespaces_on_window_are_no_data_globals() {
        let script = "window.$ = jQuery\nwindow.Ns = window.Ns || {}\nwindow.handler = function () {}\nwindow.config = { a: 1, b: 2 }\n";
        let facts = analyze(script, &NoImports, &[], &HashSet::new());
        assert_eq!(facts.global_writes.len(), 1);
        assert_eq!(facts.global_writes[0].name, "config");
        assert_eq!(facts.global_writes[0].keys, Some(vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn component_action_wrapper_with_component_in_a_constant() {
        let script = r#"
(function () {
    const COMPONENT_NAME = 'orteka:small.basket';
    const buildParams = (root) => ({
        siteId: root.getAttribute('data-site-id') || 's1',
        showDelay: 'Y',
    });
    const runAction = (action, data) => BX.ajax.runComponentAction(COMPONENT_NAME, action, {
        mode: 'class',
        data: data,
    });
    const load = async (root) => {
        const response = await runAction('load', buildParams(root));
        const payload = response?.data?.data || {};
        setContent(root, payload.html);
    };
})();
"#;
        let found = calls(script);
        assert_eq!(found.len(), 1, "{found:#?}");
        let call = &found[0];
        assert_eq!(call.client, "bx.runComponentAction");
        assert_eq!(call.via.as_deref(), Some("runAction"));
        assert_eq!(call.component.as_deref(), Some("orteka:small.basket"));
        assert_eq!(call.action.as_deref(), Some("load"));
        assert_eq!(call.mode.as_deref(), Some("class"));
        assert_eq!(call.method.as_deref(), Some("POST"));
        assert_eq!(call.params, vec!["siteId", "showDelay"]);
        assert_eq!(call.line, 13);
    }

    #[test]
    fn fetch_json_body_reads_without_envelope() {
        let script = r#"
fetch('/api/v1/location/city-center/?id=' + cityId, { method: 'post', body: JSON.stringify({ cityId }) })
  .then(r => r.json())
  .then(data => { show(data.center.lat) })
"#;
        let found = calls(script);
        assert_eq!(found[0].url.as_deref(), Some("/api/v1/location/city-center/?id={cityId}"));
        assert_eq!(found[0].method.as_deref(), Some("POST"));
        assert_eq!(found[0].params, vec!["cityId"]);
        assert_eq!(found[0].response_keys, vec!["center.lat"]);
    }

    #[test]
    fn php_template_scripts_keep_lines_and_show_php_values() {
        let content = "<?php\n$x = 1;\n?>\n<div><?= $title ?></div>\n<script>\n  $.post('<?= $templateFolder ?>/ajax.php', {id: 1});\n  window.dataApp = <?= Json::encode($arResult) ?>;\n</script>\n";
        let (script, placeholders) = template_scripts(content).unwrap();
        let mut globals = HashSet::new();
        globals.insert("dataApp".to_string());
        let facts = analyze(&script, &NoImports, &placeholders, &globals);
        assert_eq!(facts.calls.len(), 1);
        assert_eq!(facts.calls[0].line, 6);
        assert_eq!(facts.calls[0].url.as_deref(), Some("{php:$templateFolder}/ajax.php"));
        assert_eq!(facts.global_writes.len(), 1);
        assert_eq!(facts.global_writes[0].name, "dataApp");
        assert_eq!(facts.global_writes[0].expr.as_deref(), Some("{php:Json::encode($arResult)}"));
    }

    #[test]
    fn global_reads_of_known_names() {
        let script = "productStore.fill(window.dataApp.products)\nconst city = window.dataApp.delivery.current_city\nconst { offers } = dataApp\n";
        let mut globals = HashSet::new();
        globals.insert("dataApp".to_string());
        let facts = analyze(script, &NoImports, &[], &globals);
        let paths: Vec<&str> = facts.global_reads.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["products", "delivery.current_city", "offers"]);
    }
}
