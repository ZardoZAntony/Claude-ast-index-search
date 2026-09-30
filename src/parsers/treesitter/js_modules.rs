//! ES-module facts of a JavaScript/TypeScript/Vue file: what it imports (with names), what it exports, and
//! where the imported names are used. Specifiers are stored as written: resolving them depends on other files
//! and on jsconfig/tsconfig, so it happens at query time (`commands::js`).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use tree_sitter::{Language, Node};

use super::{node_line, node_text, parse_tree};
use crate::parsers::typescript::extract_vue_script;

static TSX: LazyLock<Language> = LazyLock::new(|| tree_sitter_typescript::LANGUAGE_TSX.into());

/// A reference from this file to a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsImport {
    /// Line of the specifier string (what a move rewrites).
    pub line: usize,
    /// Line of the imported name (what a rename rewrites); equals `line` without a name.
    pub name_line: usize,
    /// `import`, `side` (import for side effects), `reexport`, `dynamic` (`import()`), `require`, `mock`
    /// (`vi.mock`/`jest.mock` and friends), `glob` (`import.meta.glob` pattern), `jsdoc` (`import('x').Type` in
    /// a comment).
    pub kind: &'static str,
    pub spec: String,
    /// Exported name taken: a name, `default` or `*`; `None` when the whole module is referenced.
    pub imported: Option<String>,
    /// Local binding (import) or exported alias (re-export).
    pub local: Option<String>,
}

/// A name this file exports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsExport {
    pub line: usize,
    /// Exported name; `default` for the default export.
    pub name: String,
    /// Local name behind the export when it differs or comes from a list (`export { a as b }`, `export default Foo`).
    pub local: Option<String>,
    /// Line of the local declaration, when the export site is elsewhere.
    pub decl_line: Option<usize>,
    /// `declaration`, `list`, `default`, `reexport`.
    pub kind: &'static str,
}

/// A use of an imported binding (`member` — `ns.member` of a namespace import).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JsUse {
    pub local: String,
    pub member: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsModule {
    pub imports: Vec<JsImport>,
    pub exports: Vec<JsExport>,
    pub uses: Vec<JsUse>,
}

/// Module facts of a JS/TS file, or of a Vue SFC (`vue`: script blocks plus the template).
pub fn extract(content: &str, vue: bool) -> JsModule {
    let script = if vue {
        extract_vue_script(content)
    } else {
        content.to_string()
    };
    let mut module = JsModule::default();
    if let Ok(tree) = parse_tree(&script, &TSX) {
        let root = tree.root_node();
        let declarations = top_level_declarations(root, &script);
        collect(root, &script, &declarations, &mut module);
        let bindings: HashSet<String> = module
            .imports
            .iter()
            .filter(|i| matches!(i.kind, "import" | "require" | "dynamic"))
            .filter_map(|i| i.local.clone())
            .collect();
        if !bindings.is_empty() {
            let mut uses = HashSet::new();
            collect_uses(root, &script, &bindings, &mut uses);
            module.uses.extend(uses);
        }
    }
    if vue {
        if !module.exports.iter().any(|e| e.name == "default") {
            module.exports.push(JsExport {
                line: 1,
                name: "default".to_string(),
                local: None,
                decl_line: None,
                kind: "default",
            });
        }
        let bindings: HashSet<String> = module
            .imports
            .iter()
            .filter(|i| i.kind == "import")
            .filter_map(|i| i.local.clone())
            .collect();
        template_uses(content, &bindings, &mut module.uses);
    }
    module.uses.sort_by(|a, b| (a.line, &a.local).cmp(&(b.line, &b.local)));
    module.uses.dedup();
    module
}

fn string_value(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "string" => {
            let text = node_text(src, &node);
            Some(text[1..text.len().saturating_sub(1).max(1)].to_string())
        }
        "template_string" => {
            let text = node_text(src, &node);
            // a template literal with substitutions is a built specifier, not a module name
            (!text.contains("${")).then(|| text.trim_matches('`').to_string())
        }
        _ => None,
    }
}

