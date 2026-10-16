//! PHP read through its syntax tree: the namespace and imports a position resolves class names
//! against, and a rendering of a node that formatting, comments and attributes do not change.
//!
//! The indexer keeps PHP facts per line (see `php_names`); the commands that compare signatures
//! or follow calls need the structure itself — a parameter list spread over lines with a phpDoc
//! between parameters, an attribute holding brackets, a default value `new Foo()`.

use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{anyhow, Result};
use tree_sitter::{Node, Parser, Tree};

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

pub fn parse(content: &str) -> Result<Tree> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        let parser = match slot.as_mut() {
            Some(parser) => parser,
            None => {
                let mut parser = Parser::new();
                parser
                    .set_language(&super::treesitter::php::PHP_LANGUAGE)
                    .map_err(|e| anyhow!("Failed to set PHP language: {e}"))?;
                slot.insert(parser)
            }
        };
        parser
            .parse(content, None)
            .ok_or_else(|| anyhow!("tree-sitter parse returned None"))
    })
}

pub fn text<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

pub fn line(node: Node) -> usize {
    node.start_position().row + 1
}

pub fn end_line(node: Node) -> usize {
    node.end_position().row + 1
}

/// Named children of `node`, comments left out.
pub fn named_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

pub fn has_child_kind(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    // Bound before returning: the iterator borrows `cursor`, which a tail expression outlives.
    let found = node.children(&mut cursor).any(|child| child.kind() == kind);
    found
}

pub const CLASS_LIKE_KINDS: [&str; 4] = [
    "class_declaration",
    "interface_declaration",
    "trait_declaration",
    "enum_declaration",
];

/// Every named declaration of a class, interface, trait or enum in the file, nested ones too.
pub fn class_like_nodes(root: Node) -> Vec<Node> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if CLASS_LIKE_KINDS.contains(&node.kind()) {
            found.push(node);
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    found
}

/// The namespace and class imports in force in each part of a file.
pub struct Names {
    scopes: Vec<Scope>,
}

struct Scope {
    start: usize,
    end: usize,
    namespace: String,
    /// Lowercased alias → FQN without a leading backslash.
    classes: HashMap<String, String>,
}

impl Names {
    pub fn of(root: Node, src: &str) -> Names {
        let mut scopes: Vec<Scope> = Vec::new();
        let global = |start, end| Scope {
            start,
            end,
            namespace: String::new(),
            classes: HashMap::new(),
        };
        let mut current = global(0, root.end_byte());
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            match child.kind() {
                "namespace_definition" => {
                    let namespace = child
                        .child_by_field_name("name")
                        .map(|n| text(n, src).trim_start_matches('\\').to_string())
                        .unwrap_or_default();
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut braced = Scope {
                            start: child.start_byte(),
                            end: child.end_byte(),
                            namespace,
                            classes: HashMap::new(),
                        };
                        let mut inner = body.walk();
                        for statement in body.children(&mut inner) {
                            if statement.kind() == "namespace_use_declaration" {
                                add_imports(&mut braced.classes, statement, src);
                            }
                        }
                        scopes.push(braced);
                    } else {
                        current.end = child.start_byte();
                        scopes.push(current);
                        current = Scope {
                            start: child.start_byte(),
                            end: root.end_byte(),
                            namespace,
                            classes: HashMap::new(),
                        };
                    }
                }
                "namespace_use_declaration" => add_imports(&mut current.classes, child, src),
                _ => {}
            }
        }
        scopes.push(current);
        scopes.sort_by_key(|s| s.start);
        Names { scopes }
    }

    fn scope_at(&self, byte: usize) -> Option<&Scope> {
        // A braced namespace lies inside the unbraced scope that precedes it: the innermost wins.
        self.scopes
            .iter()
            .filter(|s| s.start <= byte && byte < s.end.max(s.start + 1))
            .min_by_key(|s| s.end - s.start)
    }

    pub fn namespace_at(&self, byte: usize) -> &str {
        self.scope_at(byte).map(|s| s.namespace.as_str()).unwrap_or("")
    }

    /// FQN, without a leading backslash, of the class `name` written at `byte`. `self`, `static`,
    /// `parent` and built-in type names come back as written.
    pub fn resolve_class(&self, name: &str, byte: usize) -> String {
        let name = name.trim();
        if let Some(stripped) = name.strip_prefix('\\') {
            return stripped.to_string();
        }
        if is_special_type(name) {
            return name.to_string();
        }
        let namespace = self.namespace_at(byte);
        if let Some(rest) = name.strip_prefix("namespace\\") {
            return qualify(namespace, rest);
        }
        let (first, rest) = match name.split_once('\\') {
            Some((first, rest)) => (first, Some(rest)),
            None => (name, None),
        };
        if let Some(fqn) = self
            .scope_at(byte)
            .and_then(|s| s.classes.get(&first.to_ascii_lowercase()))
        {
            return match rest {
                Some(rest) => format!("{fqn}\\{rest}"),
                None => fqn.clone(),
            };
        }
        qualify(namespace, name)
    }
}

