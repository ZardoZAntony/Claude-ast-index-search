//! Class members as data: `enum '<FQN>'` — cases with their values and what methods such as
//! `ttl()` return for each case; `usages '<FQN>::<MEMBER>'` — where a constant or an enum case
//! of that very class is used (not of a namesake class).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{bail, Result};
use colored::Colorize;
use serde::Serialize;
use serde_json::{json, Value as Json};
use tree_sitter::Node;

use super::php::{ensure_php_names, is_test_path, open, print_json, resolve_one_class, with_subtypes};
use super::php_classes::IndexClasses;
use crate::parsers::php_ast::{self, Names};
use crate::parsers::php_calls::Classes;
use crate::parsers::php_eval::{Constants, Evaluator, Val};
use crate::parsers::php_model;

// ---------------------------------------------------------------------------
// enum <FQN> [--method m]
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CaseEntry {
    name: String,
    value: Json,
    line: usize,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    methods: BTreeMap<String, Json>,
}

pub fn cmd_enum(root: &Path, fqn: &str, methods: &[String], format: &str) -> Result<()> {
    let _lease = crate::db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let fqn = resolve_one_class(&conn, fqn)?;
    let classes = IndexClasses::new(&conn, root);
    let Some(model) = classes.class(&fqn) else {
        bail!("{fqn} is not readable from its file");
    };
    if model.kind != "enum" {
        bail!("{fqn} is a {}, not an enum", model.kind);
    }
    let Some((path, content)) = classes.source(&fqn) else {
        bail!("{fqn} is not readable from its file");
    };
    let tree = php_ast::parse(&content)?;
    let root_node = tree.root_node();
    let names = Names::of(root_node, &content);
    let Some(enum_node) = php_model::find_class_node(root_node, &content, &names, &fqn) else {
        bail!("{fqn} is not readable from its file");
    };
    let backed = php_ast::named_children(enum_node)
        .into_iter()
        .find(|n| n.kind() == "primitive_type")
        .map(|n| php_ast::text(n, &content).to_string());

    let mut cases: Vec<CaseEntry> = model
        .cases
        .iter()
        .map(|c| CaseEntry {
            name: c.name.clone(),
            value: match classes.class_constant(&fqn, &c.name, 0) {
                Some(Val::Case { value: Some(v), .. }) => v.to_json(),
                _ => Json::Null,
            },
            line: c.line,
            methods: BTreeMap::new(),
        })
        .collect();

    let mut unsupported = Vec::new();
    for wanted in methods {
        let Some(method_node) = enum_node.child_by_field_name("body").and_then(|body| {
            php_ast::named_children(body).into_iter().find(|m| {
                m.kind() == "method_declaration"
                    && m.child_by_field_name("name")
                        .is_some_and(|n| php_ast::text(n, &content).eq_ignore_ascii_case(wanted))
            })
        }) else {
            unsupported.push(json!({"method": wanted, "reason": "not declared in the enum"}));
            continue;
        };
        match per_case_values(method_node, &content, &names, &classes, &fqn, &model.cases) {
            Some(values) => {
                for case in &mut cases {
                    if let Some(v) = values.get(&case.name) {
                        case.methods.insert(wanted.clone(), v.clone());
                    }
                }
            }
            None => unsupported.push(json!({
                "method": wanted,
                "reason": "the body is not `return match ($this) {…}` or a single `return`",
                "line": php_ast::line(method_node),
            })),
        }
    }
    let declared_methods: Vec<String> = {
        let mut m: Vec<String> = model.methods.values().map(|m| m.name.clone()).collect();
        m.sort();
        m
    };

    if format == "json" {
        return print_json(&json!({
            "enum": fqn,
            "path": path,
            "line": model.line,
            "backed": backed,
            "cases": cases,
            "methods": declared_methods,
            "unsupported": unsupported,
        }));
    }
    println!(
        "{}  {}",
        format!("enum {fqn}{}", backed.as_deref().map(|b| format!(": {b}")).unwrap_or_default()).bold(),
        format!("{path}:{}", model.line).cyan()
    );
    for case in &cases {
        let mut line = format!("  :{} {} = {}", case.line, case.name, case.value);
        for (m, v) in &case.methods {
            let shown = v.get("value").cloned().unwrap_or(Json::Null);
            let shown = if shown.is_null() {
                v.get("expr").cloned().unwrap_or(Json::Null)
            } else {
                shown
            };
            line.push_str(&format!("  {m}={shown}"));
        }
        println!("{line}");
    }
    for u in &unsupported {
        println!("  {} {}", "note:".yellow(), u);
    }
    if methods.is_empty() && !declared_methods.is_empty() {
        println!("  methods: {} (values per case: --method <name>)", declared_methods.join(", "));
    }
    Ok(())
}

