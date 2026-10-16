//! The public API of PHP classes and how it changed between two versions of a file:
//! methods with their signatures, the constructor, the class header.
//!
//! Signatures are rendered from the syntax tree (see `php_ast::render`), so a parameter list
//! re-wrapped, a phpDoc added between parameters or an attribute with brackets changes nothing,
//! while a new parameter, another type or another default value does. Class names are compared
//! by FQN: switching `use Lib\Repo` to an alias is no change, pointing it at another class is.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use anyhow::Result;
use serde::Serialize;
use tree_sitter::Node;

use super::php_ast::{self, Names};

#[derive(Debug, Clone)]
pub struct ClassApi {
    pub fqn: String,
    /// `class`, `interface`, `trait` or `enum`.
    pub kind: &'static str,
    pub is_abstract: bool,
    pub line: usize,
    /// Modifiers, name, `extends` and `implements` as written.
    pub header: String,
    /// The same with class names resolved and interfaces in a fixed order.
    pub header_canonical: String,
    pub methods: Vec<MethodApi>,
}

#[derive(Debug, Clone)]
pub struct MethodApi {
    pub name: String,
    pub visibility: &'static str,
    pub is_static: bool,
    pub is_abstract: bool,
    pub is_final: bool,
    /// `name(params): type` as written, modifiers left out.
    pub signature: String,
    pub canonical: String,
    pub params: Vec<ParamApi>,
    body: u64,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct ParamApi {
    /// `$name`.
    pub name: String,
    pub text: String,
    pub canonical: String,
    pub required: bool,
}

/// Class-like declarations of a PHP file, and whether the parser met a syntax error.
pub fn class_apis(content: &str) -> Result<(Vec<ClassApi>, bool)> {
    let tree = php_ast::parse(content)?;
    let root = tree.root_node();
    let names = Names::of(root, content);
    let classes = php_ast::class_like_nodes(root)
        .into_iter()
        .filter_map(|node| class_api(node, content, &names))
        .collect();
    Ok((classes, root.has_error()))
}

fn class_api(node: Node, src: &str, names: &Names) -> Option<ClassApi> {
    let name = php_ast::text(node.child_by_field_name("name")?, src);
    let namespace = names.namespace_at(node.start_byte());
    let fqn = if namespace.is_empty() {
        name.to_string()
    } else {
        format!("{namespace}\\{name}")
    };
    let kind = match node.kind() {
        "interface_declaration" => "interface",
        "trait_declaration" => "trait",
        "enum_declaration" => "enum",
        _ => "class",
    };
    let body = node.child_by_field_name("body");
    let methods = body
        .map(|body| {
            php_ast::named_children(body)
                .into_iter()
                .filter(|n| n.kind() == "method_declaration")
                .filter_map(|n| method_api(n, src, names))
                .collect()
        })
        .unwrap_or_default();
    Some(ClassApi {
        fqn,
        kind,
        is_abstract: php_ast::has_child_kind(node, "abstract_modifier"),
        line: php_ast::line(node),
        header: header(node, body, src, None),
        header_canonical: header(node, body, src, Some(names)),
        methods,
    })
}

fn header(node: Node, body: Option<Node>, src: &str, names: Option<&Names>) -> String {
    let mut parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child) == body || matches!(child.kind(), "attribute_list" | "comment") {
            continue;
        }
        let rendered = php_ast::render(child, src, names);
        if names.is_some() && child.kind() == "class_interface_clause" {
            // `implements A, B` and `implements B, A` declare the same class.
            let mut interfaces: Vec<String> = php_ast::named_children(child)
                .into_iter()
                .map(|n| php_ast::render(n, src, names))
                .collect();
            interfaces.sort();
            parts.push(format!("implements {}", interfaces.join(", ")));
        } else {
            parts.push(rendered);
        }
    }
    parts.join(" ")
}