fn qualify(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        name.to_string()
    } else {
        format!("{namespace}\\{name}")
    }
}

/// Names a type position may hold that are not classes.
pub fn is_special_type(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "self"
            | "static"
            | "parent"
            | "array"
            | "callable"
            | "iterable"
            | "bool"
            | "int"
            | "float"
            | "string"
            | "void"
            | "null"
            | "mixed"
            | "never"
            | "object"
            | "false"
            | "true"
    )
}

fn add_imports(classes: &mut HashMap<String, String>, declaration: Node, src: &str) {
    if is_function_or_const_use(declaration) {
        return;
    }
    let mut prefix = String::new();
    let mut cursor = declaration.walk();
    for child in declaration.children(&mut cursor) {
        match child.kind() {
            "namespace_name" => prefix = text(child, src).trim_start_matches('\\').to_string(),
            "namespace_use_clause" => add_clause(classes, child, "", src),
            "namespace_use_group" => {
                let mut inner = child.walk();
                for clause in child.children(&mut inner) {
                    if clause.kind() == "namespace_use_clause" {
                        add_clause(classes, clause, &prefix, src);
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_function_or_const_use(node: Node) -> bool {
    has_child_kind(node, "function") || has_child_kind(node, "const")
}

fn add_clause(classes: &mut HashMap<String, String>, clause: Node, prefix: &str, src: &str) {
    if is_function_or_const_use(clause) {
        return;
    }
    let Some(target) = named_children(clause)
        .into_iter()
        .find(|n| matches!(n.kind(), "qualified_name" | "name"))
    else {
        return;
    };
    let path = text(target, src).trim_start_matches('\\');
    let fqn = if prefix.is_empty() {
        path.to_string()
    } else {
        format!("{prefix}\\{path}")
    };
    let alias = clause
        .child_by_field_name("alias")
        .map(|a| text(a, src).to_string())
        .unwrap_or_else(|| short_name(&fqn).to_string());
    classes.insert(alias.to_ascii_lowercase(), fqn);
}

pub fn short_name(fqn: &str) -> &str {
    fqn.rsplit('\\').next().unwrap_or(fqn)
}

/// Kinds whose whole text is one token: their inside is content, not syntax.
const ATOMIC_KINDS: [&str; 6] = [
    "string",
    "encapsed_string",
    "heredoc",
    "nowdoc",
    "integer",
    "float",
];

/// `node` token by token with canonical spacing: line breaks, indentation, comments and
/// attributes do not change it. With `names`, class names are replaced by their FQN, so two
/// spellings of one class (an alias, a leading backslash) render alike.
pub fn render(node: Node, src: &str, names: Option<&Names>) -> String {
    let mut tokens = Vec::new();
    collect_tokens(node, src, names, &mut tokens);
    join_tokens(&tokens)
}

/// A type: written without spaces (`?int`, `A|B`, `(A&B)|null`).
pub fn render_type(node: Node, src: &str, names: Option<&Names>) -> String {
    let mut tokens = Vec::new();
    collect_tokens(node, src, names, &mut tokens);
    tokens.concat()
}

fn collect_tokens(node: Node, src: &str, names: Option<&Names>, out: &mut Vec<String>) {
    match node.kind() {
        "comment" | "attribute_list" => return,
        kind if ATOMIC_KINDS.contains(&kind) => {
            out.push(text(node, src).to_string());
            return;
        }
        _ => {}
    }
    if let Some(names) = names {
        if is_class_name_position(node) {
            let written = text(node, src);
            let resolved = names.resolve_class(written, node.start_byte());
            out.push(if is_special_type(written) {
                written.to_string()
            } else {
                format!("\\{resolved}")
            });
            return;
        }
    }
    if matches!(
        node.kind(),
        "qualified_name" | "namespace_name" | "relative_name"
    ) {
        out.push(text(node, src).split_whitespace().collect());
        return;
    }
    if node.child_count() == 0 {
        let token = text(node, src);
        if !token.is_empty() {
            out.push(token.to_string());
        }
        return;
    }
    // `name: value`: a named argument's label sticks to its colon, unlike a ternary's.
    let label = (node.kind() == "argument")
        .then(|| node.child_by_field_name("name"))
        .flatten();
    if let Some(label) = label {
        out.push(format!("{}:", text(label, src)));
    }
    let start = out.len();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if label.is_some() && (Some(child) == label || child.kind() == ":") {
            continue;
        }
        collect_tokens(child, src, names, out);
    }
    // `-1`, `!$x`: a unary operator sticks to its operand.
    if node.kind() == "unary_op_expression" && out.len() > start + 1 {
        let operand = out.remove(start + 1);
        out[start].push_str(&operand);
    }
}

/// A name in a position that holds a class: a type, `extends`/`implements`, `new X`, `X::…`.
pub fn is_class_name_position(node: Node) -> bool {
    if !matches!(node.kind(), "name" | "qualified_name" | "relative_name") {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "named_type" | "base_clause" | "class_interface_clause" => true,
        "object_creation_expression" => true,
        "class_constant_access_expression"
        | "scoped_call_expression"
        | "scoped_property_access_expression" => parent.named_child(0) == Some(node),
        "binary_expression" => {
            // `$x instanceof Foo`
            parent.child_by_field_name("right") == Some(node)
                && parent
                    .child_by_field_name("operator")
                    .is_some_and(|op| op.kind() == "instanceof")
        }
        _ => false,
    }
}

fn join_tokens(tokens: &[String]) -> String {
    let mut out = String::new();
    for (i, token) in tokens.iter().enumerate() {
        if i > 0 && needs_space(&tokens[i - 1], token) {
            out.push(' ');
        }
        out.push_str(token);
    }
    out
}

fn needs_space(prev: &str, next: &str) -> bool {
    const TIGHT_AFTER: [&str; 9] = ["(", "[", "::", "->", "?->", "$", "...", "#[", "&"];
    const TIGHT_BEFORE: [&str; 7] = [")", "]", ",", ";", "::", "->", "?->"];
    if TIGHT_AFTER.contains(&prev) || TIGHT_BEFORE.contains(&next) {
        return false;
    }
    // A call or an index sticks to what it applies to: `new Foo()`, `$a[0]`; `= [` does not.
    if next == "(" || next == "[" {
        return !prev
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | ')' | ']'));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_of_kind<'t>(root: Node<'t>, kind: &str) -> Option<Node<'t>> {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == kind {
                return Some(node);
            }
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        None
    }

    #[test]
    fn resolves_imports_aliases_groups_and_namespace() {
        let src = "<?php\nnamespace App\\Order;\nuse Foo\\Bar;\nuse Foo\\Baz as Qux;\nuse Lib\\{One, Two as Deux};\nuse function Foo\\helper;\n";
        let tree = parse(src).unwrap();
        let names = Names::of(tree.root_node(), src);
        let at = src.len() - 1;
        assert_eq!(names.resolve_class("Bar", at), "Foo\\Bar");
        assert_eq!(names.resolve_class("Qux", at), "Foo\\Baz");
        assert_eq!(names.resolve_class("Deux", at), "Lib\\Two");
        assert_eq!(names.resolve_class("One\\Inner", at), "Lib\\One\\Inner");
        assert_eq!(names.resolve_class("Local", at), "App\\Order\\Local");
        assert_eq!(names.resolve_class("\\Root", at), "Root");
        assert_eq!(names.resolve_class("helper", at), "App\\Order\\helper");
        assert_eq!(names.resolve_class("self", at), "self");
    }

    #[test]
    fn braced_namespaces_keep_their_own_imports() {
        let src = "<?php\nnamespace A { use X\\Foo; }\nnamespace B { use Y\\Foo; class C {} }\n";
        let tree = parse(src).unwrap();
        let names = Names::of(tree.root_node(), src);
        let class = first_of_kind(tree.root_node(), "class_declaration").unwrap();
        assert_eq!(names.namespace_at(class.start_byte()), "B");
        assert_eq!(names.resolve_class("Foo", class.start_byte()), "Y\\Foo");
    }

    #[test]
    fn rendering_ignores_layout_comments_and_attributes() {
        let a = "<?php\nfunction f(#[Assert\\Choice([1, 2])] int $a, Foo $foo = new Foo(), array $x = ['k' => -1]) {}\n";
        let b = "<?php\nfunction f(\n    /** doc */\n    int $a,\n    Foo $foo = new Foo( ),\n    array $x = [ 'k'=>-1 ],\n) {}\n";
        let render_params = |src: &str| {
            let tree = parse(src).unwrap();
            let params = first_of_kind(tree.root_node(), "formal_parameters").unwrap();
            named_children(params)
                .into_iter()
                .map(|p| render(p, src, None))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            render_params(a),
            vec!["int $a", "Foo $foo = new Foo()", "array $x = ['k' => -1]"]
        );
        assert_eq!(render_params(a), render_params(b));
    }

    #[test]
    fn rendering_with_names_compares_classes_not_spellings() {
        let a = "<?php\nnamespace N;\nuse Lib\\Repo;\nfunction f(?Repo $r) {}\n";
        let b = "<?php\nnamespace N;\nuse Lib\\Repo as R;\nfunction f(?R $r) {}\n";
        let render_type_of = |src: &str| {
            let tree = parse(src).unwrap();
            let names = Names::of(tree.root_node(), src);
            let param = first_of_kind(tree.root_node(), "simple_parameter").unwrap();
            render_type(param.child_by_field_name("type").unwrap(), src, Some(&names))
        };
        assert_eq!(render_type_of(a), "?\\Lib\\Repo");
        assert_eq!(render_type_of(a), render_type_of(b));
    }
}