/// Top-level declared names → line, for exports listed away from their declaration.
fn top_level_declarations(root: Node, src: &str) -> HashMap<String, usize> {
    let mut names = HashMap::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let decl = if child.kind() == "export_statement" {
            match child.child_by_field_name("declaration") {
                Some(d) => d,
                None => continue,
            }
        } else {
            child
        };
        for (name, line) in declared_names(decl, src) {
            names.entry(name).or_insert(line);
        }
    }
    names
}

/// Names a declaration introduces, with their lines.
fn declared_names(decl: Node, src: &str) -> Vec<(String, usize)> {
    let decl = if decl.kind() == "ambient_declaration" {
        match decl.named_child(0) {
            Some(inner) => inner,
            None => return vec![],
        }
    } else {
        decl
    };
    match decl.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "abstract_class_declaration"
        | "interface_declaration"
        | "type_alias_declaration"
        | "enum_declaration"
        | "internal_module"
        | "module"
        | "function_signature" => decl
            .child_by_field_name("name")
            .map(|n| vec![(node_text(src, &n).to_string(), node_line(&n))])
            .unwrap_or_default(),
        "lexical_declaration" | "variable_declaration" => {
            let mut out = Vec::new();
            let mut cursor = decl.walk();
            for declarator in decl.named_children(&mut cursor) {
                if declarator.kind() == "variable_declarator" {
                    if let Some(name) = declarator.child_by_field_name("name") {
                        pattern_names(name, src, &mut out);
                    }
                }
            }
            out
        }
        _ => vec![],
    }
}

fn pattern_names(node: Node, src: &str, out: &mut Vec<(String, usize)>) {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            out.push((node_text(src, &node).to_string(), node_line(&node)))
        }
        "pair_pattern" => {
            if let Some(value) = node.child_by_field_name("value") {
                pattern_names(value, src, out);
            }
        }
        "assignment_pattern" => {
            if let Some(left) = node.child_by_field_name("left") {
                pattern_names(left, src, out);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                pattern_names(child, src, out);
            }
        }
    }
}

fn collect(node: Node, src: &str, declarations: &HashMap<String, usize>, module: &mut JsModule) {
    match node.kind() {
        "import_statement" => {
            import_statement(node, src, module);
            return;
        }
        "export_statement" => export_statement(node, src, declarations, module),
        "call_expression" => call_expression(node, src, module),
        "comment" => jsdoc_imports(node, src, module),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(child, src, declarations, module);
    }
}

fn import_statement(node: Node, src: &str, module: &mut JsModule) {
    let Some(source) = node.child_by_field_name("source") else {
        return;
    };
    let Some(spec) = string_value(source, src) else {
        return;
    };
    let line = node_line(&source);
    let mut cursor = node.walk();
    let clause = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "import_clause");
    let Some(clause) = clause else {
        module.imports.push(JsImport {
            line,
            name_line: line,
            kind: "side",
            spec,
            imported: None,
            local: None,
        });
        return;
    };
    let mut push = |imported: String, local: &Node| {
        module.imports.push(JsImport {
            line,
            name_line: node_line(local),
            kind: "import",
            spec: spec.clone(),
            imported: Some(imported),
            local: Some(node_text(src, local).to_string()),
        });
    };
    let mut cursor = clause.walk();
    for part in clause.named_children(&mut cursor) {
        match part.kind() {
            "identifier" => push("default".to_string(), &part),
            "namespace_import" => {
                if let Some(local) = part.named_child(0) {
                    push("*".to_string(), &local);
                }
            }
            "named_imports" => {
                let mut specs = part.walk();
                for specifier in part.named_children(&mut specs) {
                    if specifier.kind() != "import_specifier" {
                        continue;
                    }
                    let Some(name) = specifier.child_by_field_name("name") else {
                        continue;
                    };
                    let imported = string_value(name, src).unwrap_or_else(|| node_text(src, &name).to_string());
                    let local = specifier.child_by_field_name("alias").unwrap_or(name);
                    push(imported, &local);
                }
            }
            _ => {}
        }
    }
}

