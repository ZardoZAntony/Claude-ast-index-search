//! What a PHP class declares, read from its syntax tree: parent, interfaces, traits, property
//! types, methods with their return types, constants and enum cases. Enough to follow a call
//! from a receiver's type to the method it lands in.

use std::collections::HashMap;
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use tree_sitter::Node;

use super::php_ast::{self, Names};

/// A type a declaration names: a class, or the class it is read on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRef {
    /// FQN without a leading backslash.
    Class(String),
    /// `self`: the declaring class.
    SelfType,
    /// `static`, `$this`: the class of the object the member is read on.
    Static,
}

#[derive(Debug, Clone)]
pub struct MethodModel {
    pub name: String,
    pub line: usize,
    pub end_line: usize,
    pub visibility: &'static str,
    pub is_static: bool,
    /// Abstract, or declared in an interface: no body to follow.
    pub is_abstract: bool,
    pub returns: Option<TypeRef>,
    /// `@template T` + `@param class-string<T> $x` + `@return T`: the call returns an instance
    /// of the class passed at this argument position.
    pub returns_class_of_param: Option<usize>,
    pub params: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ConstantModel {
    pub name: String,
    pub line: usize,
    /// The value expression as written.
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct ClassModel {
    pub fqn: String,
    /// `class`, `interface`, `trait` or `enum`.
    pub kind: &'static str,
    pub is_abstract: bool,
    pub line: usize,
    pub end_line: usize,
    pub parent: Option<String>,
    pub interfaces: Vec<String>,
    pub traits: Vec<String>,
    /// Property name without `$` → declared type.
    pub properties: HashMap<String, TypeRef>,
    /// Lowercased method name → method.
    pub methods: HashMap<String, MethodModel>,
    pub constants: Vec<ConstantModel>,
    /// Enum cases with their value expressions (empty for a pure enum case).
    pub cases: Vec<ConstantModel>,
}

impl ClassModel {
    pub fn method(&self, name: &str) -> Option<&MethodModel> {
        self.methods.get(&name.to_ascii_lowercase())
    }
}

/// Every class-like declaration of a PHP file.
pub fn class_models(content: &str) -> Result<Vec<ClassModel>> {
    let tree = php_ast::parse(content)?;
    let root = tree.root_node();
    let names = Names::of(root, content);
    Ok(php_ast::class_like_nodes(root)
        .into_iter()
        .filter_map(|node| class_model(node, content, &names))
        .collect())
}

/// The declaration of class-like `fqn` in a parsed file.
pub fn find_class_node<'t>(root: Node<'t>, src: &str, names: &Names, fqn: &str) -> Option<Node<'t>> {
    php_ast::class_like_nodes(root)
        .into_iter()
        .find(|n| declared_fqn(*n, src, names).is_some_and(|f| f.eq_ignore_ascii_case(fqn)))
}

/// FQN of the class-like declaration a node is written in.
pub fn enclosing_class(node: Node, src: &str, names: &Names) -> Option<String> {
    let mut current = node.parent();
    while let Some(n) = current {
        if php_ast::CLASS_LIKE_KINDS.contains(&n.kind()) {
            return declared_fqn(n, src, names);
        }
        current = n.parent();
    }
    None
}

/// FQN of the class-like declaration `node`.
pub fn declared_fqn(node: Node, src: &str, names: &Names) -> Option<String> {
    let name = php_ast::text(node.child_by_field_name("name")?, src);
    let namespace = names.namespace_at(node.start_byte());
    Some(if namespace.is_empty() {
        name.to_string()
    } else {
        format!("{namespace}\\{name}")
    })
}

