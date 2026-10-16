//! `new '<FQN>'`: every `new Foo(...)` of a PHP class with its arguments folded to values —
//! strings, numbers, arrays, `Bar::class`, constants, nested `new`, a variable assigned once
//! before. A registry built from objects (`new Descriptor(name: 'load', …)`) reads as data.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{bail, Result};
use colored::Colorize;
use rusqlite::params;
use serde::Serialize;
use serde_json::Value as Json;
use tree_sitter::Node;

use super::php::{is_test_path, normalize_fqn, not_defined, open, print_json, type_fqns};
use super::php_classes::IndexClasses;
use crate::parsers::php_ast::{self, Names};
use crate::parsers::php_eval::{Evaluator, Val};
use crate::parsers::php_model;

#[derive(Serialize)]
struct Instantiation {
    path: String,
    line: usize,
    end_line: usize,
    /// `Class::method` (or the class, or `null` at file level) the `new` is written in.
    #[serde(rename = "in")]
    inside: Option<String>,
    args: Vec<Json>,
    named: serde_json::Map<String, Json>,
}

pub fn cmd_new(
    root: &Path,
    fqn: &str,
    path_prefix: Option<&str>,
    no_tests: bool,
    limit: usize,
    format: &str,
) -> Result<()> {
    let _lease = crate::db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let wanted = normalize_fqn(fqn);
    let class = match type_fqns(&conn, &wanted)?.as_slice() {
        [] => bail!("{}", not_defined(&conn, "class ", &wanted)),
        [only] => only.clone(),
        many => bail!(
            "{} classes are named {wanted}; pass one FQN: {}",
            many.len(),
            many.join(", ")
        ),
    };
    let mut stmt = conn.prepare(
        "SELECT f.path, f.root_path, r.line FROM refs r JOIN files f ON f.id = r.file_id
         WHERE r.fqn = ?1 COLLATE NOCASE AND r.ref_kind = 'new' AND f.external = 0
         ORDER BY f.path, r.line",
    )?;
    let rows = stmt
        .query_map(params![class], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut by_file: BTreeMap<(String, Option<String>), HashSet<usize>> = BTreeMap::new();
    for (path, root_path, line) in rows {
        by_file
            .entry((path, root_path))
            .or_default()
            .insert(line as usize);
    }
    let prefix = path_prefix.map(|p| p.trim_start_matches("./").trim_end_matches('/').to_string());
    let classes = IndexClasses::new(&conn, root);
    let mut found = Vec::new();
    for ((path, root_path), lines) in by_file {
        if prefix
            .as_deref()
            .is_some_and(|p| !(path == p || path.starts_with(&format!("{p}/"))))
        {
            continue;
        }
        if no_tests && is_test_path(&path) {
            continue;
        }
        let base = match root_path.as_deref() {
            Some(r) if !r.is_empty() => Path::new(r).to_path_buf(),
            _ => root.to_path_buf(),
        };
        let Some(content) = classes.read(&base.join(&path)) else {
            continue;
        };
        let Ok(tree) = php_ast::parse(&content) else {
            continue;
        };
        let root_node = tree.root_node();
        let names = Names::of(root_node, &content);
        let mut stack = vec![root_node];
        while let Some(node) = stack.pop() {
            if node.kind() == "object_creation_expression" && lines.contains(&php_ast::line(node)) {
                if let Some(item) = instantiation(node, &content, &names, &classes, &class, &path) {
                    found.push(item);
                }
            }
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }
    let total = found.len();
    found.truncate(limit);

    if format == "json" {
        return print_json(&serde_json::json!({
            "class": class,
            "items": found,
            "pagination": {"total": total, "returned": found.len(), "truncated": total > found.len(), "limit": limit},
        }));
    }
    println!("{}", format!("new {class}: {total}").bold());
    for item in &found {
        let mut parts: Vec<String> = item.args.iter().map(|a| a.to_string()).collect();
        parts.extend(item.named.iter().map(|(k, v)| format!("{k}: {v}")));
        println!(
            "  {}:{}  {}  ({})",
            item.path.cyan(),
            item.line,
            item.inside.as_deref().unwrap_or("-"),
            parts.join(", ")
        );
    }
    if total > found.len() {
        println!("  … {} more; rerun with a larger --limit", total - found.len());
    }
    Ok(())
}

fn instantiation(
    node: Node,
    src: &str,
    names: &Names,
    classes: &IndexClasses,
    class: &str,
    path: &str,
) -> Option<Instantiation> {
    let (enclosing_class, enclosing_method) = enclosing(node, src, names);
    let evaluator = Evaluator {
        class: enclosing_class.clone(),
        ..Evaluator::new(src, names, classes)
    };
    let Val::New {
        class: created,
        args,
        named,
    } = evaluator.eval(node)
    else {
        return None;
    };
    if !created.eq_ignore_ascii_case(class) {
        return None;
    }
    let inside = match (enclosing_class, enclosing_method) {
        (Some(c), Some(m)) => Some(format!("{c}::{m}")),
        (Some(c), None) => Some(c),
        (None, Some(f)) => Some(f),
        (None, None) => None,
    };
    Some(Instantiation {
        path: path.to_string(),
        line: php_ast::line(node),
        end_line: php_ast::end_line(node),
        inside,
        args: args.iter().map(Val::to_json).collect(),
        named: named.iter().map(|(k, v)| (k.clone(), v.to_json())).collect(),
    })
}

/// The class and the method (or function) a node is written in.
fn enclosing(node: Node, src: &str, names: &Names) -> (Option<String>, Option<String>) {
    let mut method = None;
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "method_declaration" | "function_definition" if method.is_none() => {
                method = n
                    .child_by_field_name("name")
                    .map(|name| php_ast::text(name, src).to_string());
            }
            kind if php_ast::CLASS_LIKE_KINDS.contains(&kind) => {
                return (php_model::declared_fqn(n, src, names), method);
            }
            _ => {}
        }
        current = n.parent();
    }
    (None, method)
}
