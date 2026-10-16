//! `new '<FQN>'`: every `new Foo(...)` of a PHP class with its arguments folded to values —
//! strings, numbers, arrays, `Bar::class`, constants, nested `new`, a variable assigned once
//! before. A registry built from objects (`new Descriptor(name: 'load', …)`) reads as data.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use serde_json::Value as Json;
use tree_sitter::Node;

use super::php::{is_test_path, open, print_json, resolve_one_class};
use super::under_path_prefix;
use crate::db::IndexedFile;
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
    let class = resolve_one_class(&conn, fqn)?;
    let mut by_file: BTreeMap<String, (IndexedFile, HashSet<usize>)> = BTreeMap::new();
    for (file, line) in crate::db::new_sites(&conn, &class)? {
        by_file
            .entry(file.path.clone())
            .or_insert_with(|| (file, HashSet::new()))
            .1
            .insert(line as usize);
    }
    let classes = IndexClasses::new(&conn, root);
    let mut found = Vec::new();
    for (file, lines) in by_file.into_values() {
        if !under_path_prefix(&file.path, path_prefix) || (no_tests && is_test_path(&file.path)) {
            continue;
        }
        let Some((path, content)) = classes.read_file(&file) else {
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
        if matches!(n.kind(), "method_declaration" | "function_definition") {
            method = n
                .child_by_field_name("name")
                .map(|name| php_ast::text(name, src).to_string());
            break;
        }
        current = n.parent();
    }
    (php_model::enclosing_class(node, src, names), method)
}