fn class_model(node: Node, src: &str, names: &Names) -> Option<ClassModel> {
    let fqn = declared_fqn(node, src, names)?;
    let kind = match node.kind() {
        "interface_declaration" => "interface",
        "trait_declaration" => "trait",
        "enum_declaration" => "enum",
        _ => "class",
    };
    let mut model = ClassModel {
        fqn,
        kind,
        is_abstract: php_ast::has_child_kind(node, "abstract_modifier"),
        line: php_ast::line(node),
        end_line: php_ast::end_line(node),
        parent: None,
        interfaces: Vec::new(),
        traits: Vec::new(),
        properties: HashMap::new(),
        methods: HashMap::new(),
        constants: Vec::new(),
        cases: Vec::new(),
    };
    let class_names = |clause: Node| -> Vec<String> {
        php_ast::named_children(clause)
            .into_iter()
            .filter(|n| matches!(n.kind(), "name" | "qualified_name" | "relative_name"))
            .map(|n| names.resolve_class(php_ast::text(n, src), n.start_byte()))
            .collect()
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // An interface's `extends` lists interfaces.
            "base_clause" if kind == "interface" => model.interfaces.extend(class_names(child)),
            "base_clause" => model.parent = class_names(child).into_iter().next(),
            "class_interface_clause" => model.interfaces.extend(class_names(child)),
            _ => {}
        }
    }
    let Some(body) = node.child_by_field_name("body") else {
        return Some(model);
    };
    for member in php_ast::named_children(body).iter() {
        let doc = doc_before(*member, src);
        match member.kind() {
            "use_declaration" => model.traits.extend(class_names(*member)),
            "property_declaration" => {
                let declared = member
                    .child_by_field_name("type")
                    .and_then(|t| type_ref(t, src, names));
                let documented = doc.and_then(|d| doc_var_type(d, None, member.start_byte(), names));
                let mut inner = member.walk();
                for element in member.children(&mut inner) {
                    if element.kind() != "property_element" {
                        continue;
                    }
                    let Some(var) = php_ast::named_children(element)
                        .into_iter()
                        .find(|n| n.kind() == "variable_name")
                    else {
                        continue;
                    };
                    let name = php_ast::text(var, src).trim_start_matches('$').to_string();
                    if let Some(t) = declared.clone().or_else(|| documented.clone()) {
                        model.properties.insert(name, t);
                    }
                }
            }
            "method_declaration" => {
                let method = method_model(*member, doc, src, names, kind == "interface");
                if method.name.eq_ignore_ascii_case("__construct") {
                    for (name, t) in promoted_properties(*member, src, names) {
                        model.properties.insert(name, t);
                    }
                }
                model.methods.insert(method.name.to_ascii_lowercase(), method);
            }
            "const_declaration" => {
                let mut inner = member.walk();
                for element in member.children(&mut inner) {
                    if element.kind() != "const_element" {
                        continue;
                    }
                    let parts = php_ast::named_children(element);
                    let (Some(name), Some(value)) = (parts.first(), parts.get(1)) else {
                        continue;
                    };
                    model.constants.push(ConstantModel {
                        name: php_ast::text(*name, src).to_string(),
                        line: php_ast::line(element),
                        value: php_ast::render(*value, src, None),
                    });
                }
            }
            "enum_case" => {
                if let Some(name) = member.child_by_field_name("name") {
                    model.cases.push(ConstantModel {
                        name: php_ast::text(name, src).to_string(),
                        line: php_ast::line(*member),
                        value: member
                            .child_by_field_name("value")
                            .map(|v| php_ast::render(v, src, None))
                            .unwrap_or_default(),
                    });
                }
            }
            _ => {}
        }
    }
    Some(model)
}

/// The docblock right above `node`, if any.
pub fn doc_before<'s>(node: Node, src: &'s str) -> Option<&'s str> {
    let prev = node.prev_sibling()?;
    if prev.kind() != "comment" {
        return None;
    }
    let text = php_ast::text(prev, src);
    text.starts_with("/**").then_some(text)
}

fn method_model(
    node: Node,
    doc: Option<&str>,
    src: &str,
    names: &Names,
    in_interface: bool,
) -> MethodModel {
    let name = node
        .child_by_field_name("name")
        .map(|n| php_ast::text(n, src).to_string())
        .unwrap_or_default();
    let visibility = {
        let mut cursor = node.walk();
        let found = node
            .children(&mut cursor)
            .find(|c| c.kind() == "visibility_modifier")
            .map(|c| php_ast::text(c, src).to_ascii_lowercase());
        match found.as_deref() {
            Some("private") => "private",
            Some("protected") => "protected",
            _ => "public",
        }
    };
    let params: Vec<String> = node
        .child_by_field_name("parameters")
        .map(|list| {
            php_ast::named_children(list)
                .into_iter()
                .filter_map(|p| p.child_by_field_name("name"))
                .map(|n| php_ast::text(n, src).trim_start_matches('$').to_string())
                .collect()
        })
        .unwrap_or_default();
    let native = node
        .child_by_field_name("return_type")
        .and_then(|t| type_ref(t, src, names));
    let tags = doc.map(DocTags::parse).unwrap_or_default();
    let returns_class_of_param = tags.class_string_return().and_then(|param| {
        params.iter().position(|p| p == &param)
    });
    let returns = native.or_else(|| {
        tags.returns
            .as_deref()
            .and_then(|t| doc_type(t, node.start_byte(), names))
    });
    MethodModel {
        line: php_ast::line(node),
        end_line: php_ast::end_line(node),
        visibility,
        is_static: php_ast::has_child_kind(node, "static_modifier"),
        is_abstract: in_interface
            || php_ast::has_child_kind(node, "abstract_modifier")
            || node.child_by_field_name("body").is_none(),
        returns,
        returns_class_of_param,
        params,
        name,
    }
}