fn method_api(node: Node, src: &str, names: &Names) -> Option<MethodApi> {
    let name = php_ast::text(node.child_by_field_name("name")?, src).to_string();
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
    let params: Vec<ParamApi> = node
        .child_by_field_name("parameters")
        .map(|list| {
            php_ast::named_children(list)
                .into_iter()
                .map(|p| param_api(p, src, names))
                .collect()
        })
        .unwrap_or_default();
    let by_ref = if php_ast::has_child_kind(node, "reference_modifier") {
        "&"
    } else {
        ""
    };
    let return_type = node.child_by_field_name("return_type");
    let signature_of = |shown_name: &str, rendered: Vec<&str>, names: Option<&Names>| {
        let ret = return_type
            .map(|t| format!(": {}", php_ast::render_type(t, src, names)))
            .unwrap_or_default();
        format!("{by_ref}{shown_name}({}){ret}", rendered.join(", "))
    };
    let signature = signature_of(
        &name,
        params.iter().map(|p| p.text.as_str()).collect(),
        None,
    );
    // PHP method names are case-insensitive: `getList` and `GetList` declare one method.
    let canonical = signature_of(
        &name.to_ascii_lowercase(),
        params.iter().map(|p| p.canonical.as_str()).collect(),
        Some(names),
    );
    let body = node
        .child_by_field_name("body")
        .map(|b| {
            let mut hasher = DefaultHasher::new();
            php_ast::render(b, src, None).hash(&mut hasher);
            hasher.finish()
        })
        .unwrap_or(0);
    Some(MethodApi {
        visibility,
        is_static: php_ast::has_child_kind(node, "static_modifier"),
        is_abstract: php_ast::has_child_kind(node, "abstract_modifier"),
        is_final: php_ast::has_child_kind(node, "final_modifier"),
        signature,
        canonical,
        params,
        body,
        line: php_ast::line(node),
        name,
    })
}