fn export_statement(node: Node, src: &str, declarations: &HashMap<String, usize>, module: &mut JsModule) {
    let line = node_line(&node);
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();
    let is_default = children.iter().any(|c| c.kind() == "default");
    let source = node.child_by_field_name("source");
    let spec = source.and_then(|s| string_value(s, src));

    if let Some(decl) = node.child_by_field_name("declaration") {
        let names = declared_names(decl, src);
        if is_default {
            let local = names.first().map(|(n, _)| n.clone());
            module.exports.push(JsExport {
                line,
                name: "default".to_string(),
                local,
                decl_line: None,
                kind: "default",
            });
        } else {
            for (name, decl_line) in names {
                module.exports.push(JsExport {
                    line: decl_line,
                    name,
                    local: None,
                    decl_line: None,
                    kind: "declaration",
                });
            }
        }
        return;
    }
    if is_default {
        let local = node
            .child_by_field_name("value")
            .filter(|v| v.kind() == "identifier")
            .map(|v| node_text(src, &v).to_string());
        let decl_line = local.as_ref().and_then(|l| declarations.get(l).copied());
        module.exports.push(JsExport {
            line,
            name: "default".to_string(),
            local,
            decl_line,
            kind: "default",
        });
        return;
    }

    let clause = children.iter().find(|c| c.kind() == "export_clause");
    if let Some(clause) = clause {
        let mut specs = clause.walk();
        for specifier in clause.named_children(&mut specs) {
            if specifier.kind() != "export_specifier" {
                continue;
            }
            let Some(name) = specifier.child_by_field_name("name") else {
                continue;
            };
            let local = string_value(name, src).unwrap_or_else(|| node_text(src, &name).to_string());
            let exported = specifier
                .child_by_field_name("alias")
                .map(|a| string_value(a, src).unwrap_or_else(|| node_text(src, &a).to_string()))
                .unwrap_or_else(|| local.clone());
            let spec_line = node_line(&specifier);
            match &spec {
                Some(spec) => {
                    module.imports.push(JsImport {
                        line: source.map(|s| node_line(&s)).unwrap_or(line),
                        name_line: spec_line,
                        kind: "reexport",
                        spec: spec.clone(),
                        imported: Some(local),
                        local: Some(exported.clone()),
                    });
                    module.exports.push(JsExport {
                        line: spec_line,
                        name: exported,
                        local: None,
                        decl_line: None,
                        kind: "reexport",
                    });
                }
                None => module.exports.push(JsExport {
                    line: spec_line,
                    decl_line: declarations.get(&local).copied(),
                    name: exported,
                    local: Some(local),
                    kind: "list",
                }),
            }
        }
        return;
    }

    // export * from 'x' / export * as ns from 'x'
    if let Some(spec) = spec {
        let namespace = children
            .iter()
            .find(|c| c.kind() == "namespace_export")
            .and_then(|n| n.named_child(0))
            .map(|n| string_value(n, src).unwrap_or_else(|| node_text(src, &n).to_string()));
        let spec_line = source.map(|s| node_line(&s)).unwrap_or(line);
        module.imports.push(JsImport {
            line: spec_line,
            name_line: line,
            kind: "reexport",
            spec,
            imported: Some("*".to_string()),
            local: namespace.clone(),
        });
        if let Some(ns) = namespace {
            module.exports.push(JsExport {
                line,
                name: ns,
                local: None,
                decl_line: None,
                kind: "reexport",
            });
        }
    }
}

/// `import('../x.js').Type` in JSDoc and TS comments: a type taken from a module by path.
fn jsdoc_imports(node: Node, src: &str, module: &mut JsModule) {
    static JSDOC_IMPORT: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"import\(\s*(?:'([^']+)'|"([^"]+)")\s*\)(?:\.([A-Za-z_$][\w$]*))?"#).unwrap()
    });
    let text = node_text(src, &node);
    if !text.contains("import(") {
        return;
    }
    let first_line = node_line(&node);
    for cap in JSDOC_IMPORT.captures_iter(text) {
        let Some(spec) = cap.get(1).or_else(|| cap.get(2)) else {
            continue;
        };
        let line = first_line + text[..spec.start()].matches('\n').count();
        module.imports.push(JsImport {
            line,
            name_line: line,
            kind: "jsdoc",
            spec: spec.as_str().to_string(),
            imported: cap.get(3).map(|m| m.as_str().to_string()),
            local: None,
        });
    }
}