fn promoted_properties(ctor: Node, src: &str, names: &Names) -> Vec<(String, TypeRef)> {
    let Some(list) = ctor.child_by_field_name("parameters") else {
        return Vec::new();
    };
    php_ast::named_children(list)
        .into_iter()
        .filter(|p| p.kind() == "property_promotion_parameter")
        .filter_map(|p| {
            let name = php_ast::text(p.child_by_field_name("name")?, src)
                .trim_start_matches('$')
                .to_string();
            let t = type_ref(p.child_by_field_name("type")?, src, names)?;
            Some((name, t))
        })
        .collect()
}

/// The class a type node names: `Foo`, `?Foo`, `Foo|null`, `static`. Built-in types, unions of
/// several classes and intersections name no single class.
pub fn type_ref(node: Node, src: &str, names: &Names) -> Option<TypeRef> {
    match node.kind() {
        "named_type" => {
            let name = php_ast::named_children(node).into_iter().next()?;
            let written = php_ast::text(name, src);
            match written.to_ascii_lowercase().as_str() {
                "self" => Some(TypeRef::SelfType),
                "static" => Some(TypeRef::Static),
                _ if php_ast::is_special_type(written) => None,
                _ => Some(TypeRef::Class(
                    names.resolve_class(written, name.start_byte()),
                )),
            }
        }
        "optional_type" => type_ref(php_ast::named_children(node).into_iter().next()?, src, names),
        "union_type" => {
            let classes: Vec<TypeRef> = php_ast::named_children(node)
                .into_iter()
                .filter_map(|t| type_ref(t, src, names))
                .collect();
            match classes.as_slice() {
                [only] => Some(only.clone()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The class a PHPDoc type names: `Foo`, `?Foo`, `Foo|null`, `static`, `$this`; generics are
/// dropped (`Collection<Item>` names `Collection`), arrays name none.
pub fn doc_type(written: &str, at: usize, names: &Names) -> Option<TypeRef> {
    let written = written.trim().trim_start_matches('?');
    let mut classes = written
        .split('|')
        .map(str::trim)
        .filter(|t| !t.eq_ignore_ascii_case("null") && !t.is_empty());
    let only = classes.next()?;
    if classes.next().is_some() || only.ends_with("[]") {
        return None;
    }
    let base = only.split('<').next().unwrap_or(only).trim();
    match base.to_ascii_lowercase().as_str() {
        "self" => Some(TypeRef::SelfType),
        "static" | "$this" => Some(TypeRef::Static),
        _ if php_ast::is_special_type(base) => None,
        _ if !base
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '\\') =>
        {
            None
        }
        _ => Some(TypeRef::Class(names.resolve_class(base, at))),
    }
}

/// `@var Type $name` (or `@var Type` when `var` is `None` or the tag names no variable).
pub fn doc_var_type(doc: &str, var: Option<&str>, at: usize, names: &Names) -> Option<TypeRef> {
    DOC_VAR.captures_iter(doc).find_map(|c| {
        let named = c.get(2).map(|m| m.as_str());
        let fits = match (var, named) {
            (Some(var), Some(named)) => var == named,
            _ => true,
        };
        if fits {
            doc_type(&c[1], at, names)
        } else {
            None
        }
    })
}

static DOC_VAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"@(?:phpstan-|psalm-)?var\s+([^\s*]+)(?:\s+\$(\w+))?").expect("valid regex")
});
static DOC_RETURN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"@(?:phpstan-|psalm-)?return\s+([^\s*]+)").expect("valid regex")
});
static DOC_TEMPLATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"@(?:phpstan-|psalm-)?template\s+(\w+)").expect("valid regex")
});
static DOC_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"@(?:phpstan-|psalm-)?param\s+([^\s*]+)\s+(?:\.\.\.)?\$(\w+)").expect("valid regex")
});