/// Case name → `{"value": …, "line": N}` for a method whose body is `return match ($this) {…}`
/// or one `return` that holds for every case.
fn per_case_values(
    method: Node,
    src: &str,
    names: &Names,
    constants: &dyn Constants,
    fqn: &str,
    cases: &[php_model::ConstantModel],
) -> Option<BTreeMap<String, Json>> {
    let body = method.child_by_field_name("body")?;
    let statements = php_ast::named_children(body);
    let returned = match statements.as_slice() {
        [only] if only.kind() == "return_statement" => php_ast::named_children(*only).into_iter().next()?,
        _ => return None,
    };
    let evaluate = |case: &str, expr: Node| -> Json {
        let this = constants.class_constant(fqn, case, 0).unwrap_or(Val::Case {
            fqn: format!("{fqn}::{case}"),
            value: None,
        });
        let mut evaluator = Evaluator {
            class: Some(fqn.to_string()),
            ..Evaluator::new(src, names, constants)
        };
        evaluator.bindings.insert("this".into(), this);
        match evaluator.eval(expr) {
            Val::Expr(text) => json!({"value": null, "expr": text, "line": php_ast::line(expr)}),
            v => json!({"value": v.to_json(), "line": php_ast::line(expr)}),
        }
    };
    let mut out = BTreeMap::new();
    if returned.kind() != "match_expression" {
        for case in cases {
            out.insert(case.name.clone(), evaluate(&case.name, returned));
        }
        return Some(out);
    }
    let subject = returned
        .child_by_field_name("condition")
        .and_then(|c| php_ast::named_children(c).into_iter().next())?;
    if php_ast::text(subject, src) != "$this" {
        return None;
    }
    let block = returned.child_by_field_name("body")?;
    let mut default = None;
    for arm in php_ast::named_children(block) {
        let value = arm.child_by_field_name("return_expression");
        match arm.kind() {
            "match_default_expression" => default = value,
            "match_conditional_expression" => {
                let (Some(conditions), Some(value)) =
                    (arm.child_by_field_name("conditional_expressions"), value)
                else {
                    continue;
                };
                for condition in php_ast::named_children(conditions) {
                    if condition.kind() != "class_constant_access_expression" {
                        continue;
                    }
                    let parts = php_ast::named_children(condition);
                    let (Some(scope), Some(member)) = (parts.first(), parts.get(1)) else {
                        continue;
                    };
                    let scope_text = php_ast::text(*scope, src);
                    let names_this_enum = matches!(scope_text.to_ascii_lowercase().as_str(), "self" | "static")
                        || names.resolve_class(scope_text, scope.start_byte()).eq_ignore_ascii_case(fqn);
                    if names_this_enum {
                        let case = php_ast::text(*member, src).to_string();
                        let v = evaluate(&case, value);
                        out.entry(case).or_insert(v);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(default) = default {
        for case in cases {
            if !out.contains_key(&case.name) {
                out.insert(case.name.clone(), evaluate(&case.name, default));
            }
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// usages <FQN>::<MEMBER>
// ---------------------------------------------------------------------------

/// `usages 'Class::MEMBER'` when the class part names a PHP class — written with a namespace,
/// or a short name a PHP class in the index has. `Ok(false)`, with nothing printed, otherwise:
/// Ruby and C++ names are qualified with `::` too.
pub fn try_member_usages(
    root: &Path,
    spec: &str,
    limit: usize,
    no_tests: bool,
    format: &str,
) -> Result<bool> {
    let Some((class_part, member)) = spec.rsplit_once("::") else {
        return Ok(false);
    };
    let (class_part, member) = (class_part.trim(), member.trim());
    if member.is_empty() || class_part.is_empty() {
        return Ok(false);
    }
    let qualified = class_part.contains('\\');
    if !qualified && !crate::db::db_exists(root) {
        return Ok(false);
    }
    let _lease = crate::db::acquire_project_lease(root)?;
    let conn = if qualified {
        open(root)?
    } else {
        let conn = crate::db::open_db_leased(root)?;
        if !crate::db::has_php_class_named(&conn, class_part)? {
            return Ok(false);
        }
        ensure_php_names(&conn)?;
        conn
    };
    member_usages(&conn, root, class_part, member, limit, no_tests, format)?;
    Ok(true)
}

#[derive(Serialize)]
struct MemberUsage {
    path: String,
    line: usize,
    context: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    test: bool,
    /// Inside the declaring class itself (`self::CASE` in a `match`, say).
    #[serde(rename = "self", skip_serializing_if = "std::ops::Not::not")]
    inside: bool,
}

/// The class that declares `member` for `class`: itself, or the nearest parent, interface or
/// trait that does.
fn declaring_class(classes: &IndexClasses, class: &str, member: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let mut queue = vec![class.to_string()];
    while let Some(current) = queue.pop() {
        if !seen.insert(current.to_ascii_lowercase()) {
            continue;
        }
        let model = classes.class(&current)?;
        if model.constants.iter().any(|c| c.name == member) || model.cases.iter().any(|c| c.name == member) {
            return Some(model.fqn.clone());
        }
        queue.extend(model.traits.iter().cloned());
        queue.extend(model.interfaces.iter().cloned());
        queue.extend(model.parent.iter().cloned());
    }
    None
}

fn member_usages(
    conn: &rusqlite::Connection,
    root: &Path,
    class_part: &str,
    member: &str,
    limit: usize,
    no_tests: bool,
    format: &str,
) -> Result<()> {
    let asked = resolve_one_class(conn, class_part)?;
    let classes = IndexClasses::new(conn, root);
    let fqn = declaring_class(&classes, &asked, member).unwrap_or_else(|| asked.clone());
    let model = classes.class(&fqn);
    let declared = model.as_ref().and_then(|m| {
        m.cases
            .iter()
            .map(|c| ("case", c))
            .chain(m.constants.iter().map(|c| ("constant", c)))
            .find(|(_, c)| c.name == member)
            .map(|(kind, c)| (kind, c.line))
    });
    let declared_path = classes.source(&fqn).map(|(p, _)| p);

    // The member is reachable through every subtype that does not declare its own.
    let owners: Vec<String> = with_subtypes(conn, std::slice::from_ref(&fqn))?
        .into_iter()
        .filter(|t| t.eq_ignore_ascii_case(&fqn) || declaring_class(&classes, t, member).is_some_and(|d| d.eq_ignore_ascii_case(&fqn)))
        .collect();
    let owner_set: HashSet<String> = owners.iter().map(|o| o.to_ascii_lowercase()).collect();

    // Files that reference an owner, and the owners' own files for `self::`/`parent::`.
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for owner in &owners {
        for file in crate::db::files_referencing_fqn(conn, owner)? {
            if seen.insert(file.path.clone()) {
                files.push(file);
            }
        }
    }
    let mut own_files: Vec<(String, std::rc::Rc<String>)> = Vec::new();
    for owner in &owners {
        if let Some((path, content)) = classes.source(owner) {
            if !own_files.iter().any(|(p, _)| *p == path) {
                own_files.push((path, content));
            }
        }
    }
    let mut sources: Vec<(String, std::rc::Rc<String>)> = Vec::new();
    for file in files {
        if let Some((path, content)) = classes.read_file(&file) {
            sources.push((path, content));
        }
    }
    for (path, content) in own_files {
        if !sources.iter().any(|(p, _)| *p == path) {
            sources.push((path, content));
        }
    }

    let mut items = Vec::new();
    let mut phpdoc = Vec::new();
    for (path, content) in sources {
        let test = is_test_path(&path);
        if no_tests && test {
            continue;
        }
        let Ok(tree) = php_ast::parse(&content) else {
            continue;
        };
        let root_node = tree.root_node();
        let names = Names::of(root_node, &content);
        let lines: Vec<&str> = content.lines().collect();
        let context = |line: usize| lines.get(line - 1).map(|l| l.trim().to_string()).unwrap_or_default();
        let mut stack = vec![root_node];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "class_constant_access_expression" => {
                    if let Some(inside) = names_member(node, &content, &names, &classes, &fqn, &owner_set, member) {
                        let line = php_ast::line(node);
                        items.push(MemberUsage {
                            path: path.clone(),
                            line,
                            context: context(line),
                            test,
                            inside,
                        });
                    }
                }
                "comment" => {
                    for (offset, text) in php_ast::text(node, &content).lines().enumerate() {
                        if mentions_member(text, &names, node.start_byte(), &owner_set, member) {
                            let line = php_ast::line(node) + offset;
                            phpdoc.push(MemberUsage {
                                path: path.clone(),
                                line,
                                context: context(line),
                                test,
                                inside: false,
                            });
                        }
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }
    items.sort_by(|a, b| (a.test, &a.path, a.line).cmp(&(b.test, &b.path, b.line)));
    phpdoc.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    let total = items.len();
    items.truncate(limit);
    let shown = format!("{fqn}::{member}");

    if format == "json" {
        return print_json(&json!({
            "member": shown,
            "kind": declared.map(|(k, _)| k),
            "declared_at": declared.and_then(|(_, line)| declared_path.as_ref().map(|p| format!("{p}:{line}"))),
            "subtypes": owners.iter().filter(|o| !o.eq_ignore_ascii_case(&fqn)).collect::<Vec<_>>(),
            "items": items,
            "phpdoc": phpdoc,
            "pagination": {"total": total, "returned": items.len(), "truncated": total > items.len(), "limit": limit},
        }));
    }
    println!(
        "{}",
        format!("Usages of {shown} (showing {} of {total}):", items.len()).bold()
    );
    if declared.is_none() {
        println!("  {} {fqn} declares no constant or case {member}", "note:".yellow());
    }
    for u in &items {
        let tags = match (u.test, u.inside) {
            (true, _) => "  [test]",
            (_, true) => "  [self]",
            _ => "",
        };
        println!("  {}:{}  {}{tags}", u.path.cyan(), u.line, u.context);
    }
    if !phpdoc.is_empty() {
        println!("  in PHPDoc:");
        for u in &phpdoc {
            println!("    {}:{}  {}", u.path.cyan(), u.line, u.context);
        }
    }
    if total > items.len() {
        println!("  … {} more; rerun with a larger --limit", total - items.len());
    }
    Ok(())
}

/// `Some(inside the declaring class)` when `node` (`X::NAME`) names `member` of `fqn`: through
/// `fqn` or a subtype that inherits it (`owners`), `self::`/`static::` in one of them, or
/// `parent::` in a class whose parent is one of them.
fn names_member(
    node: Node,
    src: &str,
    names: &Names,
    classes: &IndexClasses,
    fqn: &str,
    owners: &HashSet<String>,
    member: &str,
) -> Option<bool> {
    let parts = php_ast::named_children(node);
    let (scope, name) = (parts.first()?, parts.get(1)?);
    if php_ast::text(*name, src) != member {
        return None;
    }
    let written = php_ast::text(*scope, src);
    let in_class = php_model::enclosing_class(node, src, names);
    let inside = in_class.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(fqn));
    let owned = |class: &str| owners.contains(&class.to_ascii_lowercase());
    let through = match (scope.kind(), written.to_ascii_lowercase().as_str()) {
        ("relative_scope", "self" | "static") => in_class.clone(),
        ("relative_scope", "parent") => in_class.as_deref().and_then(|c| classes.class(c)).and_then(|m| m.parent.clone()),
        ("name" | "qualified_name" | "relative_name", _) => Some(names.resolve_class(written, scope.start_byte())),
        _ => None,
    }?;
    owned(&through).then_some(inside)
}

/// A comment line naming `Class::MEMBER` with `Class` resolving to one of `owners`.
fn mentions_member(line: &str, names: &Names, at: usize, owners: &HashSet<String>, member: &str) -> bool {
    let needle = format!("::{member}");
    line.match_indices(&needle).any(|(pos, _)| {
        let after = line[pos + needle.len()..].chars().next();
        if after.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return false;
        }
        let class: String = line[..pos]
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '\\')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        !class.is_empty() && owners.contains(&names.resolve_class(&class, at).to_ascii_lowercase())
    })
}
