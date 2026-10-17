//! Backend calls a JS/TS/Vue script makes: `fetch`, `axios`, an `axios.create()` instance, a
//! project wrapper around one of those, a client object whose methods forward to one of those
//! (what a client factory returns), jQuery `$.ajax`/`$.post`/`$.get`, `BX.ajax`,
//! `BX.ajax.runComponentAction`, `BX.ajax.runAction` — with the method, the address folded from
//! literals, template strings, constants, maps and local helpers (the rest as `{…}`), the keys
//! sent, and the keys the code reads from the answer. Also `window.X = …` writes and
//! `window.X.key` reads, which carry data from a PHP template to scripts.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

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
    /// The addresses when a part of it is a map indexed by a variable, one per value:
    /// `analyze` reports a call for each.
    candidates: Vec<String>,
}

/// Where a part of a backend call comes from, seen from the function that makes it: fixed
/// there, or taken from the function's arguments (`config.url` → argument 0, path `url`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Slot {
    /// Not given: the client's default applies (GET, nothing sent).
    #[default]
    Absent,
    /// Computed in a way not followed.
    Unknown,
    Lit(String),
    Arg(usize, Vec<String>),
    /// An object literal of such values, with its spreads: `{ ...config, method: 'get', url }`.
    Object(Vec<(String, Slot)>, Vec<Slot>),
    /// The first, or the second when the first is not given: `path || '/api/default/'`.
    Or(Box<Slot>, Box<Slot>),
}

impl Slot {
    fn get(&self, key: &str) -> Slot {
        match self {
            Slot::Arg(i, path) => {
                let mut path = path.clone();
                path.push(key.to_string());
                Slot::Arg(*i, path)
            }
            Slot::Object(fields, spreads) => {
                if let Some((_, value)) = fields.iter().rev().find(|(k, _)| k == key) {
                    return value.clone();
                }
                for spread in spreads.iter().rev() {
                    match spread.get(key) {
                        Slot::Absent => continue,
                        found => return found,
                    }
                }
                Slot::Absent
            }
            Slot::Or(first, second) => Slot::Or(Box::new(first.get(key)), Box::new(second.get(key))),
            Slot::Absent => Slot::Absent,
            _ => Slot::Unknown,
        }
    }

    /// The same seen from the caller, which passes `args`.
    fn subst(&self, args: &[Slot]) -> Slot {
        match self {
            Slot::Arg(i, path) => path
                .iter()
                .fold(args.get(*i).cloned().unwrap_or_default(), |slot, key| slot.get(key)),
            Slot::Object(fields, spreads) => Slot::Object(
                fields.iter().map(|(k, v)| (k.clone(), v.subst(args))).collect(),
                spreads.iter().map(|s| s.subst(args)).collect(),
            ),
            Slot::Or(first, second) => Slot::Or(Box::new(first.subst(args)), Box::new(second.subst(args))),
            other => other.clone(),
        }
    }

    fn depends_on_args(&self) -> bool {
        match self {
            Slot::Arg(..) => true,
            Slot::Object(fields, spreads) => fields.iter().any(|(_, v)| v.depends_on_args()) || spreads.iter().any(Slot::depends_on_args),
            Slot::Or(first, second) => first.depends_on_args() || second.depends_on_args(),
            _ => false,
        }
    }
}

/// How a function's arguments become the backend call it makes: `get(url, config)` that calls
/// `request({ ...config, method: 'get', url })` fixes the method, takes the address from
/// argument 0 and the query from `params` of argument 1.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forward {
    pub method: Slot,
    pub url: Slot,
    pub body: Slot,
    pub query: Slot,
    /// It resolves to the answer's body (`return response.data`), so its callers read no
    /// envelope.
    pub returns_body: bool,
    /// The address when the caller leaves it out: the parameter's default value.
    pub url_default: Option<String>,
}

impl Forward {
    fn subst(&self, args: &[Slot]) -> Forward {
        Forward {
            method: self.method.subst(args),
            url: self.url.subst(args),
            body: self.body.subst(args),
            query: self.query.subst(args),
            returns_body: self.returns_body,
            url_default: self.url_default.clone(),
        }
    }

    /// The address comes from the arguments: its calls are reported where it is called.
    fn forwards(&self) -> bool {
        match &self.url {
            Slot::Arg(..) => true,
            Slot::Or(first, _) => matches!(**first, Slot::Arg(..)),
            _ => false,
        }
    }
}

/// A function that forwards one of its parameters as the address of a backend call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapper {
    /// The export it is reachable by: `get`, or `default.get` for a default-exported object.
    pub name: String,
    /// How its arguments become the call; unused for an instance.
    pub forward: Forward,
    /// Not a function but an `axios.create()` instance: its methods are axios methods.
    pub instance: bool,
    /// A wrapper around `BX.ajax.runComponentAction` / `runAction`: that client, with the
    /// component fixed or taken from a parameter, the action from a parameter.
    pub action: Option<ActionWrapper>,
}

/// Methods of a client object by name, with what each forwards.
pub type Methods = Vec<(String, Forward)>;

/// An object whose methods forward to an HTTP client: what a client factory returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// The factory, or the object's name, that calls through it show in `via`.
    pub origin: String,
    pub methods: Methods,
}

impl Client {
    fn method(&self, name: &str) -> Option<Forward> {
        self.methods.iter().find(|(m, _)| m == name).map(|(_, f)| f.clone())
    }
}

/// A client object, or a factory returning one, that a module exports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedClient {
    /// The export: a name or `default`.
    pub name: String,
    pub factory: bool,
    pub client: Client,
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
    pub clients: Vec<ExportedClient>,
    /// Exported string constants: name → value, `NAME.key` for the values of an object.
    pub constants: Vec<(String, String)>,
    /// Imported constants the addresses of calls were folded from: (local name — `NAME` or
    /// `NAME.key`, every value of an object). Their strings are no addresses outside calls.
    pub imported_constants: Vec<(String, bool)>,
    pub literals: Vec<LiteralUrl>,
    pub global_writes: Vec<GlobalWrite>,
    pub global_reads: Vec<GlobalRead>,
}

/// What the script's imports bring, as resolved by the caller.
pub trait Imports {
    /// The string value of an imported constant bound to `local`, or of its key
    /// (`ENDPOINTS.list`).
    fn constant(&self, local: &str) -> Option<String>;
    /// The wrapper (or instance) an imported `local` is, or its `member` for an imported
    /// object (`request.get`).
    fn wrapper(&self, local: &str, member: Option<&str>) -> Option<Wrapper>;
    /// The string values of an imported object constant.
    fn constants_of(&self, _local: &str) -> Vec<String> {
        Vec::new()
    }
    /// The client object an imported `local` (or its `member`) is, or returns when `called`.
    fn client(&self, _local: &str, _member: Option<&str>, _called: bool) -> Option<Client> {
        None
    }
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
    run(script, imports, placeholders, globals, true)
}

/// What a script gives other scripts: exported wrappers, clients and constants, and the
/// `window.X` globals it writes. Its own calls are not looked for.
pub fn exports(script: &str, imports: &dyn Imports, placeholders: &[String]) -> ScriptFacts {
    run(script, imports, placeholders, &HashSet::new(), false)
}

fn run(
    script: &str,
    imports: &dyn Imports,
    placeholders: &[String],
    globals: &HashSet<String>,
    calls: bool,
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
        local_wrappers: RefCell::new(HashMap::new()),
        declared_names: RefCell::new(None),
        instances: RefCell::new(HashSet::new()),
        forwards: RefCell::new(HashMap::new()),
        factories: RefCell::new(HashMap::new()),
        objects: RefCell::new(HashMap::new()),
        folded: RefCell::new(Vec::new()),
        folded_imports: RefCell::new(Vec::new()),
        scopes: RefCell::new(HashMap::new()),
        receivers: RefCell::new(HashMap::new()),
        called: RefCell::new(HashSet::new()),
    };
    let mut facts = ScriptFacts::default();
    let instances = ctx.instances();
    *ctx.instances.borrow_mut() = instances.clone();
    // Local wrappers first: calls through them in the same file are calls too.
    let wrappers = ctx.wrappers(&instances);
    let mut in_call: HashSet<(usize, usize)> = HashSet::new();
    // Strings an address of a call was folded from (constants, map values, helpers): no
    // longer addresses outside calls.
    let mut used: HashSet<usize> = HashSet::new();
    let mut strings: Vec<Node> = Vec::new();
    // Calls inside a forwarding method reported at its `this.method()` calls, if there are any.
    let mut deferred: Vec<(HttpCall, usize)> = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if calls && node.kind() == "call_expression" {
            ctx.folded.borrow_mut().clear();
            ctx.folded_imports.borrow_mut().clear();
            if let Some(call) = ctx.http_call(node, &instances) {
                in_call.insert((node.start_byte(), node.end_byte()));
                used.extend(ctx.folded.borrow().iter().copied());
                for folded in ctx.folded_imports.borrow().iter() {
                    if !facts.imported_constants.contains(folded) {
                        facts.imported_constants.push(folded.clone());
                    }
                }
                // An action wrapper's own inner call: reported at its callers. Other forwarding
                // calls are found by what they forward (`forwarder_of`).
                let in_wrapper = enclosing_function_name(node, script)
                    .is_some_and(|f| wrappers.iter().any(|w| w.action.is_some() && w.name == f));
                if !in_wrapper {
                    match ctx.forwarder_of(node) {
                        None | Some((Callers::Never, _)) => push_call(&mut facts.calls, call),
                        Some((Callers::Always, _)) => {}
                        Some((Callers::IfCalled, function)) => deferred.push((call, function)),
                    }
                }
            }
        }
        if calls && matches!(node.kind(), "string" | "template_string") {
            strings.push(node);
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
    for (call, function) in deferred {
        if !ctx.called.borrow().contains(&function) {
            push_call(&mut facts.calls, call);
        }
    }
    for node in strings {
        let inside_call = in_call
            .iter()
            .any(|(s, e)| *s <= node.start_byte() && node.end_byte() <= *e);
        if inside_call || used.contains(&node.start_byte()) {
            continue;
        }
        if let Some(url) = ctx.literal_url(node) {
            facts.literals.push(LiteralUrl {
                line: line(node),
                url,
            });
        }
    }
    facts.calls.sort_by_key(|c| c.line);
    facts.literals.sort_by_key(|l| l.line);
    facts.global_reads.dedup();
    facts.wrappers = ctx.exported_wrappers(&wrappers, &instances);
    facts.clients = ctx.exported_clients();
    facts.constants = ctx.exported_constants();
    facts
}