#[derive(Default)]
struct DocTags {
    returns: Option<String>,
    templates: Vec<String>,
    params: Vec<(String, String)>,
}

impl DocTags {
    fn parse(doc: &str) -> DocTags {
        DocTags {
            returns: DOC_RETURN.captures(doc).map(|c| c[1].to_string()),
            templates: DOC_TEMPLATE
                .captures_iter(doc)
                .map(|c| c[1].to_string())
                .collect(),
            params: DOC_PARAM
                .captures_iter(doc)
                .map(|c| (c[1].to_string(), c[2].to_string()))
                .collect(),
        }
    }

    /// The parameter whose `class-string<T>` the method returns an instance of.
    fn class_string_return(&self) -> Option<String> {
        let returns = self.returns.as_deref()?.trim_start_matches('?');
        let returns = returns
            .split('|')
            .find(|t| !t.eq_ignore_ascii_case("null"))?;
        if !self.templates.iter().any(|t| t == returns) {
            return None;
        }
        let wanted = format!("class-string<{returns}>");
        self.params
            .iter()
            .find(|(t, _)| t.replace(' ', "") == wanted)
            .map(|(_, name)| name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(src: &str, fqn: &str) -> ClassModel {
        class_models(src)
            .unwrap()
            .into_iter()
            .find(|c| c.fqn == fqn)
            .unwrap()
    }

    #[test]
    fn reads_parent_interfaces_traits_and_property_types() {
        let src = r#"<?php
namespace App;
use Lib\Dispatcher;
use Lib\Repo as R;
final class Handler extends Base implements One, \Two
{
    use Helpers;
    /** @var Logger */
    private $logger;
    private ?R $repo = null;
    public function __construct(private Dispatcher $dispatcher, int $plain) {}
}
"#;
        let m = model(src, "App\\Handler");
        assert_eq!(m.parent.as_deref(), Some("App\\Base"));
        assert_eq!(m.interfaces, vec!["App\\One", "Two"]);
        assert_eq!(m.traits, vec!["App\\Helpers"]);
        assert_eq!(m.properties["dispatcher"], TypeRef::Class("Lib\\Dispatcher".into()));
        assert_eq!(m.properties["repo"], TypeRef::Class("Lib\\Repo".into()));
        assert_eq!(m.properties["logger"], TypeRef::Class("App\\Logger".into()));
        assert!(!m.properties.contains_key("plain"));
    }

    #[test]
    fn reads_return_types_and_class_string_templates() {
        let src = r#"<?php
namespace App;
abstract class Bridge
{
    /**
     * @template T of HandlerInterface
     * @param class-string<T> $handlerClass
     * @return T
     */
    protected static function getHandler(string $handlerClass): HandlerInterface {}
    public function self(): static {}
    /** @return Collection<Item> */
    public function items() {}
    abstract public function run(): void;
}
"#;
        let m = model(src, "App\\Bridge");
        let get = m.method("GETHANDLER").unwrap();
        assert_eq!(get.returns_class_of_param, Some(0));
        assert_eq!(get.returns, Some(TypeRef::Class("App\\HandlerInterface".into())));
        assert_eq!(m.method("self").unwrap().returns, Some(TypeRef::Static));
        assert_eq!(
            m.method("items").unwrap().returns,
            Some(TypeRef::Class("App\\Collection".into()))
        );
        assert!(m.method("run").unwrap().is_abstract);
        assert!(!get.is_abstract);
    }

    #[test]
    fn reads_enum_cases_and_constants() {
        let src = "<?php\nenum E: string {\n    public const int TTL = 60 * 60;\n    case A = 'a';\n    case B = 'b';\n}\n";
        let m = model(src, "E");
        assert_eq!(m.kind, "enum");
        assert_eq!(m.cases.len(), 2);
        assert_eq!(m.cases[1].name, "B");
        assert_eq!(m.cases[1].value, "'b'");
        assert_eq!(m.constants[0].value, "60 * 60");
    }
}