const MOCK_CALLS: &[&str] = &[
    "vi.mock",
    "vi.doMock",
    "vi.unmock",
    "vi.doUnmock",
    "vi.importActual",
    "vi.importMock",
    "jest.mock",
    "jest.doMock",
    "jest.unmock",
    "jest.requireActual",
    "jest.requireMock",
];

fn call_expression(node: Node, src: &str, module: &mut JsModule) {
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };
    let Some(first) = arguments.named_child(0) else {
        return;
    };
    let callee: String = node_text(src, &function)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let kind = match (function.kind(), callee.as_str()) {
        ("import", _) => "dynamic",
        (_, "require") => "require",
        (_, "import.meta.glob") | (_, "import.meta.globEager") => "glob",
        (_, c) if MOCK_CALLS.contains(&c) => "mock",
        _ => return,
    };
    let specs: Vec<(String, usize)> = if kind == "glob" && first.kind() == "array" {
        let mut cursor = first.walk();
        first
            .named_children(&mut cursor)
            .filter_map(|e| string_value(e, src).map(|s| (s, node_line(&e))))
            .collect()
    } else {
        string_value(first, src)
            .map(|s| vec![(s, node_line(&first))])
            .unwrap_or_default()
    };
    // names a `require()` or `import()` result is taken apart into; `None` — the whole module
    let taken = matches!(kind, "require" | "dynamic")
        .then(|| taken_names(node, src))
        .flatten()
        .filter(|names| !names.is_empty());
    for (spec, line) in specs {
        if kind == "glob" && spec.starts_with('!') {
            continue;
        }
        match &taken {
            Some(names) => {
                for (imported, local, name_line) in names {
                    module.imports.push(JsImport {
                        line,
                        name_line: *name_line,
                        kind,
                        spec: spec.clone(),
                        imported: Some(imported.clone()),
                        local: local.clone(),
                    });
                }
            }
            None => module.imports.push(JsImport {
                line,
                name_line: line,
                kind,
                spec,
                imported: None,
                local: None,
            }),
        }
    }
}

/// What a `require()`/`import()` call's result is taken apart into: `(exported name, local binding, line)`;
/// `("*", Some(m))` for a module object bound to a name (its `m.x` uses are tracked like a namespace import).
/// `None` when the module object escapes (returned, passed on), so the whole module counts as used.
///
/// `const { a, b: c } = await import('x')`, `(await import('x')).a`, `import('x').then((m) => m.a)`,
/// `import('x').then(({ a }) => …)`, `const m = await import('x')`, `const { a } = require('x')`.
fn taken_names(call: Node, src: &str) -> Option<Vec<(String, Option<String>, usize)>> {
    let mut outer = call;
    while let Some(parent) = outer.parent() {
        if matches!(parent.kind(), "parenthesized_expression" | "await_expression") {
            outer = parent;
        } else {
            break;
        }
    }
    let parent = outer.parent()?;
    match parent.kind() {
        "variable_declarator" if parent.child_by_field_name("value").is_some_and(|v| v.id() == outer.id()) => {
            let name = parent.child_by_field_name("name")?;
            match name.kind() {
                "identifier" => Some(vec![(
                    "*".to_string(),
                    Some(node_text(src, &name).to_string()),
                    node_line(&name),
                )]),
                "object_pattern" => object_pattern_names(name, src),
                _ => None,
            }
        }
        "member_expression" if parent.child_by_field_name("object").is_some_and(|o| o.id() == outer.id()) => {
            let property = parent.child_by_field_name("property")?;
            let name = node_text(src, &property).to_string();
            if name == "then" && outer.id() == call.id() {
                then_callback_names(parent.parent()?, src)
            } else {
                Some(vec![(name, None, node_line(&property))])
            }
        }
        _ => None,
    }
}

