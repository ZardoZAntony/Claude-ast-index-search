//! The calls a PHP method makes, each with the type its receiver resolves to — read from the
//! syntax tree, so a call spread over lines is one call at the line it starts on.
//!
//! A receiver's type comes from what the code states: `$this`, `self::`/`static::`/`parent::`,
//! `Foo::`, `new Foo`, a typed or promoted property, a typed parameter, `/** @var Foo $x */`,
//! the declared return type of a method called on a known type, and a `class-string<T>`
//! template (`getHandler(Foo::class)` returning `T`). A variable whose latest binding states no
//! type — a `foreach`, a destructuring, an unknown call — is unknown rather than guessed.

use std::collections::HashSet;
use std::rc::Rc;

use anyhow::Result;
use tree_sitter::Node;

use super::php_ast::{self, Names};
use super::php_model::{self, ClassModel, MethodModel, TypeRef};

/// Class declarations by FQN, from wherever the caller keeps them (the index, a test fixture).
pub trait Classes {
    fn class(&self, fqn: &str) -> Option<Rc<ClassModel>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstRef {
    /// As written, the class part resolved for `self::`/`static::`: `Enum::CASE`.
    pub written: String,
    /// `Ns\Enum::CASE`.
    pub fqn: String,
}

#[derive(Debug, Clone)]
pub struct CallSite {
    pub line: usize,
    /// `method`, `static`, `new` or `function`.
    pub kind: &'static str,
    /// The method called (`__construct` for `new`), or the function.
    pub method: String,
    /// The receiver as written: `$this->dispatcher`, `self`, `$handler`, `Foo`.
    pub receiver: String,
    pub receiver_type: Option<String>,
    /// What stated the type: `this`, `self`, `static`, `parent`, `class`, `new`, `property`,
    /// `param`, `@var`, `return`, `template`, `catch`, `enum`; `unknown` when nothing did.
    pub resolved_by: &'static str,
    /// Class constants and enum cases passed as arguments (not those of nested calls).
    pub constants: Vec<ConstRef>,
    /// Classes passed as `Foo::class`.
    pub classes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MethodCalls {
    pub line: usize,
    pub end_line: usize,
    pub calls: Vec<CallSite>,
}

/// Calls made by `class_fqn::method` as declared in `content`; `None` when the file does not
/// declare that method with a body. `this_class` is the class `$this`, `self::` and `static::`
/// stand for: the declaring class, or for a trait method the class using the trait.
pub fn method_calls(
    content: &str,
    class_fqn: &str,
    method: &str,
    this_class: &str,
    classes: &dyn Classes,
) -> Result<Option<MethodCalls>> {
    let tree = php_ast::parse(content)?;
    let root = tree.root_node();
    let names = Names::of(root, content);
    let Some(class_node) = php_model::find_class_node(root, content, &names, class_fqn) else {
        return Ok(None);
    };
    let Some(method_node) = class_node.child_by_field_name("body").and_then(|body| {
        php_ast::named_children(body).into_iter().find(|m| {
            m.kind() == "method_declaration"
                && m.child_by_field_name("name")
                    .is_some_and(|n| php_ast::text(n, content).eq_ignore_ascii_case(method))
        })
    }) else {
        return Ok(None);
    };
    let Some(body) = method_node.child_by_field_name("body") else {
        return Ok(None);
    };
    let this = classes.class(this_class);
    let ctx = Ctx {
        src: content,
        names: &names,
        class: this.as_ref().map(|c| c.fqn.clone()).unwrap_or_else(|| this_class.to_string()),
        parent: this.as_ref().and_then(|c| c.parent.clone()),
        classes,
    };
    let mut found = Vec::new();
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if let Some(site) = ctx.call_site(node) {
            found.push((node.end_byte(), site));
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    // In the order calls run: `$a->b()->c()` and `f(g())` finish the inner call first.
    found.sort_by_key(|(end, site)| (site.line, *end));
    Ok(Some(MethodCalls {
        line: php_ast::line(method_node),
        end_line: php_ast::end_line(method_node),
        calls: found.into_iter().map(|(_, site)| site).collect(),
    }))
}

/// The method `name` a type has: declared in the class, a trait it uses, a parent, or — for an
/// abstract class or an interface — an interface. Returns the declaring class with it.
pub fn find_method(
    classes: &dyn Classes,
    fqn: &str,
    name: &str,
) -> Option<(Rc<ClassModel>, MethodModel)> {
    let mut seen = HashSet::new();
    find_method_in(classes, fqn, name, &mut seen)
}

fn find_method_in(
    classes: &dyn Classes,
    fqn: &str,
    name: &str,
    seen: &mut HashSet<String>,
) -> Option<(Rc<ClassModel>, MethodModel)> {
    if !seen.insert(fqn.to_ascii_lowercase()) {
        return None;
    }
    let class = classes.class(fqn)?;
    if let Some(method) = class.method(name) {
        return Some((class.clone(), method.clone()));
    }
    let inherited = class
        .traits
        .iter()
        .chain(class.parent.iter())
        .chain(class.interfaces.iter())
        .cloned()
        .collect::<Vec<_>>();
    inherited
        .iter()
        .find_map(|next| find_method_in(classes, next, name, seen))
}

/// Type of property `$name` on `fqn`, looking through traits and parents.
fn property_type(classes: &dyn Classes, fqn: &str, name: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let mut queue = vec![fqn.to_string()];
    while let Some(current) = queue.pop() {
        if !seen.insert(current.to_ascii_lowercase()) {
            continue;
        }
        let Some(class) = classes.class(&current) else {
            continue;
        };
        if let Some(t) = class.properties.get(name) {
            return match t {
                TypeRef::Class(c) => Some(c.clone()),
                TypeRef::SelfType => Some(class.fqn.clone()),
                TypeRef::Static => Some(fqn.to_string()),
            };
        }
        queue.extend(class.traits.iter().cloned());
        queue.extend(class.parent.iter().cloned());
    }
    None
}

struct Ctx<'a> {
    src: &'a str,
    names: &'a Names,
    class: String,
    parent: Option<String>,
    classes: &'a dyn Classes,
}

type Typed = Option<(String, &'static str)>;

const FUNCTION_LIKE: [&str; 4] = [
    "method_declaration",
    "function_definition",
    "anonymous_function",
    "arrow_function",
];

/// Written like calls, but language constructs, not functions.
const LANGUAGE_CONSTRUCTS: [&str; 8] = [
    "isset", "empty", "unset", "eval", "exit", "die", "list", "array",
];

const CALL_KINDS: [&str; 5] = [
    "member_call_expression",
    "nullsafe_member_call_expression",
    "scoped_call_expression",
    "object_creation_expression",
    "function_call_expression",
];

impl Ctx<'_> {
    fn text(&self, node: Node) -> &str {
        php_ast::text(node, self.src)
    }

    fn call_site(&self, node: Node) -> Option<CallSite> {
        let (kind, method, receiver, typed, arguments) = match node.kind() {
            "member_call_expression" | "nullsafe_member_call_expression" => {
                let object = node.child_by_field_name("object")?;
                let name = node.child_by_field_name("name")?;
                if name.kind() != "name" {
                    return None; // `$this->$method()`: no name to follow
                }
                (
                    "method",
                    self.text(name).to_string(),
                    self.text(object).to_string(),
                    self.type_of(object),
                    node.child_by_field_name("arguments"),
                )
            }
            "scoped_call_expression" => {
                let scope = node.child_by_field_name("scope")?;
                let name = node.child_by_field_name("name")?;
                if name.kind() != "name" {
                    return None;
                }
                (
                    "static",
                    self.text(name).to_string(),
                    self.text(scope).to_string(),
                    self.scope_type(scope),
                    node.child_by_field_name("arguments"),
                )
            }
            "object_creation_expression" => {
                let class = php_ast::named_children(node).into_iter().find(|n| {
                    matches!(n.kind(), "name" | "qualified_name" | "relative_name")
                })?;
                (
                    "new",
                    "__construct".to_string(),
                    self.text(class).to_string(),
                    self.class_name_type(class).map(|t| (t, "new")),
                    php_ast::named_children(node)
                        .into_iter()
                        .find(|n| n.kind() == "arguments"),
                )
            }
            "function_call_expression" => {
                let function = node.child_by_field_name("function")?;
                if !matches!(function.kind(), "name" | "qualified_name") {
                    return None;
                }
                let written = self.text(function);
                // `\count()` is the global built-in written fully qualified.
                let global = written.strip_prefix('\\').unwrap_or(written);
                let lower = global.to_ascii_lowercase();
                if !global.contains('\\')
                    && (super::treesitter::php::BUILTIN_FUNCTIONS.contains(lower.as_str())
                        || LANGUAGE_CONSTRUCTS.contains(&lower.as_str()))
                {
                    return None;
                }
                (
                    "function",
                    written.trim_start_matches('\\').to_string(),
                    String::new(),
                    None,
                    node.child_by_field_name("arguments"),
                )
            }
            _ => return None,
        };
        let (constants, classes) = arguments
            .map(|a| self.argument_constants(a))
            .unwrap_or_default();
        let (receiver_type, resolved_by) = match typed {
            Some((t, by)) => (Some(t), by),
            None => (None, if kind == "function" { "function" } else { "unknown" }),
        };
        Some(CallSite {
            line: php_ast::line(node),
            kind,
            method,
            receiver,
            receiver_type,
            resolved_by,
            constants,
            classes,
        })
    }

    /// `self`, `static`, `parent`, or a class name before `::`.
    fn scope_type(&self, scope: Node) -> Typed {
        match scope.kind() {
            "relative_scope" => match self.text(scope).to_ascii_lowercase().as_str() {
                "self" => Some((self.class.clone(), "self")),
                "static" => Some((self.class.clone(), "static")),
                "parent" => self.parent.clone().map(|p| (p, "parent")),
                _ => None,
            },
            "name" | "qualified_name" | "relative_name" => {
                self.class_name_type(scope).map(|t| (t, "class"))
            }
            _ => self.type_of(scope),
        }
    }

    fn class_name_type(&self, name: Node) -> Option<String> {
        let written = self.text(name);
        match written.to_ascii_lowercase().as_str() {
            "self" | "static" => Some(self.class.clone()),
            "parent" => self.parent.clone(),
            _ => Some(self.names.resolve_class(written, name.start_byte())),
        }
    }

    /// Type of an expression, as far as the code states it.
    fn type_of(&self, expr: Node) -> Typed {
        match expr.kind() {
            "variable_name" => {
                let var = self.text(expr).trim_start_matches('$');
                if var == "this" {
                    return Some((self.class.clone(), "this"));
                }
                self.variable_type(var, expr)
            }
            "parenthesized_expression" => self.type_of(php_ast::named_children(expr).into_iter().next()?),
            "object_creation_expression" => {
                let class = php_ast::named_children(expr).into_iter().find(|n| {
                    matches!(n.kind(), "name" | "qualified_name" | "relative_name")
                })?;
                self.class_name_type(class).map(|t| (t, "new"))
            }
            "member_access_expression" | "nullsafe_member_access_expression" => {
                let (owner, _) = self.type_of(expr.child_by_field_name("object")?)?;
                let name = expr.child_by_field_name("name")?;
                property_type(self.classes, &owner, self.text(name)).map(|t| (t, "property"))
            }
            "member_call_expression" | "nullsafe_member_call_expression" => {
                let (owner, _) = self.type_of(expr.child_by_field_name("object")?)?;
                let name = self.text(expr.child_by_field_name("name")?);
                self.return_type(&owner, name, expr)
            }
            "scoped_call_expression" => {
                let (owner, _) = self.scope_type(expr.child_by_field_name("scope")?)?;
                let name = self.text(expr.child_by_field_name("name")?);
                self.return_type(&owner, name, expr)
            }
            "class_constant_access_expression" => {
                // `Status::Active`: an enum case is an instance of its enum.
                let parts = php_ast::named_children(expr);
                let (scope, member) = (parts.first()?, parts.get(1)?);
                let (owner, _) = self.scope_type(*scope)?;
                let class = self.classes.class(&owner)?;
                let case = self.text(*member);
                class
                    .cases
                    .iter()
                    .any(|c| c.name == case)
                    .then(|| (class.fqn.clone(), "enum"))
            }
            "clone_expression" => self.type_of(php_ast::named_children(expr).into_iter().next()?),
            "binary_expression" => {
                let op = expr.child_by_field_name("operator")?;
                if self.text(op) != "??" {
                    return None;
                }
                self.type_of(expr.child_by_field_name("left")?)
                    .or_else(|| self.type_of(expr.child_by_field_name("right")?))
            }
            _ => None,
        }
    }

    /// The type a call to `owner::name` returns.
    fn return_type(&self, owner: &str, name: &str, call: Node) -> Typed {
        let (declaring, method) = find_method(self.classes, owner, name)?;
        if let Some(position) = method.returns_class_of_param {
            let argument = call
                .child_by_field_name("arguments")
                .map(php_ast::named_children)
                .and_then(|args| args.into_iter().nth(position));
            if let Some(class) = argument.and_then(|a| self.class_string(a)) {
                return Some((class, "template"));
            }
        }
        let t = match method.returns? {
            TypeRef::Class(c) => c,
            TypeRef::SelfType => declaring.fqn.clone(),
            TypeRef::Static => owner.to_string(),
        };
        Some((t, "return"))
    }

    /// `Foo::class` in an argument.
    fn class_string(&self, argument: Node) -> Option<String> {
        let expr = php_ast::named_children(argument).into_iter().next()?;
        if expr.kind() != "class_constant_access_expression" {
            return None;
        }
        let parts = php_ast::named_children(expr);
        let (scope, member) = (parts.first()?, parts.get(1)?);
        if !self.text(*member).eq_ignore_ascii_case("class") {
            return None;
        }
        self.scope_type(*scope).map(|(t, _)| t)
    }

    /// Type of `$var` at `at`: the latest binding before it in the enclosing function — a
    /// typed parameter, an assignment, `/** @var */`, `catch` — or, inside a closure or an
    /// arrow function, in the code around it.
    fn variable_type(&self, var: &str, at: Node) -> Typed {
        let mut scope = enclosing_function(at)?;
        let mut position = at.start_byte();
        loop {
            if let Some(found) = self.binding_in(scope, var, position) {
                return found;
            }
            let captures = match scope.kind() {
                "arrow_function" => true,
                "anonymous_function" => closure_captures(scope, var, self.src),
                _ => false,
            };
            if !captures {
                return None;
            }
            position = scope.start_byte();
            scope = enclosing_function(scope)?;
        }
    }

    /// `Some(type)` when the latest binding of `$var` in `function` before `position` is found
    /// (the type itself may be unknown), `None` when the function does not bind it.
    fn binding_in(&self, function: Node, var: &str, position: usize) -> Option<Typed> {
        let mut latest: Option<(usize, Typed)> = None;
        let mut consider = |at: usize, typed: Typed| {
            if latest.as_ref().is_none_or(|(p, _)| at >= *p) {
                latest = Some((at, typed));
            }
        };
        if let Some(params) = function.child_by_field_name("parameters") {
            for param in php_ast::named_children(params) {
                let named = param
                    .child_by_field_name("name")
                    .is_some_and(|n| self.text(n).trim_start_matches('$') == var);
                if named {
                    let typed = param
                        .child_by_field_name("type")
                        .and_then(|t| php_model::type_ref(t, self.src, self.names))
                        .and_then(|t| self.concrete(t))
                        .map(|t| (t, "param"));
                    consider(param.start_byte(), typed);
                }
            }
        }
        let body = function.child_by_field_name("body")?;
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            if node.start_byte() >= position {
                continue;
            }
            if node != body && FUNCTION_LIKE.contains(&node.kind()) {
                continue; // a closure binds its own variables
            }
            match node.kind() {
                "assignment_expression" | "reference_assignment_expression"
                    if node.end_byte() <= position =>
                {
                    if let Some(left) = node.child_by_field_name("left") {
                        if left.kind() == "variable_name" && self.var_name(left) == var {
                            let typed = self
                                .documented_statement_type(node, var)
                                .or_else(|| self.type_of(node.child_by_field_name("right")?));
                            consider(node.start_byte(), typed);
                        } else if binds_in_list(left, var, self.src) {
                            consider(node.start_byte(), None);
                        }
                    }
                }
                "foreach_statement" => {
                    if foreach_binds(node, var, self.src) {
                        let typed = self.documented_statement_type(node, var);
                        consider(node.start_byte(), typed);
                    }
                }
                "if_statement" => {
                    if let Some(narrowed) = self.instanceof_narrowing(node, var, position) {
                        consider(node.start_byte(), Some((narrowed, "instanceof")));
                    }
                }
                "catch_clause" => {
                    let named = node
                        .child_by_field_name("name")
                        .is_some_and(|n| self.var_name(n) == var);
                    if named {
                        let typed = node.child_by_field_name("type").and_then(|types| {
                            let list = php_ast::named_children(types);
                            match list.as_slice() {
                                [only] => php_model::type_ref(*only, self.src, self.names)
                                    .and_then(|t| self.concrete(t))
                                    .map(|t| (t, "catch")),
                                _ => None,
                            }
                        });
                        consider(node.start_byte(), typed);
                    }
                }
                "comment" => {
                    let text = self.text(node);
                    if text.starts_with("/**") && text.contains(&format!("${var}")) {
                        if let Some(t) =
                            php_model::doc_var_type(text, Some(var), node.start_byte(), self.names)
                        {
                            if let Some(t) = self.concrete(t) {
                                consider(node.start_byte(), Some((t, "@var")));
                            }
                        }
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        latest.map(|(_, typed)| typed)
    }

    /// The class an `if` narrows `$var` to at `position`: inside `if ($var instanceof Foo) {…}`,
    /// or after a guard `if (!$var instanceof Foo) { return; }` that always leaves.
    fn instanceof_narrowing(&self, if_node: Node, var: &str, position: usize) -> Option<String> {
        let condition = if_node.child_by_field_name("condition")?;
        let mut expr = php_ast::named_children(condition).into_iter().next()?;
        let mut negated = false;
        loop {
            match expr.kind() {
                "parenthesized_expression" => {
                    expr = php_ast::named_children(expr).into_iter().next()?;
                }
                "unary_op_expression" if self.text(expr).trim_start().starts_with('!') => {
                    negated = !negated;
                    expr = php_ast::named_children(expr).into_iter().next()?;
                }
                _ => break,
            }
        }
        if expr.kind() != "binary_expression"
            || expr
                .child_by_field_name("operator")
                .is_none_or(|op| op.kind() != "instanceof")
        {
            return None;
        }
        let left = expr.child_by_field_name("left")?;
        if left.kind() != "variable_name" || self.var_name(left) != var {
            return None;
        }
        let right = expr.child_by_field_name("right")?;
        if !matches!(right.kind(), "name" | "qualified_name" | "relative_name") {
            return None;
        }
        let class = self.class_name_type(right)?;
        let body = if_node.child_by_field_name("body")?;
        let inside = body.start_byte() <= position && position < body.end_byte();
        let after_guard = negated && if_node.end_byte() <= position && always_leaves(body);
        ((!negated && inside) || after_guard).then_some(class)
    }

    /// `/** @var Foo $x */` right above the statement holding `node`.
    fn documented_statement_type(&self, node: Node, var: &str) -> Typed {
        let mut statement = node;
        while let Some(parent) = statement.parent() {
            if statement.kind().ends_with("_statement") {
                break;
            }
            statement = parent;
        }
        let doc = php_model::doc_before(statement, self.src)?;
        let t = php_model::doc_var_type(doc, Some(var), statement.start_byte(), self.names)?;
        self.concrete(t).map(|t| (t, "@var"))
    }

    fn concrete(&self, t: TypeRef) -> Option<String> {
        match t {
            TypeRef::Class(c) => Some(c),
            TypeRef::SelfType | TypeRef::Static => Some(self.class.clone()),
        }
    }

    fn var_name<'s>(&'s self, node: Node) -> &'s str {
        self.text(node).trim_start_matches('$')
    }

    /// Class constants and `Foo::class` among the arguments, not inside nested calls or closures.
    fn argument_constants(&self, arguments: Node) -> (Vec<ConstRef>, Vec<String>) {
        let mut constants = Vec::new();
        let mut classes = Vec::new();
        let mut stack: Vec<Node> = php_ast::named_children(arguments);
        while let Some(node) = stack.pop() {
            if CALL_KINDS.contains(&node.kind()) || FUNCTION_LIKE.contains(&node.kind()) {
                continue;
            }
            if node.kind() == "class_constant_access_expression" {
                let parts = php_ast::named_children(node);
                if let (Some(scope), Some(member)) = (parts.first(), parts.get(1)) {
                    let member = self.text(*member);
                    if let Some((owner, _)) = self.scope_type(*scope) {
                        if member.eq_ignore_ascii_case("class") {
                            classes.push(owner);
                        } else {
                            let written_scope = match scope.kind() {
                                "relative_scope" => php_ast::short_name(&owner).to_string(),
                                _ => self.text(*scope).trim_start_matches('\\').to_string(),
                            };
                            constants.push(ConstRef {
                                written: format!("{written_scope}::{member}"),
                                fqn: format!("{owner}::{member}"),
                            });
                        }
                    }
                }
                continue;
            }
            stack.extend(php_ast::named_children(node));
        }
        constants.reverse();
        classes.reverse();
        (constants, classes)
    }
}

/// A block whose last statement returns, throws, breaks or continues.
fn always_leaves(body: Node) -> bool {
    let last = if body.kind() == "compound_statement" {
        php_ast::named_children(body).into_iter().last()
    } else {
        Some(body)
    };
    last.is_some_and(|statement| match statement.kind() {
        "return_statement" | "break_statement" | "continue_statement" => true,
        "expression_statement" => php_ast::named_children(statement)
            .first()
            .is_some_and(|e| matches!(e.kind(), "throw_expression" | "exit_statement")),
        _ => false,
    })
}

fn enclosing_function(node: Node) -> Option<Node> {
    let mut current = node.parent();
    while let Some(n) = current {
        if FUNCTION_LIKE.contains(&n.kind()) {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

fn closure_captures(closure: Node, var: &str, src: &str) -> bool {
    php_ast::named_children(closure)
        .into_iter()
        .filter(|c| c.kind() == "anonymous_function_use_clause")
        .any(|clause| {
            let mut stack = vec![clause];
            while let Some(n) = stack.pop() {
                if n.kind() == "variable_name" && php_ast::text(n, src).trim_start_matches('$') == var {
                    return true;
                }
                stack.extend(php_ast::named_children(n));
            }
            false
        })
}

fn foreach_binds(foreach: Node, var: &str, src: &str) -> bool {
    // The iterated expression is the first named child; the bindings follow `as`.
    php_ast::named_children(foreach)
        .into_iter()
        .skip(1)
        .filter(|n| n.kind() != "compound_statement" && Some(*n) != foreach.child_by_field_name("body"))
        .any(|n| binds_in_list(n, var, src) || (n.kind() == "variable_name" && php_ast::text(n, src).trim_start_matches('$') == var))
}

/// `$var` appears as a target inside a destructuring or a `foreach` binding.
fn binds_in_list(node: Node, var: &str, src: &str) -> bool {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "variable_name" && php_ast::text(n, src).trim_start_matches('$') == var {
            return true;
        }
        if CALL_KINDS.contains(&n.kind()) {
            continue;
        }
        stack.extend(php_ast::named_children(n));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fixture(HashMap<String, Rc<ClassModel>>);

    impl Fixture {
        fn of(files: &[&str]) -> Fixture {
            let mut map = HashMap::new();
            for content in files {
                for model in php_model::class_models(content).unwrap() {
                    map.insert(model.fqn.to_ascii_lowercase(), Rc::new(model));
                }
            }
            Fixture(map)
        }
    }

    impl Classes for Fixture {
        fn class(&self, fqn: &str) -> Option<Rc<ClassModel>> {
            self.0.get(&fqn.to_ascii_lowercase()).cloned()
        }
    }

    const BRIDGE_BASE: &str = r#"<?php
namespace Share;
abstract class AbstractBridge
{
    /**
     * @template T of HandlerInterface
     * @param class-string<T> $handlerClass
     * @return T
     */
    protected static function getHandler(string $handlerClass): HandlerInterface {}
}
"#;

    const HANDLER: &str = r#"<?php
namespace Basket;
use Share\DispatcherInterface;
use Share\Enum\EntityType;
final readonly class ChangedHandler
{
    public function __construct(private DispatcherInterface $dispatcher) {}
    public function handle(Event $event): void
    {
        $this->dispatcher->dispatchPreferSync(
            $fuserId,
            EntityType::BASKET_SUMMARY,
            ['siteId' => $siteId],
        );
    }
}
"#;

    const DISPATCHER: &str = "<?php\nnamespace Share;\ninterface DispatcherInterface { public function dispatchPreferSync(int $id, $type, array $ctx = []): void; }\n";

    fn calls(fixture: &Fixture, content: &str, class: &str, method: &str) -> Vec<CallSite> {
        method_calls(content, class, method, class, fixture).unwrap().unwrap().calls
    }

    #[test]
    fn bridge_resolves_handler_by_var_doc_and_by_class_string_template() {
        let with_doc = r#"<?php
namespace Basket;
use Share\AbstractBridge;
final class Bridge extends AbstractBridge
{
    public static function onSaved(Event $event): void
    {
        /** @var ChangedHandler $handler */
        $handler = self::getHandler(ChangedHandler::class);
        $handler->handle($event);
    }
    public static function onDeleted(Event $event): void
    {
        $handler = self::getHandler(ChangedHandler::class);
        $handler->handle($event);
    }
}
"#;
        let fixture = Fixture::of(&[BRIDGE_BASE, HANDLER, with_doc]);
        let saved = calls(&fixture, with_doc, "Basket\\Bridge", "onSaved");
        assert_eq!(saved[0].method, "getHandler");
        assert_eq!(saved[0].receiver_type.as_deref(), Some("Basket\\Bridge"));
        assert_eq!(saved[0].classes, vec!["Basket\\ChangedHandler"]);
        assert_eq!(saved[1].method, "handle");
        assert_eq!(saved[1].receiver_type.as_deref(), Some("Basket\\ChangedHandler"));
        assert_eq!(saved[1].resolved_by, "@var");
        assert_eq!(saved[1].line, 10);

        let deleted = calls(&fixture, with_doc, "Basket\\Bridge", "onDeleted");
        assert_eq!(deleted[1].receiver_type.as_deref(), Some("Basket\\ChangedHandler"));
        assert_eq!(deleted[1].resolved_by, "template");
    }

    #[test]
    fn multiline_call_through_promoted_property_carries_its_constants() {
        let fixture = Fixture::of(&[HANDLER, DISPATCHER]);
        let sites = calls(&fixture, HANDLER, "Basket\\ChangedHandler", "handle");
        assert_eq!(sites.len(), 1);
        let call = &sites[0];
        assert_eq!(call.line, 10);
        assert_eq!(call.receiver_type.as_deref(), Some("Share\\DispatcherInterface"));
        assert_eq!(call.resolved_by, "property");
        assert_eq!(
            call.constants,
            vec![ConstRef {
                written: "EntityType::BASKET_SUMMARY".into(),
                fqn: "Share\\Enum\\EntityType::BASKET_SUMMARY".into()
            }]
        );
    }

    #[test]
    fn variable_types_follow_the_latest_binding() {
        let src = r#"<?php
namespace App;
class Svc
{
    public function run(Repo $repo, array $items): void
    {
        $repo->find(1);
        $made = new Maker();
        $made->make();
        foreach ($items as $made) {
            $made->unknown();
        }
        try {} catch (Failure $e) { $e->report(); }
        $fn = function () use ($repo) { $repo->inner(); };
        $arrow = fn() => $repo->arrow();
        $repo = $this->other();
        $repo->afterReassign();
    }
}
"#;
        let fixture = Fixture::of(&[src]);
        let sites = calls(&fixture, src, "App\\Svc", "run");
        let by_method = |m: &str| sites.iter().find(|s| s.method == m).unwrap();
        assert_eq!(by_method("find").resolved_by, "param");
        assert_eq!(by_method("make").receiver_type.as_deref(), Some("App\\Maker"));
        assert_eq!(by_method("unknown").receiver_type, None);
        assert_eq!(by_method("report").receiver_type.as_deref(), Some("App\\Failure"));
        assert_eq!(by_method("inner").receiver_type.as_deref(), Some("App\\Repo"));
        assert_eq!(by_method("arrow").receiver_type.as_deref(), Some("App\\Repo"));
        assert_eq!(by_method("afterReassign").receiver_type, None);
        assert_eq!(by_method("other").receiver_type.as_deref(), Some("App\\Svc"));
    }

    #[test]
    fn return_types_chain_and_static_returns_the_receiver() {
        let src = r#"<?php
namespace App;
class Builder
{
    public function with(): static { return $this; }
    public function repo(): Repo {}
    public function run(): void
    {
        $this->with()->repo()->save();
        (new Builder())->with()->done();
        parent::boot();
    }
}
"#;
        let fixture = Fixture::of(&[src]);
        let sites = calls(&fixture, src, "App\\Builder", "run");
        let by_method = |m: &str| sites.iter().find(|s| s.method == m).unwrap();
        assert_eq!(by_method("save").receiver_type.as_deref(), Some("App\\Repo"));
        assert_eq!(by_method("save").resolved_by, "return");
        assert_eq!(by_method("done").receiver_type.as_deref(), Some("App\\Builder"));
        assert_eq!(by_method("boot").receiver_type, None, "no parent declared");
        assert!(sites.iter().any(|s| s.kind == "new" && s.receiver == "Builder"));
    }

    #[test]
    fn a_trait_method_reads_this_as_the_class_using_the_trait() {
        let src = r#"<?php
namespace App;
trait Notifies
{
    public function notify(): void
    {
        $this->send();
        self::format();
    }
}
final class Mailer
{
    use Notifies;
    public function send(): void {}
    public static function format(): string { return ''; }
}
"#;
        let fixture = Fixture::of(&[src]);
        let sites = method_calls(src, "App\\Notifies", "notify", "App\\Mailer", &fixture)
            .unwrap()
            .unwrap()
            .calls;
        assert_eq!(sites[0].receiver_type.as_deref(), Some("App\\Mailer"));
        assert_eq!(sites[1].receiver_type.as_deref(), Some("App\\Mailer"));
        let (declaring, _) = find_method(&fixture, "App\\Mailer", "send").unwrap();
        assert_eq!(declaring.fqn, "App\\Mailer");
    }

    #[test]
    fn find_method_walks_traits_parents_and_interfaces() {
        let src = r#"<?php
namespace App;
trait Logs { public function log(): void {} }
interface Port { public function send(): void; }
abstract class Base implements Port { use Logs; public function boot(): void {} }
final class Impl extends Base {}
"#;
        let fixture = Fixture::of(&[src]);
        let (declaring, _) = find_method(&fixture, "App\\Impl", "log").unwrap();
        assert_eq!(declaring.fqn, "App\\Logs");
        let (declaring, _) = find_method(&fixture, "App\\Impl", "BOOT").unwrap();
        assert_eq!(declaring.fqn, "App\\Base");
        let (declaring, method) = find_method(&fixture, "App\\Impl", "send").unwrap();
        assert_eq!(declaring.fqn, "App\\Port");
        assert!(method.is_abstract);
    }

    #[test]
    fn instanceof_narrows_inside_the_if_and_after_a_guard_that_leaves() {
        let src = r#"<?php
namespace App;
class Svc
{
    public function run(Event $event): void
    {
        $entity = $event->getParameter('ENTITY');
        if (!$entity instanceof Item) {
            return;
        }
        $entity->guarded();
        $other = $event->getParameter('OTHER');
        if ($other instanceof Item) {
            $other->inside();
        }
        $other->outside();
        $third = $event->getParameter('THIRD');
        if (!$third instanceof Item) {
            $this->log();
        }
        $third->notGuarded();
    }
}
"#;
        let fixture = Fixture::of(&[src]);
        let sites = calls(&fixture, src, "App\\Svc", "run");
        let by_method = |m: &str| sites.iter().find(|s| s.method == m).unwrap();
        assert_eq!(by_method("guarded").receiver_type.as_deref(), Some("App\\Item"));
        assert_eq!(by_method("guarded").resolved_by, "instanceof");
        assert_eq!(by_method("inside").receiver_type.as_deref(), Some("App\\Item"));
        assert_eq!(by_method("outside").receiver_type, None);
        assert_eq!(by_method("notGuarded").receiver_type, None);
    }

    #[test]
    fn builtin_functions_are_skipped_project_functions_kept() {
        let src = "<?php\nclass A { public function f() { count([]); \\sprintf('%s', 1); isset($a); plural_form(1); \\App\\helper(); } }\n";
        let fixture = Fixture::of(&[src]);
        let sites = calls(&fixture, src, "A", "f");
        let names: Vec<&str> = sites.iter().map(|s| s.method.as_str()).collect();
        assert_eq!(names, vec!["plural_form", "App\\helper"]);
    }
}