fn param_api(node: Node, src: &str, names: &Names) -> ParamApi {
    let field = |name: &str| node.child_by_field_name(name);
    let render = |names: Option<&Names>| {
        let mut out = String::new();
        if let Some(visibility) = field("visibility") {
            out.push_str(&php_ast::text(visibility, src).to_ascii_lowercase());
            out.push(' ');
        }
        if field("readonly").is_some() {
            out.push_str("readonly ");
        }
        if let Some(t) = field("type") {
            out.push_str(&php_ast::render_type(t, src, names));
            out.push(' ');
        }
        if field("reference_modifier").is_some() || php_ast::has_child_kind(node, "&") {
            out.push('&');
        }
        if php_ast::has_child_kind(node, "...") {
            out.push_str("...");
        }
        if let Some(n) = field("name") {
            out.push_str(php_ast::text(n, src));
        }
        if let Some(default) = field("default_value") {
            out.push_str(" = ");
            out.push_str(&php_ast::render(default, src, names));
        }
        out
    };
    ParamApi {
        name: field("name")
            .map(|n| php_ast::text(n, src).to_string())
            .unwrap_or_default(),
        text: render(None),
        canonical: render(Some(names)),
        required: field("default_value").is_none() && node.kind() != "variadic_parameter",
    }
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MethodChange {
    pub name: String,
    /// `added`, `removed`, `changed` (signature or modifiers) or `body` (signature kept).
    pub change: &'static str,
    pub visibility: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility_before: Option<&'static str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub r#static: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub r#abstract: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Both versions exist and their bodies differ.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub body_changed: bool,
    /// Line in the new version, or in the old one for a removed method.
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConstructorChange {
    /// `added`, `removed` or `changed`.
    pub change: &'static str,
    pub visibility: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    pub params: ParamChanges,
    pub line: usize,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ParamChanges {
    pub added: Vec<AddedParam>,
    pub removed: Vec<String>,
    pub changed: Vec<ChangedParam>,
    /// The same parameters in another order: positional calls break.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reordered: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AddedParam {
    pub param: String,
    /// No default value and not variadic: every caller has to pass it.
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChangedParam {
    pub name: String,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HeaderChange {
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassDiff {
    pub header: Option<HeaderChange>,
    pub methods: Vec<MethodChange>,
    pub constructor: Option<ConstructorChange>,
}

impl ClassDiff {
    pub fn is_empty(&self) -> bool {
        self.header.is_none() && self.methods.is_empty() && self.constructor.is_none()
    }
}

const CONSTRUCTOR: &str = "__construct";

/// Methods that belong to the API of `class`: public ones; for an abstract class or an
/// interface protected ones too — subclasses implement and call them.
fn in_api(method: &MethodApi, protected_counts: bool) -> bool {
    method.visibility == "public" || (protected_counts && method.visibility == "protected")
}

fn protected_counts(classes: &[Option<&ClassApi>]) -> bool {
    classes
        .iter()
        .flatten()
        .any(|c| c.is_abstract || c.kind == "interface")
}

/// How the API of a class changed; `None` on a side means the class is new or gone.
pub fn diff_classes(before: Option<&ClassApi>, after: Option<&ClassApi>) -> ClassDiff {
    let protected = protected_counts(&[before, after]);
    let methods_of = |class: Option<&ClassApi>| -> HashMap<String, MethodApi> {
        class
            .map(|c| {
                c.methods
                    .iter()
                    .map(|m| (m.name.to_ascii_lowercase(), m.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let old = methods_of(before);
    let new = methods_of(after);

    let mut keys: Vec<&String> = old.keys().chain(new.keys()).collect();
    keys.sort();
    keys.dedup();

    let mut methods = Vec::new();
    let mut constructor = None;
    for key in keys {
        let (b, a) = (old.get(key), new.get(key));
        if key == CONSTRUCTOR {
            constructor = diff_constructor(b, a);
            continue;
        }
        if let Some(change) = diff_method(b, a, protected) {
            methods.push(change);
        }
    }
    methods.sort_by_key(|m| m.line);

    let header = match (before, after) {
        (Some(b), Some(a)) if b.header_canonical != a.header_canonical => Some(HeaderChange {
            before: b.header.clone(),
            after: a.header.clone(),
        }),
        _ => None,
    };
    ClassDiff {
        header,
        methods,
        constructor,
    }
}

fn diff_method(
    before: Option<&MethodApi>,
    after: Option<&MethodApi>,
    protected: bool,
) -> Option<MethodChange> {
    let listed_before = before.filter(|m| in_api(m, protected));
    let listed_after = after.filter(|m| in_api(m, protected));
    let base = |m: &MethodApi, change| MethodChange {
        name: m.name.clone(),
        change,
        visibility: m.visibility,
        visibility_before: None,
        r#static: m.is_static,
        r#abstract: m.is_abstract,
        before: None,
        after: None,
        body_changed: false,
        line: m.line,
    };
    // Narrowing public to protected takes the method out of the public API, even where
    // protected methods are listed.
    let narrowed = matches!((before, after), (Some(b), Some(a)) if b.visibility == "public" && a.visibility != "public");
    match (listed_before, listed_after) {
        (None, None) => None,
        (Some(b), _) if narrowed => {
            let a = after?;
            Some(MethodChange {
                visibility: a.visibility,
                visibility_before: Some(b.visibility),
                before: Some(b.signature.clone()),
                line: a.line,
                ..base(b, "removed")
            })
        }
        (Some(b), None) => Some(MethodChange {
            visibility_before: after.map(|_| b.visibility),
            visibility: after.map(|a| a.visibility).unwrap_or(b.visibility),
            before: Some(b.signature.clone()),
            ..base(b, "removed")
        }),
        (None, Some(a)) => Some(MethodChange {
            visibility_before: before.map(|b| b.visibility),
            after: Some(a.signature.clone()),
            ..base(a, "added")
        }),
        (Some(b), Some(a)) => {
            let body_changed = b.body != a.body;
            let modifiers_changed = b.is_static != a.is_static
                || b.is_abstract != a.is_abstract
                || b.is_final != a.is_final
                || b.visibility != a.visibility;
            if b.canonical != a.canonical || modifiers_changed {
                Some(MethodChange {
                    visibility_before: (b.visibility != a.visibility).then_some(b.visibility),
                    before: Some(b.signature.clone()),
                    after: Some(a.signature.clone()),
                    body_changed,
                    ..base(a, "changed")
                })
            } else if body_changed {
                Some(MethodChange {
                    body_changed,
                    ..base(a, "body")
                })
            } else {
                None
            }
        }
    }
}

fn diff_constructor(
    before: Option<&MethodApi>,
    after: Option<&MethodApi>,
) -> Option<ConstructorChange> {
    let empty: Vec<ParamApi> = Vec::new();
    let old_params = before.map(|m| &m.params).unwrap_or(&empty);
    let new_params = after.map(|m| &m.params).unwrap_or(&empty);
    let change = match (before, after) {
        (None, None) => return None,
        (None, Some(_)) => "added",
        (Some(_), None) => "removed",
        (Some(b), Some(a)) => {
            if b.canonical == a.canonical && b.visibility == a.visibility {
                return None;
            }
            "changed"
        }
    };
    let by_name = |params: &[ParamApi]| -> HashMap<String, ParamApi> {
        params.iter().map(|p| (p.name.clone(), p.clone())).collect()
    };
    let (old, new) = (by_name(old_params), by_name(new_params));
    let mut params = ParamChanges::default();
    for p in new_params {
        match old.get(&p.name) {
            None => params.added.push(AddedParam {
                param: p.text.clone(),
                required: p.required,
            }),
            Some(o) if o.canonical != p.canonical => params.changed.push(ChangedParam {
                name: p.name.clone(),
                before: o.text.clone(),
                after: p.text.clone(),
            }),
            Some(_) => {}
        }
    }
    for p in old_params {
        if !new.contains_key(&p.name) {
            params.removed.push(p.text.clone());
        }
    }
    let kept_order = |params: &[ParamApi]| -> Vec<String> {
        params
            .iter()
            .filter(|p| old.contains_key(&p.name) && new.contains_key(&p.name))
            .map(|p| p.name.clone())
            .collect()
    };
    params.reordered = kept_order(old_params) != kept_order(new_params);
    let shown = after.or(before)?;
    Some(ConstructorChange {
        change,
        visibility: shown.visibility,
        before: before.map(|m| m.signature.clone()),
        after: after.map(|m| m.signature.clone()),
        params,
        line: shown.line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(src: &str) -> ClassApi {
        let (classes, _) = class_apis(src).unwrap();
        classes.into_iter().next().unwrap()
    }

    fn diff(before: &str, after: &str) -> ClassDiff {
        diff_classes(Some(&class(before)), Some(&class(after)))
    }

    #[test]
    fn added_interface_method_is_reported_with_its_signature() {
        let d = diff(
            "<?php\nnamespace N;\ninterface FileService { public function get(int $id): array; }\n",
            "<?php\nnamespace N;\ninterface FileService {\n    public function get(int $id): array;\n    public function getPath(int $fileId): string;\n}\n",
        );
        assert_eq!(d.methods.len(), 1);
        assert_eq!(d.methods[0].change, "added");
        assert_eq!(d.methods[0].after.as_deref(), Some("getPath(int $fileId): string"));
        assert_eq!(d.methods[0].line, 5);
        assert!(d.constructor.is_none() && d.header.is_none());
    }

    #[test]
    fn phpdoc_and_layout_changes_are_no_change() {
        let d = diff(
            "<?php\ninterface R { public function resolve(string $fias, ?int $x = null): ?int; }\n",
            "<?php\n/** Contract. */\ninterface R\n{\n    /** Resolves. */\n    public function resolve(\n        string $fias,\n        ?int $x = null,\n    ): ?int;\n}\n",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn constructor_reports_added_required_and_promoted_parameters() {
        let d = diff(
            "<?php\nfinal readonly class City {\n    public function __construct(\n        private int $bitrixId,\n        private string $cityName = '',\n    ) {}\n}\n",
            "<?php\nfinal readonly class City {\n    public function __construct(\n        private int $bitrixId,\n        /** Presence. */\n        private bool $isPresence,\n        private string $cityName = '',\n        private int $bitrixRegionId = 0,\n    ) {}\n}\n",
        );
        let ctor = d.constructor.expect("constructor change");
        assert_eq!(ctor.change, "changed");
        assert_eq!(
            ctor.params.added,
            vec![
                AddedParam {
                    param: "private bool $isPresence".into(),
                    required: true
                },
                AddedParam {
                    param: "private int $bitrixRegionId = 0".into(),
                    required: false
                },
            ]
        );
        assert!(ctor.params.removed.is_empty() && !ctor.params.reordered);
        assert!(d.methods.is_empty());
    }

    #[test]
    fn attributes_and_new_defaults_leave_a_clean_signature() {
        let c = class(
            "<?php\nclass A { public function f(#[Assert\\Choice([1, 2])] int $a, Foo $foo = new Foo()): void {} }\n",
        );
        assert_eq!(c.methods[0].signature, "f(int $a, Foo $foo = new Foo()): void");
    }

    #[test]
    fn narrowing_to_protected_removes_from_public_api() {
        let d = diff(
            "<?php\nclass S { public function run(): void {} }\n",
            "<?php\nclass S { protected function run(): void {} }\n",
        );
        assert_eq!(d.methods[0].change, "removed");
        assert_eq!(d.methods[0].visibility, "protected");
        assert_eq!(d.methods[0].visibility_before, Some("public"));
    }

    #[test]
    fn protected_methods_count_in_abstract_classes_only() {
        let before = "<?php\nabstract class Base { protected function step(int $a): void {} }\n";
        let after = "<?php\nabstract class Base { protected function step(int $a, int $b): void {} }\n";
        let d = diff(before, after);
        assert_eq!(d.methods[0].change, "changed");
        assert_eq!(d.methods[0].visibility, "protected");
        let concrete = diff(
            &before.replace("abstract ", ""),
            &after.replace("abstract ", ""),
        );
        assert!(concrete.methods.is_empty());
    }

    #[test]
    fn default_value_change_is_a_signature_change_and_body_is_flagged() {
        let d = diff(
            "<?php\nclass S { public function f(int $a = 1) { return $a; } public function g() { return 1; } }\n",
            "<?php\nclass S { public function f(int $a = 2) { return $a; } public function g() { return 2; } }\n",
        );
        let f = d.methods.iter().find(|m| m.name == "f").unwrap();
        assert_eq!(f.change, "changed");
        assert!(!f.body_changed);
        let g = d.methods.iter().find(|m| m.name == "g").unwrap();
        assert_eq!(g.change, "body");
        assert!(g.body_changed);
    }

    #[test]
    fn import_alias_is_no_change_but_another_class_is() {
        let base = "<?php\nnamespace N;\nuse Lib\\Repo;\nclass S { public function f(Repo $r) {} }\n";
        let alias = "<?php\nnamespace N;\nuse Lib\\Repo as R;\nclass S { public function f(R $r) {} }\n";
        let other = "<?php\nnamespace N;\nuse Other\\Repo;\nclass S { public function f(Repo $r) {} }\n";
        assert!(diff(base, alias).is_empty());
        assert_eq!(diff(base, other).methods[0].change, "changed");
    }

    #[test]
    fn method_names_compare_case_insensitively() {
        let d = diff(
            "<?php\nclass S { public function getList() {} }\n",
            "<?php\nclass S { public function GetList() {} }\n",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn header_keeps_fully_qualified_names_apart_from_keywords() {
        let c = class("<?php\nclass C extends \\ArrayObject implements \\Countable, Lib\\Sized {}\n");
        assert_eq!(c.header, "class C extends \\ArrayObject implements \\Countable, Lib\\Sized");
    }

    #[test]
    fn header_change_reports_new_interface() {
        let d = diff(
            "<?php\nclass S implements A {}\n",
            "<?php\nclass S implements A, B {}\n",
        );
        assert_eq!(
            d.header,
            Some(HeaderChange {
                before: "class S implements A".into(),
                after: "class S implements A, B".into()
            })
        );
    }
}