/// `{ a, b: c }` → `[(a, a), (b, c)]`; a rest element keeps the whole module.
fn object_pattern_names(pattern: Node, src: &str) -> Option<Vec<(String, Option<String>, usize)>> {
    let mut out = Vec::new();
    let mut cursor = pattern.walk();
    for part in pattern.named_children(&mut cursor) {
        match part.kind() {
            "shorthand_property_identifier_pattern" => {
                let name = node_text(src, &part).to_string();
                out.push((name.clone(), Some(name), node_line(&part)));
            }
            "pair_pattern" => {
                let key = part.child_by_field_name("key")?;
                let value = part.child_by_field_name("value")?;
                let local = (value.kind() == "identifier").then(|| node_text(src, &value).to_string());
                out.push((node_text(src, &key).trim_matches(['\'', '"']).to_string(), local, node_line(&key)));
            }
            "object_assignment_pattern" => {
                let left = part.child_by_field_name("left")?;
                let name = node_text(src, &left).to_string();
                out.push((name.clone(), Some(name), node_line(&left)));
            }
            "comment" => {}
            _ => return None,
        }
    }
    Some(out)
}

/// `.then((m) => m.a)` / `.then(({ a }) => …)`: names the callback takes from the module object.
fn then_callback_names(then_call: Node, src: &str) -> Option<Vec<(String, Option<String>, usize)>> {
    let callback = then_call.child_by_field_name("arguments")?.named_child(0)?;
    if !matches!(callback.kind(), "arrow_function" | "function_expression" | "function") {
        return None;
    }
    let param = match callback.child_by_field_name("parameter") {
        Some(p) => p,
        None => {
            let params = callback.child_by_field_name("parameters")?;
            let first = params.named_child(0)?;
            first.child_by_field_name("pattern").unwrap_or(first)
        }
    };
    match param.kind() {
        "object_pattern" => object_pattern_names(param, src),
        "identifier" => {
            let name = node_text(src, &param);
            let body = callback.child_by_field_name("body")?;
            let mut members = Vec::new();
            member_uses(body, src, name, &mut members).then_some(members)
        }
        _ => None,
    }
}

