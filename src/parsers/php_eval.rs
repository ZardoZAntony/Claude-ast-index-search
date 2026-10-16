//! Values of PHP expressions that are fixed in the source: literals, concatenation, arithmetic,
//! arrays, `Foo::class`, class constants and enum cases (through a lookup the caller provides),
//! variables bound by an enclosing loop or assigned once before. Anything else stays the source
//! text, so a caller can tell a value from an expression it could not fold.

use std::collections::HashMap;

use serde_json::{json, Map, Value as Json};
use tree_sitter::Node;

use super::php_ast::{self, Names};

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    List(Vec<Val>),
    Map(Vec<(Val, Val)>),
    /// `new Foo(...)`: the class with its positional and named arguments.
    New {
        class: String,
        args: Vec<Val>,
        named: Vec<(String, Val)>,
    },
    /// An enum case, with its backing value when it has one.
    Case {
        fqn: String,
        value: Option<Box<Val>>,
    },
    /// A closure or arrow function: its lines.
    Closure { line: usize, end_line: usize },
    /// Not foldable: the expression as written.
    Expr(String),
}

impl Val {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_known(&self) -> bool {
        !matches!(self, Val::Expr(_))
    }

    /// The value as a string the way PHP would concatenate it.
    fn to_php_string(&self) -> Option<String> {
        match self {
            Val::Str(s) => Some(s.clone()),
            Val::Int(i) => Some(i.to_string()),
            Val::Float(f) => Some(f.to_string()),
            Val::Bool(true) => Some("1".into()),
            Val::Bool(false) | Val::Null => Some(String::new()),
            Val::Case { value: Some(v), .. } => v.to_php_string(),
            _ => None,
        }
    }

    pub fn to_json(&self) -> Json {
        match self {
            Val::Str(s) => json!(s),
            Val::Int(i) => json!(i),
            Val::Float(f) => json!(f),
            Val::Bool(b) => json!(b),
            Val::Null => Json::Null,
            Val::List(items) => Json::Array(items.iter().map(Val::to_json).collect()),
            Val::Map(pairs) => {
                let mut map = Map::new();
                for (k, v) in pairs {
                    let key = k.to_php_string().unwrap_or_else(|| format!("{k:?}"));
                    map.insert(key, v.to_json());
                }
                Json::Object(map)
            }
            Val::New { class, args, named } => {
                let mut obj = Map::new();
                obj.insert("new".into(), json!(class));
                if !args.is_empty() {
                    obj.insert("args".into(), Json::Array(args.iter().map(Val::to_json).collect()));
                }
                if !named.is_empty() {
                    let mut n = Map::new();
                    for (k, v) in named {
                        n.insert(k.clone(), v.to_json());
                    }
                    obj.insert("named".into(), Json::Object(n));
                }
                Json::Object(obj)
            }
            Val::Case { fqn, value } => {
                let mut obj = Map::new();
                obj.insert("case".into(), json!(fqn));
                if let Some(v) = value {
                    obj.insert("value".into(), v.to_json());
                }
                Json::Object(obj)
            }
            Val::Closure { line, end_line } => json!({"closure": format!("{line}-{end_line}")}),
            Val::Expr(text) => json!({"expr": text}),
        }
    }
}

/// Class constants and enum cases by class FQN, from wherever the caller keeps classes.
pub trait Constants {
    /// `Foo::NAME`: a constant's value or an enum case (`Val::Case`); `None` when unknown.
    fn class_constant(&self, class: &str, name: &str, depth: usize) -> Option<Val>;
}

/// No class lookup: class constants stay expressions.
pub struct NoConstants;

impl Constants for NoConstants {
    fn class_constant(&self, _: &str, _: &str, _: usize) -> Option<Val> {
        None
    }
}

/// Constant lookups follow constants that name constants; this bounds a cycle.
pub const MAX_DEPTH: usize = 8;

pub struct Evaluator<'a> {
    pub src: &'a str,
    pub names: &'a Names,
    /// The class `self::`/`static::`/`__CLASS__` mean, if the code is inside one.
    pub class: Option<String>,
    pub constants: &'a dyn Constants,
    /// Variables with a fixed value here (a loop variable of one unrolled iteration).
    pub bindings: HashMap<String, Val>,
    pub depth: usize,
}

impl<'a> Evaluator<'a> {
    pub fn new(src: &'a str, names: &'a Names, constants: &'a dyn Constants) -> Self {
        Evaluator {
            src,
            names,
            class: None,
            constants,
            bindings: HashMap::new(),
            depth: 0,
        }
    }