/// One call per candidate address.
fn push_call(calls: &mut Vec<HttpCall>, mut call: HttpCall) {
    let candidates = std::mem::take(&mut call.candidates);
    if candidates.is_empty() {
        calls.push(call);
        return;
    }
    for url in candidates {
        calls.push(HttpCall {
            url: Some(url),
            url_expr: None,
            ..call.clone()
        });
    }
}

/// Addresses one call expands to at most; past that the map stays a `{…}` hole.
const MAX_CANDIDATES: usize = 64;

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
    /// Wrapper functions of the script by start byte.
    local_wrappers: RefCell<HashMap<usize, Wrapper>>,
    /// Functions and classes declared anywhere in the script, collected on first use.
    declared_names: RefCell<Option<HashSet<String>>>,
    instances: RefCell<HashSet<String>>,
    /// What a function forwards, by its start byte (`None` while it is being worked out).
    forwards: RefCell<HashMap<usize, Option<Forward>>>,
    /// The client a function returns, by its start byte.
    factories: RefCell<HashMap<usize, Option<Client>>>,
    /// The forwarding methods of an object literal, by its start byte.
    objects: RefCell<HashMap<usize, Option<Methods>>>,
    /// Start bytes of the strings `parts` has folded.
    folded: RefCell<Vec<usize>>,
    /// The imported constants it has folded: (local name, every value of an object).
    folded_imports: RefCell<Vec<(String, bool)>>,
    /// What each block declares, by node id.
    scopes: RefCell<HashMap<usize, HashMap<&'a str, Node<'a>>>>,
    /// The client a bound value holds, by the value's start byte.
    receivers: RefCell<HashMap<usize, Option<Client>>>,
    /// Forwarding methods called as `this.name()`, by start byte.
    called: RefCell<HashSet<usize>>,
}

enum Part {
    Lit(String),
    Hole(String),
    /// A map indexed by a variable (the expression as written): each of its string values.
    Alt(String, Vec<String>),
}

fn folds(parts: &[Part]) -> bool {
    parts.iter().all(|p| matches!(p, Part::Lit(_) | Part::Alt(..)))
}