/// Members read from `name` in `node` (`name.a`); false when `name` is used otherwise (passed on whole).
fn member_uses(node: Node, src: &str, name: &str, out: &mut Vec<(String, Option<String>, usize)>) -> bool {
    if node.kind() == "member_expression" {
        if let (Some(object), Some(property)) = (node.child_by_field_name("object"), node.child_by_field_name("property")) {
            if object.kind() == "identifier" && node_text(src, &object) == name {
                out.push((node_text(src, &property).to_string(), None, node_line(&property)));
                return true;
            }
        }
    }
    if node.kind() == "identifier" && node_text(src, &node) == name {
        return false;
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    children.into_iter().all(|child| member_uses(child, src, name, out))
}

fn collect_uses(node: Node, src: &str, bindings: &HashSet<String>, uses: &mut HashSet<JsUse>) {
    match node.kind() {
        "import_statement" => return,
        "member_expression" | "nested_type_identifier" | "nested_identifier" => {
            let (object, property) = match node.kind() {
                "member_expression" => (
                    node.child_by_field_name("object"),
                    node.child_by_field_name("property"),
                ),
                "nested_type_identifier" => (
                    node.child_by_field_name("module"),
                    node.child_by_field_name("name"),
                ),
                _ => (node.named_child(0), node.named_child(1)),
            };
            if let (Some(object), Some(property)) = (object, property) {
                let name = node_text(src, &object);
                if object.kind() == "identifier" && bindings.contains(name) {
                    uses.insert(JsUse {
                        local: name.to_string(),
                        member: Some(node_text(src, &property).to_string()),
                        line: node_line(&object),
                    });
                    return;
                }
            }
        }
        "identifier" | "type_identifier" | "shorthand_property_identifier" => {
            let name = node_text(src, &node);
            if bindings.contains(name) && !is_binding_site(node) {
                uses.insert(JsUse {
                    local: name.to_string(),
                    member: None,
                    line: node_line(&node),
                });
            }
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_uses(child, src, bindings, uses);
    }
}

/// `const { useFoo } = x` or a parameter named like an import: a new binding, not a use.
fn is_binding_site(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "variable_declarator" | "required_parameter" | "optional_parameter" => parent
            .child_by_field_name("name")
            .or_else(|| parent.child_by_field_name("pattern"))
            .is_some_and(|n| n.id() == node.id()),
        "function_declaration" | "class_declaration" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        _ => false,
    }
}

/// Uses of imported names in the `<template>` of a Vue SFC: component tags (PascalCase or kebab-case) and
/// identifiers in bindings and interpolations.
fn template_uses(content: &str, bindings: &HashSet<String>, uses: &mut Vec<JsUse>) {
    let (Some(start), Some(end)) = (content.find("<template"), content.rfind("</template>")) else {
        return;
    };
    if bindings.is_empty() || end <= start {
        return;
    }
    let first_line = content[..start].matches('\n').count() + 1;
    let region = &content[start..end];
    let bytes = region.as_bytes();
    let mut line = first_line;
    let mut seen = HashSet::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
            let begin = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$' | b'-')) {
                i += 1;
            }
            let word = &region[begin..i];
            let prev = if begin > 0 { bytes[begin - 1] } else { b' ' };
            if prev == b'.' {
                continue;
            }
            let name = if word.contains('-') {
                // kebab-case tag of a component (`<base-popup>`) — anything else with a dash is not a binding
                if prev != b'<' && !(prev == b'/' && begin > 1 && bytes[begin - 2] == b'<') {
                    continue;
                }
                kebab_to_pascal(word)
            } else {
                word.to_string()
            };
            if bindings.contains(&name) && seen.insert((name.clone(), line)) {
                uses.push(JsUse {
                    local: name,
                    member: None,
                    line,
                });
            }
            continue;
        }
        i += 1;
    }
}