    fn text(&self, node: Node) -> &'a str {
        php_ast::text(node, self.src)
    }

    fn expr(&self, node: Node) -> Val {
        Val::Expr(php_ast::render(node, self.src, None))
    }

    pub fn eval(&self, node: Node) -> Val {
        match node.kind() {
            "string" | "encapsed_string" => self.string(node),
            "integer" => parse_int(self.text(node)).map(Val::Int).unwrap_or_else(|| self.expr(node)),
            "float" => self
                .text(node)
                .replace('_', "")
                .parse()
                .map(Val::Float)
                .unwrap_or_else(|_| self.expr(node)),
            "boolean" => Val::Bool(self.text(node).eq_ignore_ascii_case("true")),
            "null" => Val::Null,
            "parenthesized_expression" | "argument" => {
                // `name: value`: the label is a child too.
                let label = node.child_by_field_name("name");
                match php_ast::named_children(node)
                    .into_iter()
                    .rfind(|c| Some(*c) != label)
                {
                    Some(inner) => self.eval(inner),
                    None => self.expr(node),
                }
            }
            "array_creation_expression" => self.array(node),
            "binary_expression" => self.binary(node),
            "unary_op_expression" => self.unary(node),
            "class_constant_access_expression" => self.class_constant(node),
            "member_access_expression" | "nullsafe_member_access_expression" => {
                self.enum_member(node)
            }
            "object_creation_expression" => self.new_object(node),
            "anonymous_function" | "arrow_function" => Val::Closure {
                line: php_ast::line(node),
                end_line: php_ast::end_line(node),
            },
            "name" => match self.text(node).to_ascii_uppercase().as_str() {
                "__CLASS__" => self
                    .class
                    .clone()
                    .map(Val::Str)
                    .unwrap_or_else(|| self.expr(node)),
                "PHP_EOL" => Val::Str("\n".into()),
                _ => self.expr(node),
            },
            "variable_name" => {
                let var = self.text(node).trim_start_matches('$');
                if let Some(v) = self.bindings.get(var) {
                    return v.clone();
                }
                self.assigned_value(var, node)
                    .unwrap_or_else(|| self.expr(node))
            }
            _ => self.expr(node),
        }
    }

    fn string(&self, node: Node) -> Val {
        let raw = self.text(node);
        if node.kind() == "encapsed_string"
            && php_ast::named_children(node)
                .iter()
                .any(|c| !matches!(c.kind(), "string_content" | "escape_sequence"))
        {
            return self.expr(node); // "a{$b}": interpolation
        }
        // A string the parser recovered without its closing quote is no value.
        let (quote, inner) = match raw.chars().next() {
            Some(q @ ('\'' | '"')) => match raw[1..].strip_suffix(q) {
                Some(inner) => (q, inner),
                None => return self.expr(node),
            },
            _ => return self.expr(node),
        };
        Val::Str(if quote == '\'' {
            inner.replace("\\'", "'").replace("\\\\", "\\")
        } else {
            unescape_double(inner)
        })
    }

    fn array(&self, node: Node) -> Val {
        let mut list = Vec::new();
        let mut pairs: Vec<(Val, Val)> = Vec::new();
        let mut keyed = false;
        let mut next_index = 0i64;
        for element in php_ast::named_children(node) {
            if element.kind() != "array_element_initializer" {
                continue;
            }
            let parts = php_ast::named_children(element);
            let has_arrow = php_ast::has_child_kind(element, "=>");
            if php_ast::has_child_kind(element, "...") {
                return self.expr(node);
            }
            match (has_arrow, parts.as_slice()) {
                (true, [key, value]) => {
                    keyed = true;
                    let k = self.eval(*key);
                    if let Val::Int(i) = k {
                        next_index = next_index.max(i + 1);
                    }
                    pairs.push((k, self.eval(*value)));
                }
                (false, [value]) => {
                    let v = self.eval(*value);
                    pairs.push((Val::Int(next_index), v.clone()));
                    next_index += 1;
                    list.push(v);
                }
                _ => return self.expr(node),
            }
        }
        if keyed {
            Val::Map(pairs)
        } else {
            Val::List(list)
        }
    }

    fn binary(&self, node: Node) -> Val {
        let (Some(left), Some(right), Some(op)) = (
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
            node.child_by_field_name("operator"),
        ) else {
            return self.expr(node);
        };
        let op = self.text(op);
        let (l, r) = (self.eval(left), self.eval(right));
        let folded = match op {
            "." => match (l.to_php_string(), r.to_php_string()) {
                (Some(a), Some(b)) => Some(Val::Str(a + &b)),
                _ => None,
            },
            "??" => match &l {
                Val::Null => Some(r),
                v if v.is_known() => Some(l.clone()),
                _ => None,
            },
            "+" | "-" | "*" | "/" | "%" | "**" => arithmetic(op, &l, &r),
            _ => None,
        };
        folded.unwrap_or_else(|| self.expr(node))
    }

    fn unary(&self, node: Node) -> Val {
        let Some(operand) = php_ast::named_children(node).into_iter().next() else {
            return self.expr(node);
        };
        let op = self.text(node).trim_start().chars().next();
        match (op, self.eval(operand)) {
            (Some('-'), Val::Int(i)) => Val::Int(-i),
            (Some('-'), Val::Float(f)) => Val::Float(-f),
            (Some('+'), v @ (Val::Int(_) | Val::Float(_))) => v,
            (Some('!'), Val::Bool(b)) => Val::Bool(!b),
            _ => self.expr(node),
        }
    }

    /// The class a scope before `::` names.
    pub fn scope_class(&self, scope: Node) -> Option<String> {
        match scope.kind() {
            "relative_scope" => match self.text(scope).to_ascii_lowercase().as_str() {
                "self" | "static" => self.class.clone(),
                _ => None,
            },
            "name" | "qualified_name" | "relative_name" => {
                let written = self.text(scope);
                match written.to_ascii_lowercase().as_str() {
                    "self" | "static" => self.class.clone(),
                    "parent" => None,
                    _ => Some(self.names.resolve_class(written, scope.start_byte())),
                }
            }
            _ => None,
        }
    }

    fn class_constant(&self, node: Node) -> Val {
        let parts = php_ast::named_children(node);
        let (Some(scope), Some(member)) = (parts.first(), parts.get(1)) else {
            return self.expr(node);
        };
        let Some(class) = self.scope_class(*scope) else {
            return self.expr(node);
        };
        let member = self.text(*member);
        if member.eq_ignore_ascii_case("class") {
            return Val::Str(class);
        }
        if self.depth >= MAX_DEPTH {
            return self.expr(node);
        }
        self.constants
            .class_constant(&class, member, self.depth + 1)
            .unwrap_or_else(|| self.expr(node))
    }

    /// `Enum::CASE->value`, `Enum::CASE->name`.
    fn enum_member(&self, node: Node) -> Val {
        let (Some(object), Some(name)) = (
            node.child_by_field_name("object"),
            node.child_by_field_name("name"),
        ) else {
            return self.expr(node);
        };
        match (self.eval(object), self.text(name)) {
            (Val::Case { value: Some(v), .. }, "value") => *v,
            (Val::Case { fqn, .. }, "name") => {
                Val::Str(fqn.rsplit("::").next().unwrap_or_default().to_string())
            }
            _ => self.expr(node),
        }
    }

    fn new_object(&self, node: Node) -> Val {
        let children = php_ast::named_children(node);
        let Some(class) = children
            .iter()
            .find(|n| matches!(n.kind(), "name" | "qualified_name" | "relative_name"))
            .and_then(|n| self.scope_class(*n))
        else {
            return self.expr(node);
        };
        let (args, named) = children
            .iter()
            .find(|n| n.kind() == "arguments")
            .map(|a| self.arguments(*a))
            .unwrap_or_default();
        Val::New { class, args, named }
    }

    /// Positional and named arguments of a call.
    pub fn arguments(&self, arguments: Node) -> (Vec<Val>, Vec<(String, Val)>) {
        let mut args = Vec::new();
        let mut named = Vec::new();
        for argument in php_ast::named_children(arguments) {
            if argument.kind() != "argument" {
                continue;
            }
            let value = self.eval(argument);
            match argument.child_by_field_name("name") {
                Some(name) => named.push((self.text(name).to_string(), value)),
                None => args.push(value),
            }
        }
        (args, named)
    }

    /// The value of `$var` from its one assignment before `at` in the same function (or the
    /// file's top-level code), when nothing changes it on the way: no `.=`/`+=`, `++`, list
    /// destructuring, reference, `unset`, loop binding, and no call taking it as an argument
    /// (it may take it by reference) between the assignment and `at`.
    fn assigned_value(&self, var: &str, at: Node) -> Option<Val> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        let is_var = |n: Node| n.kind() == "variable_name" && self.text(n).trim_start_matches('$') == var;
        let mentions_var = |n: Node| {
            let mut inner = vec![n];
            while let Some(x) = inner.pop() {
                if is_var(x) {
                    return true;
                }
                if !is_function_like(x.kind()) {
                    inner.extend(php_ast::named_children(x));
                }
            }
            false
        };
        let scope = enclosing_scope(at);
        let mut assignments: Vec<Node> = Vec::new();
        // Start bytes of places that change the variable or may change it.
        let mut changes: Vec<usize> = Vec::new();
        let mut stack = vec![scope];
        while let Some(node) = stack.pop() {
            if node.start_byte() >= at.start_byte() {
                continue;
            }
            if node != scope && is_function_like(node.kind()) {
                continue;
            }
            match node.kind() {
                "assignment_expression" => {
                    match node.child_by_field_name("left") {
                        Some(l) if is_var(l) => {
                            if node.end_byte() > at.start_byte() {
                                return None; // `$x = f($x)`: the value is being computed
                            }
                            assignments.push(node);
                        }
                        // `[$x, $y] = …`, `list($x) = …`
                        Some(l) if mentions_var(l) => changes.push(node.start_byte()),
                        _ => {}
                    }
                }
                "augmented_assignment_expression" | "reference_assignment_expression" => {
                    if mentions_var(node) {
                        changes.push(node.start_byte());
                    }
                }
                "update_expression" | "unset_statement" => {
                    if mentions_var(node) {
                        changes.push(node.start_byte());
                    }
                }
                "foreach_statement" | "global_declaration" | "function_static_declaration" => {
                    let binds = php_ast::named_children(node)
                        .into_iter()
                        .skip(1)
                        .filter(|n| n.kind() != "compound_statement")
                        .any(mentions_var);
                    if binds {
                        return None;
                    }
                }
                "argument" => {
                    // Passed to a call that is not the use itself.
                    let passed = php_ast::named_children(node).into_iter().any(is_var);
                    let call = node.parent().and_then(|args| args.parent());
                    let around_use = call.is_some_and(|c| c.start_byte() <= at.start_byte() && at.end_byte() <= c.end_byte());
                    if passed && !around_use {
                        changes.push(node.start_byte());
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        // Assigned more than once: which one applies depends on the path taken.
        let [only] = assignments.as_slice() else {
            return None;
        };
        if changes.iter().any(|c| *c >= only.end_byte()) {
            return None;
        }
        let right = only.child_by_field_name("right")?;
        let nested = Evaluator {
            src: self.src,
            names: self.names,
            class: self.class.clone(),
            constants: self.constants,
            bindings: self.bindings.clone(),
            depth: self.depth + 1,
        };
        let value = nested.eval(right);
        value.is_known().then_some(value)
    }
}

fn is_function_like(kind: &str) -> bool {
    matches!(
        kind,
        "method_declaration" | "function_definition" | "anonymous_function" | "arrow_function"
    )
}

/// The function body or the file a node's variables live in.
pub fn enclosing_scope(node: Node) -> Node {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if is_function_like(parent.kind()) {
            return parent;
        }
        current = parent;
    }
    current
}

fn parse_int(text: &str) -> Option<i64> {
    let t = text.replace('_', "");
    let lower = t.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix("0x") {
        i64::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = lower.strip_prefix("0b") {
        i64::from_str_radix(bin, 2).ok()
    } else if let Some(oct) = lower.strip_prefix("0o") {
        i64::from_str_radix(oct, 8).ok()
    } else if lower.len() > 1 && lower.starts_with('0') {
        i64::from_str_radix(&lower[1..], 8).ok()
    } else {
        lower.parse().ok()
    }
}

fn arithmetic(op: &str, l: &Val, r: &Val) -> Option<Val> {
    match (l, r) {
        (Val::Int(a), Val::Int(b)) => match op {
            "+" => a.checked_add(*b).map(Val::Int),
            "-" => a.checked_sub(*b).map(Val::Int),
            "*" => a.checked_mul(*b).map(Val::Int),
            "/" if *b != 0 && a % b == 0 => Some(Val::Int(a / b)),
            "/" if *b != 0 => Some(Val::Float(*a as f64 / *b as f64)),
            "%" if *b != 0 => Some(Val::Int(a % b)),
            "**" if *b >= 0 => u32::try_from(*b).ok().and_then(|e| a.checked_pow(e)).map(Val::Int),
            _ => None,
        },
        _ => {
            let a = number(l)?;
            let b = number(r)?;
            match op {
                "+" => Some(Val::Float(a + b)),
                "-" => Some(Val::Float(a - b)),
                "*" => Some(Val::Float(a * b)),
                "/" if b != 0.0 => Some(Val::Float(a / b)),
                "**" => Some(Val::Float(a.powf(b))),
                _ => None,
            }
        }
    }
}

fn number(v: &Val) -> Option<f64> {
    match v {
        Val::Int(i) => Some(*i as f64),
        Val::Float(f) => Some(*f),
        _ => None,
    }
}

fn unescape_double(inner: &str) -> String {
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('$') => out.push('$'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluates the expression of the last `return` in `src`.
    fn eval_return(src: &str) -> Val {
        let tree = php_ast::parse(src).unwrap();
        let names = Names::of(tree.root_node(), src);
        let mut stack = vec![tree.root_node()];
        let mut last = None;
        while let Some(n) = stack.pop() {
            if n.kind() == "return_statement" {
                last = Some(n);
            }
            let mut c = n.walk();
            let children: Vec<Node> = n.children(&mut c).collect();
            stack.extend(children.into_iter().rev());
        }
        let expr = php_ast::named_children(last.unwrap()).into_iter().next().unwrap();
        Evaluator::new(src, &names, &NoConstants).eval(expr)
    }

    #[test]
    fn folds_literals_concatenation_and_arithmetic() {
        assert_eq!(eval_return("<?php return 'a' . \"b\\n\" . 1;"), Val::Str("ab\n1".into()));
        assert_eq!(eval_return("<?php return 60 * 60 * 24;"), Val::Int(86400));
        assert_eq!(eval_return("<?php return -5 + 0x10;"), Val::Int(11));
        assert_eq!(eval_return("<?php return 7 / 2;"), Val::Float(3.5));
        assert_eq!(eval_return("<?php return null ?? 'x';"), Val::Str("x".into()));
    }

    #[test]
    fn folds_arrays_and_class_strings() {
        let src = "<?php\nnamespace App;\nuse Lib\\Foo;\nreturn [Foo::class, 'k' => ['a', 'b']];\n";
        assert_eq!(
            eval_return(src),
            Val::Map(vec![
                (Val::Int(0), Val::Str("Lib\\Foo".into())),
                (Val::Str("k".into()), Val::List(vec![Val::Str("a".into()), Val::Str("b".into())])),
            ])
        );
    }

    #[test]
    fn variables_assigned_once_fold_and_interpolation_stays_an_expression() {
        assert_eq!(
            eval_return("<?php\n$prefix = 'Order';\nreturn $prefix . 'OnAdd';\n"),
            Val::Str("OrderOnAdd".into())
        );
        assert!(!eval_return("<?php\n$a = 'x';\n$a = 'y';\nreturn $a;\n").is_known());
        assert!(!eval_return("<?php return \"a{$b}\";").is_known());
    }

    #[test]
    fn a_variable_changed_after_its_assignment_does_not_fold() {
        for changed in [
            "$p = 'Order';\n$p .= 'Extra';\nreturn $p;",
            "$p = 1;\n$p++;\nreturn $p;",
            "$p = 'a';\n[$p, $q] = f();\nreturn $p;",
            "$p = 'a';\n$r = &$p;\nreturn $p;",
            "$p = 'a';\nunset($p);\nreturn $p;",
            "$p = 'a';\n$this->mutate($p);\nreturn $p;",
            "foreach ([1, 2] as $p) {}\nreturn $p;",
        ] {
            let value = eval_return(&format!("<?php\n{changed}\n"));
            assert!(!value.is_known(), "{changed} folded to {value:?}");
        }
        let Val::New { args, .. } = eval_return("<?php\n$p = ['a'];\nreturn new Dto($p);\n") else {
            panic!("not new");
        };
        assert_eq!(
            args,
            vec![Val::List(vec![Val::Str("a".into())])],
            "the call holding the use is the use, not a change"
        );
        assert_eq!(eval_return("<?php\n$p = 'a';\nreturn [$p];\n"), Val::List(vec![Val::Str("a".into())]));
    }

    #[test]
    fn a_string_without_its_closing_quote_is_no_value() {
        let src = "<?php return 'привет";
        let tree = php_ast::parse(src).unwrap();
        let names = Names::of(tree.root_node(), src);
        let mut stack = vec![tree.root_node()];
        while let Some(n) = stack.pop() {
            if n.kind() == "string" {
                assert!(!Evaluator::new(src, &names, &NoConstants).eval(n).is_known());
            }
            stack.extend(php_ast::named_children(n));
        }
    }

    #[test]
    fn new_keeps_class_and_named_arguments() {
        let src = "<?php\nuse App\\Dto;\nreturn new Dto(name: 'load', items: [1, 2]);\n";
        let Val::New { class, named, args } = eval_return(src) else {
            panic!("not new");
        };
        assert_eq!(class, "App\\Dto");
        assert!(args.is_empty());
        assert_eq!(named[0], ("name".into(), Val::Str("load".into())));
    }
}