/// What a name refers to in its scope.
enum Bound<'a> {
    /// `const x = value`, `let`, `var`.
    Value(Node<'a>),
    /// A parameter, with its default value.
    Param(Option<Node<'a>>),
    /// A function declaration or a variable holding a function.
    Function(Node<'a>),
    /// Declared without a value.
    Opaque,
}

/// A slot of a forwarded call as given at a call site.
enum At<'a> {
    Absent,
    Lit(String),
    Node(Node<'a>),
    /// Not visible at the call: the expression as far as it is written (`config.url`).
    Opaque(Option<String>),
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
                    if name.kind() == "identifier" && self.creates_instance(value, 0) {
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

    /// `axios.create(…)`, or one passed through a function that returns it: `wrap(axios.create())`
    /// attaching interceptors. A local function is checked; an imported one is taken to return
    /// what it is given, as interceptor setups do.
    fn creates_instance(&self, value: Node<'a>, depth: usize) -> bool {
        if depth > 3 {
            return false;
        }
        match value.kind() {
            "parenthesized_expression" | "await_expression" | "as_expression" | "satisfies_expression" | "non_null_expression" => {
                named(value).into_iter().next().is_some_and(|v| self.creates_instance(v, depth + 1))
            }
            "call_expression" => {
                if self.is_axios_create(value) {
                    return true;
                }
                let Some(index) = Self::args(value).into_iter().position(|a| self.creates_instance(a, depth + 1)) else {
                    return false;
                };
                let Some(function) = value.child_by_field_name("function").filter(|f| f.kind() == "identifier") else {
                    return false;
                };
                match self.resolve(self.text(function), function) {
                    Some(Bound::Function(f)) => self.returns_param(f, index),
                    Some(_) => false,
                    None => true,
                }
            }
            _ => false,
        }
    }

    fn returns_param(&self, function: Node<'a>, index: usize) -> bool {
        let Some(name) = self.param_slots(function).into_iter().find_map(|(n, s)| (s == Slot::Arg(index, Vec::new())).then_some(n)) else {
            return false;
        };
        let Some(body) = function.child_by_field_name("body") else {
            return false;
        };
        let returned = if body.kind() == "statement_block" { returned_values(body) } else { vec![body] };
        returned.iter().any(|r| r.kind() == "identifier" && self.text(*r) == name)
    }

    /// What `name` refers to at `at`: the nearest declaration, parameter or function declaration
    /// of the enclosing scopes; `None` for a global or an import.
    fn resolve(&self, name: &str, at: Node<'a>) -> Option<Bound<'a>> {
        for scope in self.ancestors(at) {
            if is_function(scope) || matches!(scope.kind(), "function_declaration" | "method_definition" | "generator_function_declaration") {
                if let Some(default) = param_binding(scope, name, self.src) {
                    return Some(Bound::Param(default));
                }
            }
            if matches!(scope.kind(), "statement_block" | "program") {
                if let Some(declared) = self.declared(scope, name) {
                    return Some(match declared.kind() {
                        "variable_declarator" => match declared.child_by_field_name("value") {
                            Some(v) if is_function(v) => Bound::Function(v),
                            Some(v) => Bound::Value(v),
                            None => Bound::Opaque,
                        },
                        _ => Bound::Function(declared),
                    });
                }
            }
        }
        None
    }

    /// The parameters of a function as slots: `(url, { params, signal } = {})` → `url` is
    /// argument 0, `params` is `params` of argument 1.
    fn param_slots(&self, function: Node<'a>) -> Vec<(String, Slot)> {
        let mut out = Vec::new();
        if let Some(single) = function.child_by_field_name("parameter") {
            out.push((self.text(single).to_string(), Slot::Arg(0, Vec::new())));
            return out;
        }
        let Some(params) = function.child_by_field_name("parameters") else {
            return out;
        };
        for (i, param) in named(params).into_iter().enumerate() {
            let pattern = param.child_by_field_name("pattern").unwrap_or(param);
            self.pattern_slots(pattern, Slot::Arg(i, Vec::new()), &mut out);
        }
        out
    }

    fn pattern_slots(&self, pattern: Node<'a>, slot: Slot, out: &mut Vec<(String, Slot)>) {
        match pattern.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => out.push((self.text(pattern).to_string(), slot)),
            "assignment_pattern" => {
                if let Some(left) = pattern.child_by_field_name("left") {
                    self.pattern_slots(left, slot, out);
                }
            }
            "rest_pattern" => {
                if let Some(inner) = named(pattern).into_iter().next() {
                    self.pattern_slots(inner, slot, out);
                }
            }
            "object_pattern" => {
                for member in named(pattern) {
                    match member.kind() {
                        "shorthand_property_identifier_pattern" => {
                            let key = self.text(member);
                            out.push((key.to_string(), slot.get(key)));
                        }
                        "object_assignment_pattern" => {
                            if let Some(left) = member.child_by_field_name("left") {
                                self.pattern_slots(left, slot.get(self.text(left)), out);
                            }
                        }
                        "pair_pattern" => {
                            if let (Some(key), Some(value)) = (member.child_by_field_name("key"), member.child_by_field_name("value")) {
                                self.pattern_slots(value, slot.get(&self.key_text(key)), out);
                            }
                        }
                        "rest_pattern" => self.pattern_slots(member, slot.clone(), out),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// The default value of parameter `index` (or of its key `path[0]`, destructured).
    fn param_default(&self, function: Node<'a>, index: usize, path: &[String]) -> Option<Node<'a>> {
        let param = named(function.child_by_field_name("parameters")?).into_iter().nth(index)?;
        let [key] = path else {
            return path.is_empty().then(|| param.child_by_field_name("value")).flatten();
        };
        let pattern = param.child_by_field_name("pattern").filter(|p| p.kind() == "object_pattern")?;
        named(pattern).into_iter().find_map(|member| match member.kind() {
            "object_assignment_pattern" => member
                .child_by_field_name("left")
                .is_some_and(|l| self.text(l) == key)
                .then(|| member.child_by_field_name("right"))
                .flatten(),
            "pair_pattern" => {
                let value = member.child_by_field_name("value").filter(|v| v.kind() == "assignment_pattern")?;
                member
                    .child_by_field_name("key")
                    .is_some_and(|k| self.key_text(k) == *key)
                    .then(|| value.child_by_field_name("right"))
                    .flatten()
            }
            _ => None,
        })
    }

    fn key_text(&self, key: Node) -> String {
        match key.kind() {
            "string" => self.string_value(key),
            _ => self.text(key).to_string(),
        }
    }

    /// The value expression a name is bound to by `const`/`let`/`var` visible at `at`.
    fn binding(&self, name: &str, at: Node<'a>) -> Option<Node<'a>> {
        for scope in self.ancestors(at) {
            if matches!(scope.kind(), "statement_block" | "program") {
                if let Some(declared) = self.declared(scope, name).filter(|d| d.kind() == "variable_declarator") {
                    return declared.child_by_field_name("value");
                }
            }
        }
        None
    }

    /// The ancestors of `node`, innermost first, in one descent from the root: `Node::parent`
    /// descends from the root at every level, and name lookups climb many levels per call.
    fn ancestors(&self, node: Node<'a>) -> Vec<Node<'a>> {
        let mut out = Vec::new();
        let mut current = self.root;
        while current != node {
            out.push(current);
            match current.child_with_descendant(node) {
                Some(child) => current = child,
                None => break,
            }
        }
        out.reverse();
        out
    }

    /// The declarator or function declaration of `name` in a block, the first of a name. Each
    /// block's names are collected once: lookups walk up many blocks for every call.
    fn declared(&self, scope: Node<'a>, name: &str) -> Option<Node<'a>> {
        if let Some(known) = self.scopes.borrow().get(&scope.id()) {
            return known.get(name).copied();
        }
        let mut declared: HashMap<&'a str, Node<'a>> = HashMap::new();
        for statement in named(scope) {
            let declaration = match statement.kind() {
                "export_statement" => match statement.child_by_field_name("declaration") {
                    Some(d) => d,
                    None => continue,
                },
                _ => statement,
            };
            match declaration.kind() {
                "function_declaration" | "generator_function_declaration" => {
                    if let Some(name) = declaration.child_by_field_name("name") {
                        declared.entry(self.text(name)).or_insert(declaration);
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    for declarator in named(declaration) {
                        if let Some(name) = declarator
                            .child_by_field_name("name")
                            .filter(|n| declarator.kind() == "variable_declarator" && n.kind() == "identifier")
                        {
                            declared.entry(self.text(name)).or_insert(declarator);
                        }
                    }
                }
                _ => {}
            }
        }
        let found = declared.get(name).copied();
        self.scopes.borrow_mut().insert(scope.id(), declared);
        found
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
        self.parts_in(node, depth, &[])
    }

    /// An imported constant, noted as folded.
    fn imported_constant(&self, name: &str) -> Option<String> {
        let value = self.imports.constant(name)?;
        self.folded_imports.borrow_mut().push((name.to_string(), false));
        Some(value)
    }

    fn imported_constants_of(&self, name: &str) -> Vec<String> {
        let values = self.imports.constants_of(name);
        if !values.is_empty() {
            self.folded_imports.borrow_mut().push((name.to_string(), true));
        }
        values
    }

    /// `env`: the parameters of a local helper being folded, bound to the arguments of its call.
    fn parts_in(&self, node: Node<'a>, depth: usize, env: &[(String, Node<'a>)]) -> Vec<Part> {
        if depth > 6 {
            return vec![Part::Hole(self.text(node).to_string())];
        }
        let hole = || vec![Part::Hole(self.text(node).to_string())];
        match node.kind() {
            "string" => {
                self.folded.borrow_mut().push(node.start_byte());
                vec![Part::Lit(self.string_value(node))]
            }
            "template_string" => {
                self.folded.borrow_mut().push(node.start_byte());
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
                            let inner = self.parts_in(expr, depth + 1, env);
                            if folds(&inner) {
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
                        let mut out = self.parts_in(l, depth + 1, env);
                        out.extend(self.parts_in(r, depth + 1, env));
                        out
                    }
                    _ => hole(),
                }
            }
            "parenthesized_expression" => match named(node).into_iter().next() {
                Some(inner) => self.parts_in(inner, depth + 1, env),
                None => Vec::new(),
            },
            "identifier" | "shorthand_property_identifier" => {
                let name = self.text(node);
                if let Some((_, arg)) = env.iter().find(|(n, _)| *n == name) {
                    return self.parts(*arg, depth + 1);
                }
                let value = match self.resolve(name, node) {
                    Some(Bound::Value(v)) => Some(v),
                    // The address unless the caller passes another; an empty default only
                    // marks an optional part, which stays a hole.
                    Some(Bound::Param(Some(d))) if self.string_of(d).is_none_or(|s| !s.is_empty()) => Some(d),
                    _ => None,
                };
                if let Some(value) = value {
                    let inner = self.parts(value, depth + 1);
                    if folds(&inner) {
                        return inner;
                    }
                }
                match self.imported_constant(name) {
                    Some(value) => vec![Part::Lit(value)],
                    None => vec![Part::Hole(name.to_string())],
                }
            }
            // `ENDPOINTS.list` of an object constant, local or imported.
            "member_expression" => {
                let (Some(object), Some(property)) =
                    (node.child_by_field_name("object"), node.child_by_field_name("property"))
                else {
                    return hole();
                };
                if object.kind() != "identifier" || property.kind() != "property_identifier" {
                    return hole();
                }
                let key = self.text(property);
                match self.object_value(object) {
                    Some(map) => {
                        if let Some(value) = self.property(map, key) {
                            let inner = self.parts(value, depth + 1);
                            if folds(&inner) {
                                return inner;
                            }
                        }
                        hole()
                    }
                    None => match self.imported_constant(&format!("{}.{key}", self.text(object))) {
                        Some(value) => vec![Part::Lit(value)],
                        None => hole(),
                    },
                }
            }
            // `endpoints[key]`: the value of a known key, or each value of the map.
            "subscript_expression" => {
                let (Some(object), Some(index)) =
                    (node.child_by_field_name("object"), node.child_by_field_name("index"))
                else {
                    return hole();
                };
                if object.kind() != "identifier" {
                    return hole();
                }
                let key = self.string_of(index);
                let values = match (self.object_value(object), key) {
                    (Some(map), Some(key)) => {
                        let inner = self.property(map, &key).map(|v| self.parts(v, depth + 1));
                        return inner.filter(|p| folds(p)).unwrap_or_else(hole);
                    }
                    (Some(map), None) => named(map)
                        .into_iter()
                        .filter(|m| m.kind() == "pair")
                        .filter_map(|m| m.child_by_field_name("value"))
                        .filter_map(|v| self.string_of(v))
                        .collect(),
                    (None, Some(key)) => {
                        return match self.imported_constant(&format!("{}.{key}", self.text(object))) {
                            Some(value) => vec![Part::Lit(value)],
                            None => hole(),
                        }
                    }
                    (None, None) => self.imported_constants_of(self.text(object)),
                };
                if values.is_empty() {
                    hole()
                } else {
                    vec![Part::Alt(self.text(node).to_string(), values)]
                }
            }
            "call_expression" => self.helper_parts(node, depth).unwrap_or_else(hole),
            _ => hole(),
        }
    }

    /// `itemUrl(id)` of a local helper returning an address: `(id) => \`/api/items/${id}/\``;
    /// `this.apiUrl(path)` of a method of the same object.
    fn helper_parts(&self, call: Node<'a>, depth: usize) -> Option<Vec<Part>> {
        let function = call.child_by_field_name("function")?;
        let helper = match function.kind() {
            "identifier" => match self.resolve(self.text(function), function)? {
                Bound::Function(f) => f,
                _ => return None,
            },
            "member_expression" if function.child_by_field_name("object").is_some_and(|o| o.kind() == "this") => {
                self.sibling(function, self.text(function.child_by_field_name("property")?))?
            }
            _ => return None,
        };
        let body = helper.child_by_field_name("body")?;
        let returned = if body.kind() == "statement_block" {
            match named(body).as_slice() {
                [only] if only.kind() == "return_statement" => named(*only).into_iter().next()?,
                _ => return None,
            }
        } else {
            body
        };
        let args = Self::args(call);
        let env: Vec<(String, Node<'a>)> = self
            .param_slots(helper)
            .into_iter()
            .filter_map(|(name, slot)| match slot {
                Slot::Arg(i, path) if path.is_empty() => Some((name, *args.get(i)?)),
                _ => None,
            })
            .collect();
        let parts = self.parts_in(returned, depth + 1, &env);
        parts
            .iter()
            .any(|p| matches!(p, Part::Lit(s) if !s.is_empty()) || matches!(p, Part::Alt(..)))
            .then_some(parts)
    }

    /// The object literal a name is bound to (`Object.freeze({…})` too).
    fn object_value(&self, name: Node<'a>) -> Option<Node<'a>> {
        match self.resolve(self.text(name), name)? {
            Bound::Value(v) => self.object_literal(v),
            _ => None,
        }
    }

    fn string_value(&self, node: Node) -> String {
        let raw = self.text(node);
        let inner = raw
            .get(1..raw.len().saturating_sub(1))
            .unwrap_or_default();
        inner.replace("\\/", "/").replace("\\'", "'").replace("\\\"", "\"")
    }

    /// (address, nothing folded → the expression as written, the addresses for each value of
    /// a map indexed by a variable).
    fn addresses(&self, node: Node<'a>) -> (Option<String>, Option<String>, Vec<String>) {
        let parts = self.parts(node, 0);
        if !parts
            .iter()
            .any(|p| matches!(p, Part::Lit(s) if !s.is_empty()) || matches!(p, Part::Alt(..)))
        {
            return (None, Some(self.with_placeholders(self.text(node))), Vec::new());
        }
        let mut shown = String::new();
        let mut variants = vec![String::new()];
        for part in &parts {
            match part {
                Part::Lit(s) => {
                    shown.push_str(s);
                    variants.iter_mut().for_each(|v| v.push_str(s));
                }
                Part::Alt(expr, values) if variants.len() * values.len() <= MAX_CANDIDATES => {
                    shown.push_str(&format!("{{{expr}}}"));
                    variants = variants
                        .iter()
                        .flat_map(|v| values.iter().map(move |value| format!("{v}{value}")))
                        .collect();
                }
                Part::Hole(expr) | Part::Alt(expr, _) => {
                    let hole = format!("{{{expr}}}");
                    shown.push_str(&hole);
                    variants.iter_mut().for_each(|v| v.push_str(&hole));
                }
            }
        }
        match variants.as_slice() {
            [only] => (Some(self.with_placeholders(only)), None, Vec::new()),
            _ => (
                Some(self.with_placeholders(&shown)),
                None,
                variants.iter().map(|v| self.with_placeholders(v)).collect(),
            ),
        }
    }

    /// (address, nothing folded → the expression as written).
    fn url(&self, node: Node<'a>) -> (Option<String>, Option<String>) {
        let (url, expr, _) = self.addresses(node);
        (url, expr)
    }

    fn set_url(&self, node: Node<'a>, out: &mut HttpCall) {
        let (url, expr, candidates) = self.addresses(node);
        out.url = url;
        out.url_expr = expr;
        out.candidates = candidates;
    }

    fn string_of(&self, node: Node<'a>) -> Option<String> {
        let parts = self.parts(node, 0);
        let mut out = String::new();
        for p in parts {
            match p {
                Part::Lit(s) => out.push_str(&s),
                Part::Hole(_) | Part::Alt(..) => return None,
            }
        }
        Some(self.with_placeholders(&out))
    }

    /// The value of `key` in an object literal (or a name bound to one).
    fn property(&self, object: Node<'a>, key: &str) -> Option<Node<'a>> {
        self.property_at(object, key, 0)
    }

    fn property_at(&self, object: Node<'a>, key: &str, depth: usize) -> Option<Node<'a>> {
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
        // `{ ...base, url }`: a key of a known object spread into it.
        if depth > 4 {
            return None;
        }
        named(object)
            .into_iter()
            .rev()
            .filter(|m| m.kind() == "spread_element")
            .filter_map(|m| named(m).into_iter().next())
            .find_map(|inner| self.property_at(inner, key, depth + 1))
    }

    fn object_literal(&self, node: Node<'a>) -> Option<Node<'a>> {
        match node.kind() {
            "object" => Some(node),
            "identifier" => self
                .binding(self.text(node), node)
                .and_then(|v| self.object_literal(v).filter(|_| v.kind() != "identifier")),
            "parenthesized_expression" => named(node).into_iter().next().and_then(|n| self.object_literal(n)),
            "call_expression"
                if node
                    .child_by_field_name("function")
                    .is_some_and(|f| self.text(f).replace(char::is_whitespace, "") == "Object.freeze") =>
            {
                Self::args(node).into_iter().next().filter(|a| a.kind() == "object")
            }
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
            "identifier" | "shorthand_property_identifier" => {
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
                self.set_url(*args.first()?, &mut out);
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
                self.set_url(self.property(config, "url")?, &mut out);
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
                self.set_url(*args.first()?, &mut out);
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
                let Some(wrapper) = self.wrapper_for(function, member.as_ref()) else {
                    return self.client_call(call, function, member.as_ref(), &args, out);
                };
                let returns_body = wrapper.forward.returns_body;
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
                    out.params = self.slot_keys(&wrapper.forward.body, &args);
                    self.reads(call, &mut out, if returns_body { "" } else { "data" });
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
                    self.apply(&wrapper.forward, &args, &mut out)?;
                }
                self.parse_ajax_url(&mut out);
                self.reads(call, &mut out, if returns_body { "" } else { "data" });
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

    /// `http.get(url)` where `http` holds a client object: a factory's result bound to a
    /// constant, a parameter's default value (destructured too), `injected || createClient()`,
    /// an import. A parameter without a default tells nothing: no call.
    fn client_call(
        &self,
        call: Node<'a>,
        function: Node<'a>,
        member: Option<&(String, String)>,
        args: &[Node<'a>],
        mut out: HttpCall,
    ) -> Option<HttpCall> {
        let (_, method) = member?;
        let object = function.child_by_field_name("object")?;
        // `this.request(url)`: a forwarding method of the same object.
        let (via, forward) = if object.kind() == "this" {
            let sibling = self.sibling(function, method)?;
            let forward = self.forward_of(sibling, 0)?;
            self.called.borrow_mut().insert(sibling.start_byte());
            (format!("this.{method}"), forward)
        } else {
            let client = self.client_of(object, 0)?;
            (format!("{}.{method}", client.origin), client.method(method)?)
        };
        out.client = "wrapper";
        out.via = Some(via);
        self.apply(&forward, args, &mut out)?;
        self.parse_ajax_url(&mut out);
        self.reads(call, &mut out, if forward.returns_body { "" } else { "data" });
        Some(out)
    }

    /// The call a forwarding function makes, given the arguments of its call.
    fn apply(&self, forward: &Forward, args: &[Node<'a>], out: &mut HttpCall) -> Option<()> {
        match self.at(&forward.url, args) {
            At::Node(node) => self.set_url(node, out),
            At::Lit(url) => out.url = Some(self.with_placeholders(&url)),
            At::Opaque(Some(expr)) => out.url_expr = Some(self.with_placeholders(&expr)),
            At::Absent if forward.url_default.is_some() => out.url = forward.url_default.clone(),
            At::Opaque(None) | At::Absent => return None,
        }
        out.method = match self.at(&forward.method, args) {
            At::Lit(m) => Some(m.to_ascii_uppercase()),
            At::Node(node) => self.string_of(node).map(|m| m.to_ascii_uppercase()),
            At::Absent => Some("GET".into()),
            At::Opaque(_) => None,
        };
        out.params = self.slot_keys(&forward.body, args);
        out.params.extend(self.slot_keys(&forward.query, args));
        Some(())
    }

    fn at(&self, slot: &Slot, args: &[Node<'a>]) -> At<'a> {
        match slot {
            Slot::Absent => At::Absent,
            Slot::Lit(s) => At::Lit(s.clone()),
            Slot::Unknown | Slot::Object(..) => At::Opaque(None),
            Slot::Or(first, second) => match self.at(first, args) {
                At::Absent => self.at(second, args),
                found => found,
            },
            Slot::Arg(i, path) => {
                let Some(mut node) = args.get(*i).copied() else {
                    return At::Absent;
                };
                for (k, key) in path.iter().enumerate() {
                    let Some(object) = self.object_literal(node) else {
                        return At::Opaque(Some(format!("{}.{}", self.text(node), path[k..].join("."))));
                    };
                    match self.property(object, key) {
                        Some(value) => node = value,
                        // A spread whose keys are not known may carry it.
                        None if named(object).iter().any(|m| {
                            m.kind() == "spread_element"
                                && named(*m).into_iter().next().is_none_or(|inner| self.object_literal(inner).is_none())
                        }) =>
                        {
                            return At::Opaque(Some(format!("{}.{}", self.text(object), path[k..].join("."))))
                        }
                        None => return At::Absent,
                    }
                }
                At::Node(node)
            }
        }
    }

    /// Keys of the data a slot sends at a call.
    fn slot_keys(&self, slot: &Slot, args: &[Node<'a>]) -> Vec<String> {
        match slot {
            Slot::Object(fields, spreads) => {
                let mut out: Vec<String> = fields.iter().map(|(k, _)| k.clone()).collect();
                for spread in spreads {
                    out.extend(self.slot_keys(spread, args));
                }
                out
            }
            _ => match self.at(slot, args) {
                At::Node(node) => self.keys(node),
                _ => Vec::new(),
            },
        }
    }

    fn wrapper_for(&self, function: Node<'a>, member: Option<&(String, String)>) -> Option<Wrapper> {
        if let Some((object, method)) = member {
            return self.imports.wrapper(object, Some(method));
        }
        let name = self.text(function);
        // The wrapper in scope: two scripts' IIFEs may each have a `send` of their own.
        if !self.local_wrappers.borrow().values().any(|w| w.name == name) {
            return self.imports.wrapper(name, None);
        }
        match self.resolve(name, function) {
            Some(Bound::Function(f)) => self.local_wrappers.borrow().get(&f.start_byte()).cloned(),
            Some(_) => None,
            None => self.imports.wrapper(name, None),
        }
    }

    fn axios_config(&self, args: &[Node<'a>], out: &mut HttpCall) -> Option<()> {
        let first = *args.first()?;
        let config = if self.object_literal(first).is_some() {
            first
        } else {
            self.set_url(first, out);
            *args.get(1)?
        };
        if out.url.is_none() && out.url_expr.is_none() {
            self.set_url(self.property(config, "url")?, out);
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
        self.set_url(*args.first()?, out);
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
                self.set_url(first, out);
                *args.get(1)?
            };
            if out.url.is_none() && out.url_expr.is_none() {
                self.set_url(self.property(settings, "url")?, out);
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
        self.set_url(first, out);
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
    /// fetch; `cb` inline or a local function), `await` into a variable or a destructuring, a
    /// variable `.then`-ed later, and the success callback of jQuery and `BX.ajax`.
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
            // `.then(data)` with `const data = (response) => response.data` of this script too.
            if let Some(cb) = Self::args(then_call).into_iter().next().and_then(|c| self.function_value(c)) {
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
                forward: Forward::default(),
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
                if let Some(w) = self.wrapper_of(&name, function) {
                    self.local_wrappers.borrow_mut().insert(function.start_byte(), w.clone());
                    out.push(w);
                }
            }
            push_children(&mut stack, node);
        }
        out
    }

    fn wrapper_of(&self, name: &str, function: Node<'a>) -> Option<Wrapper> {
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
        let body = function.child_by_field_name("body")?;
        if let Some(w) = self.action_wrapper(name, body, &params) {
            return Some(w);
        }
        Some(Wrapper {
            name: name.to_string(),
            forward: self.forward_of(function, 0)?,
            instance: false,
            action: None,
        })
    }

    /// The backend call a function makes from its parameters, when it passes one on as the
    /// address: directly to a client, or through another such function or client object.
    fn forward_of(&self, function: Node<'a>, depth: usize) -> Option<Forward> {
        if depth > 8 {
            return None;
        }
        let key = function.start_byte();
        if let Some(known) = self.forwards.borrow().get(&key) {
            return known.clone();
        }
        self.forwards.borrow_mut().insert(key, None);
        let found = self.find_forward(function, depth);
        self.forwards.borrow_mut().insert(key, found.clone());
        found
    }

    fn find_forward(&self, function: Node<'a>, depth: usize) -> Option<Forward> {
        let env = self.param_slots(function);
        if env.is_empty() {
            return None;
        }
        let body = function.child_by_field_name("body")?;
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            if node != body && !runs_with_caller(node) {
                continue;
            }
            if node.kind() == "call_expression" {
                if let Some(forward) = self.call_forward(node, &env, depth).filter(Forward::forwards) {
                    let own_default = match &forward.url {
                        Slot::Arg(i, path) => self
                            .param_default(function, *i, path)
                            .and_then(|d| self.string_of(d))
                            .filter(|d| !d.is_empty()),
                        _ => None,
                    };
                    return Some(Forward {
                        returns_body: forward.returns_body || returns_data(body, self.src),
                        url_default: own_default.or(forward.url_default.clone()),
                        ..forward
                    });
                }
            }
            push_children(&mut stack, node);
        }
        None
    }

    /// The backend call `call` makes, in terms of the parameters `env` of the function it is in.
    fn call_forward(&self, call: Node<'a>, env: &[(String, Slot)], depth: usize) -> Option<Forward> {
        let arguments = call.child_by_field_name("arguments")?;
        // Most calls in a function do not touch its parameters; skip them before tracing names.
        let text = self.text(arguments);
        if !env.iter().any(|(name, _)| text.contains(name.as_str())) {
            return None;
        }
        let args: Vec<Slot> = named(arguments).into_iter().map(|a| self.abs(a, env, 0)).collect();
        if !args.iter().any(Slot::depends_on_args) {
            return None;
        }
        let function = call.child_by_field_name("function")?;
        let callee = self.text(function).replace(char::is_whitespace, "");
        let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
        match callee.as_str() {
            "fetch" | "window.fetch" => {
                return Some(Forward {
                    method: arg(1).get("method"),
                    url: arg(0),
                    body: arg(1).get("body"),
                    query: Slot::Absent,
                    returns_body: false,
                    url_default: None,
                })
            }
            // `axios(config)` or `axios(url, config)`.
            "axios" => {
                return Some(if args.len() > 1 || matches!(arg(0), Slot::Lit(_)) {
                    Forward {
                        url: arg(0),
                        ..config_forward(arg(1))
                    }
                } else {
                    config_forward(arg(0))
                })
            }
            _ => {}
        }
        if let Some((object, method)) = self.member_parts(function) {
            if object == "axios" || self.instances.borrow().contains(&object) {
                return axios_forward(&method, &args);
            }
            if matches!(object.as_str(), "$" | "jQuery") {
                return jquery_forward(&method, &args);
            }
        }
        Some(self.callee_forward(function, depth)?.subst(&args))
    }

    /// What a called function or client method forwards, in terms of its own arguments.
    fn callee_forward(&self, function: Node<'a>, depth: usize) -> Option<Forward> {
        match function.kind() {
            "identifier" => {
                let name = self.text(function);
                match self.resolve(name, function) {
                    Some(Bound::Function(f)) => self.forward_of(f, depth + 1),
                    Some(_) => None,
                    None => self
                        .imports
                        .wrapper(name, None)
                        .filter(|w| !w.instance && w.action.is_none())
                        .map(|w| w.forward),
                }
            }
            "member_expression" => {
                let object = function.child_by_field_name("object")?;
                let method = self.text(function.child_by_field_name("property")?);
                if object.kind() == "this" {
                    return self.forward_of(self.sibling(function, method)?, depth + 1);
                }
                if let Some(client) = self.client_of(object, depth + 1) {
                    return client.method(method);
                }
                if object.kind() != "identifier" {
                    return None;
                }
                let wrapper = self.imports.wrapper(self.text(object), Some(method))?;
                if wrapper.instance {
                    let own: Vec<Slot> = (0..3).map(|i| Slot::Arg(i, Vec::new())).collect();
                    return axios_forward(method, &own);
                }
                wrapper.action.is_none().then_some(wrapper.forward)
            }
            _ => None,
        }
    }

    /// The method `name` of the object or class whose method contains `at` (`this.name`).
    fn sibling(&self, at: Node<'a>, name: &str) -> Option<Node<'a>> {
        let ancestors = self.ancestors(at);
        // Arrow functions take `this` from outside.
        let i = ancestors.iter().position(|n| {
            matches!(n.kind(), "method_definition" | "function_expression" | "function" | "function_declaration")
        })?;
        let container = match ancestors.get(i + 1)? {
            p if p.kind() == "pair" => *ancestors.get(i + 2)?,
            p => *p,
        };
        match container.kind() {
            "object" => self.object_method(container, name).or_else(|| self.component_method(container, name)),
            "class_body" => self.object_method(container, name),
            _ => None,
        }
    }

    /// A method of a component's options reached through `this`: in a group next to the one
    /// holding the caller (`methods: { … }` beside `mounted()`, or beside `computed`), as
    /// Vue's options API binds them all to one instance.
    fn component_method(&self, object: Node<'a>, name: &str) -> Option<Node<'a>> {
        let options = object
            .parent()
            .filter(|p| p.kind() == "pair")
            .and_then(|p| p.parent())
            .filter(|o| o.kind() == "object")
            .unwrap_or(object);
        let groups = named(options)
            .into_iter()
            .filter(|m| m.kind() == "pair")
            .filter_map(|m| m.child_by_field_name("value"))
            .filter(|v| v.kind() == "object" && *v != object);
        self.object_method(options, name)
            .filter(|_| options != object)
            .or_else(|| groups.into_iter().find_map(|g| self.object_method(g, name)))
    }

    fn object_method(&self, object: Node<'a>, name: &str) -> Option<Node<'a>> {
        named(object).into_iter().find_map(|member| match member.kind() {
            "method_definition" => member
                .child_by_field_name("name")
                .is_some_and(|n| self.text(n) == name)
                .then_some(member),
            "pair" => {
                let key = member.child_by_field_name("key")?;
                if self.key_text(key) != name {
                    return None;
                }
                self.function_value(member.child_by_field_name("value")?)
            }
            "shorthand_property_identifier" if self.text(member) == name => self.function_value(member),
            _ => None,
        })
    }

    /// A function, or a name bound to one.
    fn function_value(&self, node: Node<'a>) -> Option<Node<'a>> {
        if is_function(node) {
            return Some(node);
        }
        match node.kind() {
            "identifier" | "shorthand_property_identifier" => match self.resolve(self.text(node), node)? {
                Bound::Function(f) => Some(f),
                _ => None,
            },
            _ => None,
        }
    }

    /// The value of an expression inside a forwarding function, in terms of its parameters.
    fn abs(&self, node: Node<'a>, env: &[(String, Slot)], depth: usize) -> Slot {
        if depth > 6 {
            return Slot::Unknown;
        }
        let first = || named(node).into_iter().next();
        match node.kind() {
            "identifier" | "shorthand_property_identifier" => {
                let name = self.text(node);
                if let Some((_, slot)) = env.iter().rev().find(|(n, _)| n == name) {
                    return slot.clone();
                }
                match self.binding(name, node) {
                    Some(value) => self.abs(value, env, depth + 1),
                    None => Slot::Unknown,
                }
            }
            "string" => Slot::Lit(self.string_value(node)),
            "template_string" if named(node).iter().all(|c| c.kind() == "string_fragment") => {
                Slot::Lit(self.text(node).trim_matches('`').to_string())
            }
            "member_expression" => {
                let (Some(object), Some(property)) =
                    (node.child_by_field_name("object"), node.child_by_field_name("property"))
                else {
                    return Slot::Unknown;
                };
                match self.abs(object, env, depth + 1) {
                    slot @ (Slot::Arg(..) | Slot::Object(..)) => slot.get(self.text(property)),
                    _ => Slot::Unknown,
                }
            }
            "object" => {
                let mut fields = Vec::new();
                let mut spreads = Vec::new();
                for member in named(node) {
                    match member.kind() {
                        "pair" => {
                            if let (Some(k), Some(v)) = (member.child_by_field_name("key"), member.child_by_field_name("value")) {
                                if k.kind() != "computed_property_name" {
                                    fields.push((self.key_text(k), self.abs(v, env, depth + 1)));
                                }
                            }
                        }
                        "shorthand_property_identifier" => {
                            fields.push((self.text(member).to_string(), self.abs(member, env, depth + 1)))
                        }
                        "spread_element" => {
                            if let Some(inner) = named(member).into_iter().next() {
                                spreads.push(self.abs(inner, env, depth + 1));
                            }
                        }
                        _ => {}
                    }
                }
                Slot::Object(fields, spreads)
            }
            "parenthesized_expression" | "await_expression" => match first() {
                Some(inner) => self.abs(inner, env, depth + 1),
                None => Slot::Unknown,
            },
            // `config || {}`, `url ?? DEFAULT`.
            "binary_expression"
                if node
                    .child_by_field_name("operator")
                    .is_some_and(|o| matches!(self.text(o), "||" | "??")) =>
            {
                let left = node
                    .child_by_field_name("left")
                    .map(|l| self.abs(l, env, depth + 1))
                    .unwrap_or_default();
                let right = node
                    .child_by_field_name("right")
                    .map(|r| self.abs(r, env, depth + 1))
                    .unwrap_or_default();
                match (left, right) {
                    (Slot::Unknown | Slot::Absent, right) => right,
                    (left, Slot::Unknown | Slot::Absent) => left,
                    (left, right) => Slot::Or(Box::new(left), Box::new(right)),
                }
            }
            "call_expression"
                if node
                    .child_by_field_name("function")
                    .is_some_and(|f| self.text(f).replace(char::is_whitespace, "") == "JSON.stringify") =>
            {
                match Self::args(node).into_iter().next() {
                    Some(inner) => self.abs(inner, env, depth + 1),
                    None => Slot::Unknown,
                }
            }
            _ => Slot::Unknown,
        }
    }

    /// The client object an expression holds. `depth` bounds chains of names and factories.
    fn client_of(&self, receiver: Node<'a>, depth: usize) -> Option<Client> {
        if depth > 8 {
            return None;
        }
        match receiver.kind() {
            "identifier" => {
                let name = self.text(receiver);
                let value = match self.resolve(name, receiver) {
                    Some(Bound::Value(value)) | Some(Bound::Param(Some(value))) => value,
                    Some(_) => return None,
                    None => return self.imports.client(name, None, false),
                };
                let key = value.start_byte();
                if let Some(known) = self.receivers.borrow().get(&key) {
                    return known.clone();
                }
                let found = self.shape_of(value, name, depth + 1);
                self.receivers.borrow_mut().insert(key, found.clone());
                found
            }
            // `api.http` of a namespace import.
            "member_expression" => {
                let object = receiver.child_by_field_name("object").filter(|o| o.kind() == "identifier")?;
                let property = receiver.child_by_field_name("property")?;
                let name = self.text(object);
                if self.resolve(name, object).is_some() {
                    return None;
                }
                self.imports.client(name, Some(self.text(property)), false)
            }
            _ => None,
        }
    }

    /// The client object a value is: a factory's result, an object literal of forwarding
    /// methods, the first that is one of `a || b`.
    fn shape_of(&self, value: Node<'a>, name: &str, depth: usize) -> Option<Client> {
        if depth > 8 {
            return None;
        }
        match value.kind() {
            "call_expression" => {
                let function = value.child_by_field_name("function")?;
                match function.kind() {
                    "identifier" => {
                        let factory = self.text(function);
                        match self.resolve(factory, function) {
                            Some(Bound::Function(f)) => self.factory_of(f, factory, depth + 1),
                            Some(_) => None,
                            None => self.imports.client(factory, None, true),
                        }
                    }
                    "member_expression" => {
                        let object = function.child_by_field_name("object").filter(|o| o.kind() == "identifier")?;
                        let property = function.child_by_field_name("property")?;
                        if self.resolve(self.text(object), object).is_some() {
                            return None;
                        }
                        self.imports.client(self.text(object), Some(self.text(property)), true)
                    }
                    // `(function () { return { get(url) {…} } })()`: a module object.
                    _ => {
                        let mut callee = function;
                        while callee.kind() == "parenthesized_expression" {
                            callee = named(callee).into_iter().next()?;
                        }
                        is_function(callee).then(|| self.factory_of(callee, name, depth + 1)).flatten()
                    }
                }
            }
            "binary_expression"
                if value
                    .child_by_field_name("operator")
                    .is_some_and(|o| matches!(self.text(o), "||" | "??")) =>
            {
                let left = value.child_by_field_name("left").and_then(|l| self.shape_of(l, name, depth + 1));
                left.or_else(|| value.child_by_field_name("right").and_then(|r| self.shape_of(r, name, depth + 1)))
            }
            "ternary_expression" => value
                .child_by_field_name("consequence")
                .and_then(|c| self.shape_of(c, name, depth + 1))
                .or_else(|| value.child_by_field_name("alternative").and_then(|a| self.shape_of(a, name, depth + 1))),
            "parenthesized_expression" | "await_expression" | "as_expression" | "satisfies_expression" | "non_null_expression" => {
                named(value).into_iter().next().and_then(|inner| self.shape_of(inner, name, depth + 1))
            }
            "identifier" => self.client_of(value, depth + 1),
            "object" => self.object_client(value, name, depth + 1),
            _ => None,
        }
    }

    /// The client object a function returns.
    fn factory_of(&self, function: Node<'a>, name: &str, depth: usize) -> Option<Client> {
        let key = function.start_byte();
        if let Some(known) = self.factories.borrow().get(&key) {
            return known.clone();
        }
        self.factories.borrow_mut().insert(key, None);
        let found = function.child_by_field_name("body").and_then(|body| {
            let returned = if body.kind() == "statement_block" { returned_values(body) } else { vec![body] };
            returned.into_iter().find_map(|r| self.shape_of(r, name, depth + 1))
        });
        let found = found.map(|client| Client {
            origin: name.to_string(),
            ..client
        });
        self.factories.borrow_mut().insert(key, found.clone());
        found
    }

    /// An object literal whose methods forward their arguments to a backend call.
    fn object_client(&self, object: Node<'a>, name: &str, depth: usize) -> Option<Client> {
        let key = object.start_byte();
        if let Some(known) = self.objects.borrow().get(&key) {
            return known.clone().map(|methods| Client {
                origin: name.to_string(),
                methods,
            });
        }
        self.objects.borrow_mut().insert(key, None);
        let mut methods = Vec::new();
        for member in named(object) {
            let (key, function) = match member.kind() {
                "method_definition" => (member.child_by_field_name("name").map(|n| self.text(n).to_string()), Some(member)),
                "pair" => (
                    member.child_by_field_name("key").map(|k| self.key_text(k)),
                    member.child_by_field_name("value").and_then(|v| self.function_value(v)),
                ),
                "shorthand_property_identifier" => (Some(self.text(member).to_string()), self.function_value(member)),
                _ => continue,
            };
            if let (Some(key), Some(function)) = (key, function) {
                if let Some(forward) = self.forward_of(function, depth + 1) {
                    methods.push((key, forward));
                }
            }
        }
        let found = (!methods.is_empty()).then_some(methods);
        self.objects.borrow_mut().insert(key, found.clone());
        found.map(|methods| Client {
            origin: name.to_string(),
            methods,
        })
    }

    /// The function around the call that passes one of its parameters on as the call's address,
    /// with where its own calls are found.
    fn forwarder_of(&self, call: Node<'a>) -> Option<(Callers, usize)> {
        let mut current = enclosing_function(call);
        for _ in 0..3 {
            let function = current?;
            let env = self.param_slots(function);
            if !env.is_empty() && self.call_forward(call, &env, 0).is_some_and(|f| f.forwards()) {
                return Some((callers(function), function.start_byte()));
            }
            if !runs_with_caller(function) {
                return None;
            }
            current = enclosing_function(function);
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
                    forward: Forward {
                        body: data_param.map(|i| Slot::Arg(i, Vec::new())).unwrap_or_default(),
                        returns_body: returns_data(body, self.src),
                        ..Forward::default()
                    },
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

    /// Wrappers reachable from other modules: exported functions, members of a default-exported
    /// object, exported instances.
    /// `export { local as name }` without `from`: (local, exported name). The local may be an
    /// import the module passes on, as an index does.
    fn export_list(&self) -> Vec<(Node<'a>, String)> {
        let mut out = Vec::new();
        for statement in named(self.root) {
            if statement.kind() != "export_statement" || statement.child_by_field_name("source").is_some() {
                continue;
            }
            for clause in named(statement).into_iter().filter(|c| c.kind() == "export_clause") {
                for specifier in named(clause) {
                    if let Some(local) = specifier.child_by_field_name("name") {
                        let alias = specifier.child_by_field_name("alias").unwrap_or(local);
                        out.push((local, self.text(alias).to_string()));
                    }
                }
            }
        }
        out
    }

    fn exported_wrappers(&self, wrappers: &[Wrapper], instances: &HashSet<String>) -> Vec<Wrapper> {
        let mut out = Vec::new();
        for (local, name) in self.export_list() {
            let local_name = self.text(local);
            let found = match self.resolve(local_name, local) {
                Some(_) => wrappers.iter().find(|w| w.name == local_name).cloned(),
                None => self.imports.wrapper(local_name, None),
            };
            out.extend(found.map(|w| Wrapper { name, ..w }));
        }
        for statement in named(self.root) {
            if statement.kind() != "export_statement" {
                continue;
            }
            let default = self.text(statement)["export".len()..].trim_start().starts_with("default");
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
                        // `export default function request(url)`: imported as the default.
                        let name = if default { "default".to_string() } else { name };
                        out.push(Wrapper { name, ..w.clone() });
                    }
                }
            }
            if let Some(value) = statement.child_by_field_name("value") {
                if value.kind() == "identifier" && instances.contains(self.text(value)) {
                    out.push(Wrapper {
                        name: "default".into(),
                        forward: Forward::default(),
                        instance: true,
                        action: None,
                    });
                }
                // `export default request` of a wrapper held by a variable.
                if value.kind() == "identifier" {
                    if let Some(w) = wrappers.iter().find(|w| !w.instance && w.name == self.text(value)) {
                        out.push(Wrapper {
                            name: "default".into(),
                            ..w.clone()
                        });
                    }
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
        for (local, name) in self.export_list() {
            let local_name = self.text(local);
            match self.resolve(local_name, local) {
                Some(Bound::Value(v)) => out.extend(self.constant_entries(&name, v)),
                Some(_) => {}
                None => out.extend(self.imports.constant(local_name).map(|v| (name, v))),
            }
        }
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
                    out.extend(self.constant_entries(self.text(n), v));
                }
            }
        }
        out
    }

    /// A string constant `name`, or `name.key` for each string value of an object constant.
    fn constant_entries(&self, name: &str, value: Node<'a>) -> Vec<(String, String)> {
        if let Some(s) = self.string_of(value) {
            return vec![(name.to_string(), s)];
        }
        let Some(object) = self.object_literal(value) else {
            return Vec::new();
        };
        named(object)
            .into_iter()
            .filter_map(|member| {
                let (key, value) = (member.child_by_field_name("key")?, member.child_by_field_name("value")?);
                Some((format!("{name}.{}", self.key_text(key)), self.string_of(value)?))
            })
            .collect()
    }

    /// Client objects and client factories reachable from other modules.
    fn exported_clients(&self) -> Vec<ExportedClient> {
        let mut out = Vec::new();
        for statement in named(self.root) {
            if statement.kind() != "export_statement" || statement.child_by_field_name("source").is_some() {
                continue;
            }
            let default = self.text(statement)["export".len()..].trim_start().starts_with("default");
            let exported = |name: &str| if default { "default".to_string() } else { name.to_string() };
            if let Some(declaration) = statement.child_by_field_name("declaration") {
                match declaration.kind() {
                    "function_declaration" => {
                        if let Some(name) = declaration.child_by_field_name("name").map(|n| self.text(n)) {
                            out.extend(self.export_client(exported(name), name, declaration));
                        }
                    }
                    "lexical_declaration" | "variable_declaration" => {
                        for declarator in named(declaration) {
                            if let (Some(n), Some(v)) = (declarator.child_by_field_name("name"), declarator.child_by_field_name("value")) {
                                out.extend(self.export_client(self.text(n).to_string(), self.text(n), v));
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let Some(value) = statement.child_by_field_name("value").filter(|_| default) {
                let local = if value.kind() == "identifier" { self.text(value) } else { "default" };
                let value = match value.kind() {
                    "identifier" => match self.resolve(local, value) {
                        Some(Bound::Function(f)) => f,
                        Some(Bound::Value(v)) => v,
                        _ => continue,
                    },
                    _ => value,
                };
                out.extend(self.export_client("default".into(), local, value));
            }
        }
        for (local, name) in self.export_list() {
            let local_name = self.text(local);
            let value = match self.resolve(local_name, local) {
                Some(Bound::Function(f)) => f,
                Some(Bound::Value(v)) => v,
                Some(_) => continue,
                None => {
                    for factory in [true, false] {
                        if let Some(client) = self.imports.client(local_name, None, factory) {
                            out.push(ExportedClient {
                                name: name.clone(),
                                factory,
                                client,
                            });
                        }
                    }
                    continue;
                }
            };
            out.extend(self.export_client(name, local_name, value));
        }
        out
    }

    fn export_client(&self, exported: String, local: &str, value: Node<'a>) -> Option<ExportedClient> {
        let factory = is_function(value) || value.kind() == "function_declaration";
        let client = if factory {
            self.factory_of(value, local, 0)
        } else {
            self.shape_of(value, local, 0)
        }?;
        Some(ExportedClient {
            name: exported,
            factory,
            client,
        })
    }
}

/// Not a function, or one passed as an argument (a callback: a `Promise` executor, `.then`),
/// which runs as part of the call of the function around it. A function it returns, or a
/// method of an object it returns, runs later: its calls are not the outer function's.
fn runs_with_caller(node: Node) -> bool {
    if !(is_function(node) || matches!(node.kind(), "function_declaration" | "method_definition" | "class_declaration")) {
        return true;
    }
    node.parent().is_some_and(|p| p.kind() == "arguments")
}

/// Where the calls of a forwarding function are found, so that its own call is reported there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Callers {
    /// By its name, or through the client object it is a method of: an object bound to a
    /// name, exported, or returned by a function that is.
    Always,
    /// As `this.name()`, a method of an object found nowhere else (component options, an
    /// object a callback or a class method returns): only when the script makes such a call.
    IfCalled,
    /// An anonymous callback, a constructor assigned to a property: the call stays where it is.
    Never,
}

fn callers(function: Node) -> Callers {
    let parent = function.parent();
    let object = match function.kind() {
        "function_declaration" => return Callers::Always,
        "method_definition" => parent.filter(|p| p.kind() == "object"),
        _ => match parent {
            Some(p) if p.kind() == "variable_declarator" => return Callers::Always,
            Some(p) if p.kind() == "pair" => p.parent().filter(|o| o.kind() == "object"),
            _ => None,
        },
    };
    match object {
        Some(object) if reachable_object(object) => Callers::Always,
        Some(_) => Callers::IfCalled,
        None => Callers::Never,
    }
}

/// Calls through the object's methods can be found: it is bound to a name, exported, or
/// returned by a function that is reachable itself.
fn reachable_object(object: Node) -> bool {
    let mut holder = object.parent();
    while let Some(h) = holder.filter(|h| h.kind() == "parenthesized_expression") {
        holder = h.parent();
    }
    let Some(holder) = holder else {
        return false;
    };
    match holder.kind() {
        "variable_declarator" | "export_statement" => true,
        "arrow_function" => reachable_function(holder),
        "return_statement" => enclosing_function(holder).is_some_and(reachable_function),
        _ => false,
    }
}

/// A function its callers can name: declared, held by a variable, exported, a method of a
/// reachable object, or called on the spot with the result bound to a name (an IIFE module).
fn reachable_function(function: Node) -> bool {
    let mut parent = function.parent();
    while let Some(p) = parent.filter(|p| p.kind() == "parenthesized_expression") {
        parent = p.parent();
    }
    if function.kind() == "function_declaration" {
        return true;
    }
    match parent {
        Some(p) if matches!(p.kind(), "variable_declarator" | "export_statement") => true,
        Some(p) if p.kind() == "pair" || function.kind() == "method_definition" => {
            let object = if function.kind() == "method_definition" { Some(p) } else { p.parent() };
            object.is_some_and(|o| o.kind() == "object" && reachable_object(o))
        }
        Some(p) if p.kind() == "call_expression" => {
            let mut holder = p.parent();
            while let Some(h) = holder.filter(|h| h.kind() == "parenthesized_expression") {
                holder = h.parent();
            }
            holder.is_some_and(|h| h.kind() == "variable_declarator")
        }
        _ => false,
    }
}

/// The values a function body returns, nested functions aside.
fn returned_values(body: Node) -> Vec<Node> {
    let mut out = Vec::new();
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node.kind() == "return_statement" {
            out.extend(named(node).into_iter().next());
            continue;
        }
        if node != body
            && (is_function(node) || matches!(node.kind(), "function_declaration" | "method_definition" | "class_declaration"))
        {
            continue;
        }
        push_children(&mut stack, node);
    }
    out
}

/// The parameter of `function` named `name`, with its default value.
fn param_binding<'t>(function: Node<'t>, name: &str, src: &str) -> Option<Option<Node<'t>>> {
    if let Some(single) = function.child_by_field_name("parameter") {
        return (single.utf8_text(src.as_bytes()).unwrap_or("") == name).then_some(None);
    }
    let params = function.child_by_field_name("parameters")?;
    named(params).into_iter().find_map(|param| {
        let pattern = param.child_by_field_name("pattern").unwrap_or(param);
        pattern_binding(pattern, name, param.child_by_field_name("value"), src)
    })
}

fn pattern_binding<'t>(pattern: Node<'t>, name: &str, default: Option<Node<'t>>, src: &str) -> Option<Option<Node<'t>>> {
    match pattern.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            (pattern.utf8_text(src.as_bytes()).unwrap_or("") == name).then_some(default)
        }
        "assignment_pattern" | "object_assignment_pattern" => {
            pattern_binding(pattern.child_by_field_name("left")?, name, pattern.child_by_field_name("right"), src)
        }
        "pair_pattern" => pattern_binding(pattern.child_by_field_name("value")?, name, None, src),
        "object_pattern" | "array_pattern" | "rest_pattern" => {
            named(pattern).into_iter().find_map(|m| pattern_binding(m, name, None, src))
        }
        _ => None,
    }
}

/// The calls a client makes from its arguments, axios and its instances: `get(url, config)`,
/// `post(url, data, config)`, `request(config)`.
fn axios_forward(method: &str, args: &[Slot]) -> Option<Forward> {
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    let fixed = Slot::Lit(method.to_string());
    Some(match method {
        "request" => config_forward(arg(0)),
        "get" | "delete" | "head" | "options" => Forward {
            method: fixed,
            url: arg(0),
            body: arg(1).get("data"),
            query: arg(1).get("params"),
            returns_body: false,
            url_default: None,
        },
        "post" | "put" | "patch" => Forward {
            method: fixed,
            url: arg(0),
            body: arg(1),
            query: arg(2).get("params"),
            returns_body: false,
            url_default: None,
        },
        _ => return None,
    })
}

/// An axios request config.
fn config_forward(config: Slot) -> Forward {
    Forward {
        method: config.get("method"),
        url: config.get("url"),
        body: config.get("data"),
        query: config.get("params"),
        returns_body: false,
        url_default: None,
    }
}

fn jquery_forward(method: &str, args: &[Slot]) -> Option<Forward> {
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    match method {
        "ajax" => {
            let (url, settings) = if args.len() > 1 {
                (arg(0), arg(1))
            } else {
                (arg(0).get("url"), arg(0))
            };
            // `type` or `method`: only an object literal tells which.
            let method = match &settings {
                Slot::Object(..) => match settings.get("method") {
                    Slot::Absent => settings.get("type"),
                    m => m,
                },
                Slot::Absent => Slot::Absent,
                _ => Slot::Unknown,
            };
            Some(Forward {
                method,
                url,
                body: settings.get("data"),
                query: Slot::Absent,
                returns_body: false,
                url_default: None,
            })
        }
        "get" | "getJSON" | "post" => Some(Forward {
            method: Slot::Lit(if method == "post" { "post" } else { "get" }.to_string()),
            url: arg(0),
            body: arg(1),
            query: Slot::Absent,
            returns_body: false,
            url_default: None,
        }),
        _ => None,
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
        assert!(facts.wrappers.iter().all(|w| w.name != "default.post" || w.forward.returns_body));
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

    fn line_of(script: &str, needle: &str) -> usize {
        script.lines().position(|l| l.contains(needle)).expect(needle) + 1
    }

    const FACTORY: &str = include_str!("../../tests/fixtures/js/http_client_factory.js");

    #[test]
    fn client_factory_methods_take_method_and_arguments_from_what_they_forward() {
        let facts = analyze(FACTORY, &NoImports, &[], &HashSet::new());
        let at = |needle: &str| -> Vec<&HttpCall> {
            let line = line_of(FACTORY, needle);
            facts.calls.iter().filter(|c| c.line == line).collect()
        };
        // The factory's own forwarding calls are reported where its methods are called.
        assert!(facts.calls.iter().all(|c| c.line > line_of(FACTORY, "ITEMS_ENDPOINT =")), "{:#?}", facts.calls);

        let list = at("list: (page)");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].client, "wrapper");
        assert_eq!(list[0].via.as_deref(), Some("createClient.get"));
        assert_eq!(list[0].method.as_deref(), Some("GET"));
        assert_eq!(list[0].url.as_deref(), Some("/api/items/"), "a module constant");
        assert_eq!(list[0].params, vec!["page"], "`params` of the config");

        let add = at("add: (id)");
        assert_eq!(add[0].method.as_deref(), Some("POST"));
        assert_eq!(add[0].params, vec!["id", "quantity"], "the body is the second argument of `post`");
        assert_eq!(add[0].response_keys, vec!["item"], "`request` returns the body: no envelope");

        let update = at("update: (id, quantity)");
        assert_eq!(update[0].via.as_deref(), Some("createClient.request"));
        assert_eq!(update[0].method.as_deref(), Some("PATCH"), "from the config literal");
        assert_eq!(update[0].url.as_deref(), Some("/api/items/{encodeURIComponent(id)}/"), "a local helper");
        assert_eq!(update[0].params, vec!["quantity"]);
        assert_eq!(at("remove: (id)")[0].method.as_deref(), Some("DELETE"));

        let find = at("find: (query)");
        assert_eq!(find[0].url.as_deref(), Some("/api/search/?q={query}"), "a destructured parameter's default");

        // `httpClient || createClient()`, a map indexed by a variable: one call per value.
        let load: Vec<&str> = at("return client.get(`${endpointByOperation").iter().filter_map(|c| c.url.as_deref()).collect();
        assert_eq!(load, vec!["/api/catalog/list/{suffix}?{query}", "/api/catalog/search/{suffix}?{query}"]);

        // A function the factory returns runs later: its call stays where it is, the address
        // from the parameter's default.
        let submit = at("return http.post(endpoint");
        assert_eq!(submit[0].url.as_deref(), Some("/api/forms/submit/"));
        assert_eq!(submit[0].params, vec!["name"]);

        // A client parameter without a default, a `Map`, an object that does not forward to an
        // HTTP client: no calls, their addresses stay outside recognised calls.
        let literals: Vec<&str> = facts.literals.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(literals, vec!["/api/ping/", "/api/cached/", "/api/stored/"]);
        assert_eq!(facts.calls.len(), 8, "{:#?}", facts.calls);

        let exported: Vec<(&str, bool)> = facts.clients.iter().map(|c| (c.name.as_str(), c.factory)).collect();
        assert_eq!(exported, vec![("createClient", true)]);
    }

    #[test]
    fn object_methods_forwarding_through_this_and_callbacks_that_keep_their_call() {
        let script = r#"
const api = {
    request(config) {
        return axios(config).then((response) => response.data)
    },
    apiUrl(path) {
        return this.host + '/api' + path
    },
    get(url, params) {
        return this.request({ url, params })
    },
    remove(url) {
        return this.request({ url, method: 'DELETE' })
    },
    load(page) {
        return this.get(this.apiUrl('/orders/'), { page })
    },
}
api.remove('/api/orders/7/')
page.evaluate((image) => fetch(image.src))
"#;
        let found = calls(script);
        let at = |needle: &str| found.iter().find(|c| c.line == line_of(script, needle)).unwrap_or_else(|| panic!("{needle}: {found:#?}"));
        let load = at("return this.get(");
        assert_eq!(load.via.as_deref(), Some("this.get"));
        assert_eq!(load.method.as_deref(), Some("GET"));
        assert_eq!(load.url.as_deref(), Some("{this.host}/api/orders/"));
        assert_eq!(load.params, vec!["page"]);
        let remove = at("api.remove(");
        assert_eq!(remove.via.as_deref(), Some("api.remove"));
        assert_eq!(remove.method.as_deref(), Some("DELETE"));
        assert_eq!(remove.url.as_deref(), Some("/api/orders/7/"));
        // Nothing calls the callback by name: its call is reported in place.
        assert_eq!(at("fetch(image.src)").url_expr.as_deref(), Some("image.src"));
        assert_eq!(found.len(), 3, "{found:#?}");
    }

    #[test]
    fn objects_of_unreachable_functions_keep_their_calls_and_an_iife_module_is_a_client() {
        let script = r#"
var Api = (function () {
    return { get: function (url) { return $.get(url) } }
})()
Api.get('/api/iife/')
module.exports = function createApi() {
    return { get(url) { return axios.get(url) } }
}
define(['jquery'], function ($) {
    return { post: function (url) { return $.post(url) } }
})
class Factory {
    build() { return { get(url) { return axios.get(url) } } }
}
"#;
        let found = calls(script);
        let shown: Vec<(usize, &str, Option<&str>, Option<&str>)> = found
            .iter()
            .map(|c| (c.line, c.via.as_deref().unwrap_or(c.client), c.url.as_deref(), c.url_expr.as_deref()))
            .collect();
        assert_eq!(
            shown,
            vec![
                (line_of(script, "Api.get('/api/iife/')"), "Api.get", Some("/api/iife/"), None),
                (line_of(script, "return { get(url) { return axios.get(url) } }"), "axios", None, Some("url")),
                (line_of(script, "return { post:"), "jquery", None, Some("url")),
                (line_of(script, "build() {"), "axios", None, Some("url")),
            ],
            "no one can name the object of a CommonJS export, an AMD callback, a class method: its call stays"
        );
    }

    #[test]
    fn wrappers_in_scope_spread_configs_default_addresses_and_type_casts() {
        let script = r#"
const api = axios.create()
export function get(url) { return api.get(url) }
(function () {
    function get() { return api.get('/api/fixed/') }
    function send(url, data) { return api.post(url, data) }
    send('/api/first/', { a: 1 })
})();
(function () {
    function send(url, data) { return $.get(url, data) }
    send('/api/second/', { b: 1 })
})();
function request(config) { return api.request(config) }
const base = { url: '/api/spread/', method: 'post' }
request({ ...base })
request(build())
function fetchAll(endpoint = '/api/all/') { return fetch(endpoint) }
fetchAll()
function outer(path) { return fetchAll(path || '/api/alt/') }
outer()
const typed = axios.create() as AxiosInstance
typed.get('/api/typed/')
"#;
        let found = calls(script);
        let at = |needle: &str| found.iter().find(|c| c.line == line_of(script, needle)).unwrap_or_else(|| panic!("{needle}: {found:#?}"));
        assert_eq!(at("/api/fixed/").url.as_deref(), Some("/api/fixed/"), "a local `get` is not the exported wrapper");
        assert_eq!(at("send('/api/first/'").method.as_deref(), Some("POST"));
        assert_eq!(at("send('/api/second/'").method.as_deref(), Some("GET"), "the `send` in scope");
        let spread = at("request({ ...base })");
        assert_eq!((spread.method.as_deref(), spread.url.as_deref()), (Some("POST"), Some("/api/spread/")));
        assert_eq!(at("request(build())").url_expr.as_deref(), Some("build().url"));
        assert_eq!(at("fetchAll()").url.as_deref(), Some("/api/all/"), "the parameter's default");
        assert_eq!(at("outer()").url.as_deref(), Some("/api/alt/"), "`path || …` before the inner default");
        assert_eq!(at("typed.get(").client, "axios-instance");
    }

    #[test]
    fn this_in_component_options_reaches_the_methods_group() {
        let script = r#"
export default {
    methods: {
        getData(url, params) { return $.ajax({ url: url, data: params }) },
    },
    mounted() { this.getData('/api/mounted/', { page: 1 }) },
}
"#;
        let found = calls(script);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].via.as_deref(), Some("this.getData"));
        assert_eq!(found[0].url.as_deref(), Some("/api/mounted/"));
        assert_eq!(found[0].params, vec!["page"]);
    }

    #[test]
    fn forwarding_methods_of_component_options_keep_their_call_unless_called_through_this() {
        let script = r#"
new Vue({
    methods: {
        getData(url, params) {
            return $.ajax({ url: url, data: params })
        },
        send(url) {
            return $.ajax({ url: url, type: 'POST' })
        },
        load() {
            this.getData('/api/list/', { page: 1 })
        },
    },
})
"#;
        let found = calls(script);
        let urls: Vec<(usize, Option<&str>, Option<&str>)> =
            found.iter().map(|c| (c.line, c.url.as_deref(), c.url_expr.as_deref())).collect();
        assert_eq!(
            urls,
            vec![
                (line_of(script, "type: 'POST'"), None, Some("url")),
                (line_of(script, "this.getData("), Some("/api/list/"), None)
            ],
            "`send` is called nowhere: its own call stays"
        );
        assert_eq!(found[1].params, vec!["page"]);
    }
}