fn kebab_to_pascal(word: &str) -> String {
    word.split('-')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut chars = p.chars();
            chars
                .next()
                .map(|f| f.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (kind, imported, local, line, name_line) of the imports of `spec`.
    type Row = (String, Option<String>, Option<String>, usize, usize);

    fn imp(m: &JsModule, spec: &str) -> Vec<Row> {
        m.imports
            .iter()
            .filter(|i| i.spec == spec)
            .map(|i| (i.kind.to_string(), i.imported.clone(), i.local.clone(), i.line, i.name_line))
            .collect()
    }

    #[test]
    fn imports_with_names_lines_and_kinds() {
        let src = "import Popup from '@/ui/Popup.vue'\nimport {\n  useFoo,\n  bar as baz,\n} from './foo'\nimport * as api from '../api'\nimport './styles.css'\nimport type { Props } from './types'\n";
        let m = extract(src, false);
        assert_eq!(imp(&m, "@/ui/Popup.vue"), vec![("import".into(), Some("default".into()), Some("Popup".into()), 1, 1)]);
        assert_eq!(
            imp(&m, "./foo"),
            vec![
                ("import".into(), Some("useFoo".into()), Some("useFoo".into()), 5, 3),
                ("import".into(), Some("bar".into()), Some("baz".into()), 5, 4),
            ]
        );
        assert_eq!(imp(&m, "../api"), vec![("import".into(), Some("*".into()), Some("api".into()), 6, 6)]);
        assert_eq!(imp(&m, "./styles.css"), vec![("side".into(), None, None, 7, 7)]);
        assert_eq!(imp(&m, "./types"), vec![("import".into(), Some("Props".into()), Some("Props".into()), 8, 8)]);
    }

    #[test]
    fn exports_of_every_form() {
        let src = "export function useFoo() {}\nexport const A = 1, { b, c: d } = obj\nconst local = 2\nexport { local as renamed, useFoo as alias }\nexport default local\nexport { x as y } from './x'\nexport * from './all'\nexport * as ns from './ns'\nexport interface I {}\nexport type T = string\nexport enum E { A }\nexport class K {}\n";
        let m = extract(src, false);
        let names: Vec<(&str, &str, usize, Option<usize>)> = m
            .exports
            .iter()
            .map(|e| (e.name.as_str(), e.kind, e.line, e.decl_line))
            .collect();
        assert!(names.contains(&("useFoo", "declaration", 1, None)));
        assert!(names.contains(&("A", "declaration", 2, None)));
        assert!(names.contains(&("b", "declaration", 2, None)));
        assert!(names.contains(&("d", "declaration", 2, None)));
        assert!(names.contains(&("renamed", "list", 4, Some(3))));
        assert!(names.contains(&("alias", "list", 4, Some(1))));
        assert!(names.contains(&("default", "default", 5, Some(3))));
        assert!(names.contains(&("y", "reexport", 6, None)));
        assert!(names.contains(&("ns", "reexport", 8, None)));
        assert!(names.contains(&("I", "declaration", 9, None)));
        assert!(names.contains(&("T", "declaration", 10, None)));
        assert!(names.contains(&("E", "declaration", 11, None)));
        assert!(names.contains(&("K", "declaration", 12, None)));
        assert_eq!(imp(&m, "./x"), vec![("reexport".into(), Some("x".into()), Some("y".into()), 6, 6)]);
        assert_eq!(imp(&m, "./all"), vec![("reexport".into(), Some("*".into()), None, 7, 7)]);
        assert_eq!(imp(&m, "./ns"), vec![("reexport".into(), Some("*".into()), Some("ns".into()), 8, 8)]);
    }

    #[test]
    fn default_function_and_class_exports() {
        let m = extract("export default function setup() {}\n", false);
        assert_eq!(m.exports[0].name, "default");
        assert_eq!(m.exports[0].local.as_deref(), Some("setup"));
        let m = extract("export default {\n  name: 'X',\n}\n", false);
        assert_eq!(m.exports[0].name, "default");
        assert_eq!(m.exports[0].local, None);
    }

    #[test]
    fn dynamic_require_mock_and_glob() {
        let src = "const Page = () => import('./Page.vue')\nconst cfg = require('./cfg')\nvi.mock('@/shared/api', () => ({}))\nconst mods = import.meta.glob('./blocks/*.vue', { eager: true })\nconst many = import.meta.glob(['./a/*.js', '!./a/skip.js'])\nimport(`./${name}.js`)\n";
        let m = extract(src, false);
        assert_eq!(imp(&m, "./Page.vue"), vec![("dynamic".into(), None, None, 1, 1)]);
        assert_eq!(imp(&m, "./cfg"), vec![("require".into(), Some("*".into()), Some("cfg".into()), 2, 2)]);
        assert_eq!(imp(&m, "@/shared/api"), vec![("mock".into(), None, None, 3, 3)]);
        assert_eq!(imp(&m, "./blocks/*.vue"), vec![("glob".into(), None, None, 4, 4)]);
        assert_eq!(imp(&m, "./a/*.js").len(), 1);
        assert!(imp(&m, "!./a/skip.js").is_empty());
        assert!(m.imports.iter().all(|i| !i.spec.contains("${")));
    }

    #[test]
    fn dynamic_imports_taken_apart_by_name() {
        let src = "const { a, b: bee } = await import('./m1')\nconst x = (await import('./m2')).c\nimport('./m3').then((mod) => ({ default: mod.D }))\nimport('./m4').then(({ e }) => e)\nconst whole = await import('./m5')\nwhole.f()\nconst load = () => import('./m6')\nimport('./m7').then((m) => use(m))\nconst { g } = require('./m8')\n";
        let m = extract(src, false);
        assert_eq!(
            imp(&m, "./m1"),
            vec![
                ("dynamic".into(), Some("a".into()), Some("a".into()), 1, 1),
                ("dynamic".into(), Some("b".into()), Some("bee".into()), 1, 1),
            ]
        );
        assert_eq!(imp(&m, "./m2"), vec![("dynamic".into(), Some("c".into()), None, 2, 2)]);
        assert_eq!(imp(&m, "./m3"), vec![("dynamic".into(), Some("D".into()), None, 3, 3)]);
        assert_eq!(imp(&m, "./m4"), vec![("dynamic".into(), Some("e".into()), Some("e".into()), 4, 4)]);
        assert_eq!(imp(&m, "./m5"), vec![("dynamic".into(), Some("*".into()), Some("whole".into()), 5, 5)]);
        assert!(m.uses.contains(&JsUse { local: "whole".into(), member: Some("f".into()), line: 6 }));
        assert_eq!(imp(&m, "./m6"), vec![("dynamic".into(), None, None, 7, 7)]);
        assert_eq!(imp(&m, "./m7"), vec![("dynamic".into(), None, None, 8, 8)], "the module object escapes");
        assert_eq!(imp(&m, "./m8"), vec![("require".into(), Some("g".into()), Some("g".into()), 9, 9)]);
    }

    #[test]
    fn jsdoc_type_imports_from_comments() {
        let src = "/**\n * @param {import('./types.js').Props} props\n * @returns {import(\"vue\").Ref}\n */\nfunction f(props) {}\n// import('./not-in-jsdoc') is still a path\n";
        let m = extract(src, false);
        assert_eq!(imp(&m, "./types.js"), vec![("jsdoc".into(), Some("Props".into()), None, 2, 2)]);
        assert_eq!(imp(&m, "vue"), vec![("jsdoc".into(), Some("Ref".into()), None, 3, 3)]);
        assert_eq!(imp(&m, "./not-in-jsdoc").len(), 1);
    }

    #[test]
    fn uses_of_imported_names_only() {
        let src = "import { useFoo, Status } from './foo'\nimport * as api from './api'\nimport type { Props } from './types'\nconst x = useFoo()\nif (s === Status.IDLE) {}\napi.load(1)\nfunction f(p: Props) { return { useFoo } }\nobj.useFoo()\nconst useFooLocal = 1\n";
        let m = extract(src, false);
        let uses: Vec<(&str, Option<&str>, usize)> = m
            .uses
            .iter()
            .map(|u| (u.local.as_str(), u.member.as_deref(), u.line))
            .collect();
        assert!(uses.contains(&("useFoo", None, 4)));
        assert!(uses.contains(&("Status", Some("IDLE"), 5)));
        assert!(uses.contains(&("api", Some("load"), 6)));
        assert!(uses.contains(&("Props", None, 7)));
        assert!(uses.contains(&("useFoo", None, 7)));
        assert!(!uses.iter().any(|u| u.2 == 8), "obj.useFoo is a property, not the import");
        assert!(!uses.iter().any(|u| u.2 == 1), "the import line itself is not a use");
    }

    #[test]
    fn vue_script_setup_template_and_implicit_default() {
        let src = "<template>\n  <BasePopup :open=\"isOpen\">\n    <base-popup-header />\n    {{ formatPrice(price) }}\n    <div class=\"format-price\" />\n  </BasePopup>\n</template>\n\n<script setup>\nimport BasePopup from '@/ui/BasePopup.vue'\nimport BasePopupHeader from '@/ui/BasePopupHeader.vue'\nimport { formatPrice } from '@/shared/price'\n</script>\n";
        let m = extract(src, true);
        assert_eq!(imp(&m, "@/ui/BasePopup.vue")[0].3, 10);
        let uses: Vec<(&str, usize)> = m.uses.iter().map(|u| (u.local.as_str(), u.line)).collect();
        assert!(uses.contains(&("BasePopup", 2)));
        assert!(uses.contains(&("BasePopup", 6)));
        assert!(uses.contains(&("BasePopupHeader", 3)));
        assert!(uses.contains(&("formatPrice", 4)));
        assert!(!uses.contains(&("formatPrice", 5)), "a class name is not the function");
        assert!(m.exports.iter().any(|e| e.name == "default" && e.kind == "default"));
    }
}
