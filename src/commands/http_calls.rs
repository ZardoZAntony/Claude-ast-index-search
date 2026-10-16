//! `http-calls`: backend calls made by the frontend — JS/TS/Vue modules and the inline scripts
//! of PHP templates — with method, address, component action, keys sent and keys read from the
//! answer; and the `window.X` globals a template writes and scripts read.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use colored::Colorize;
use regex::Regex;
use rusqlite::params;
use serde::Serialize;

use super::js::{modules_with_imports, ModuleImports};
use super::php::{print_json, read_lossy};
use crate::db;
use crate::parsers::js_http::{self, HttpCall, Imports, NoImports, ScriptFacts, Wrapper};
use crate::parsers::php_ast;
use crate::parsers::php_eval::{Evaluator, NoConstants, Val};
use crate::parsers::typescript::extract_vue_script;

#[derive(Serialize)]
struct CallEntry {
    path: String,
    line: usize,
    client: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
    method: Option<String>,
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url_expr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    params: Vec<String>,
    reads: Vec<String>,
    response_keys: Vec<String>,
}

#[derive(Serialize)]
struct LiteralEntry {
    path: String,
    line: usize,
    url: String,
}

#[derive(Serialize)]
struct GlobalWriteEntry {
    path: String,
    line: usize,
    keys: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expr: Option<String>,
}

#[derive(Serialize)]
struct GlobalReadEntry {
    path: String,
    line: usize,
    key: String,
}

#[derive(Serialize, Default)]
struct GlobalEntry {
    writes: Vec<GlobalWriteEntry>,
    reads: Vec<GlobalReadEntry>,
}

fn skipped(path: &str) -> bool {
    path.contains("/dist/") || path.starts_with("dist/") || path.ends_with(".min.js") || path.contains("node_modules/")
}

/// One script to analyze: a JS module or a PHP template's inline scripts.
struct Source {
    path: String,
    script: String,
    placeholders: Vec<String>,
    /// The template's content, for the PHP side of `window.X = <?= … ?>`.
    template: Option<String>,
    module: Option<usize>,
}

struct ModuleLookup<'a> {
    imports: &'a [(Option<String>, Option<String>, Option<usize>)],
    constants: &'a [HashMap<String, String>],
    wrappers: &'a [Vec<Wrapper>],
}

impl ModuleLookup<'_> {
    fn import_of(&self, local: &str) -> Option<(&str, usize)> {
        self.imports.iter().find_map(|(l, imported, target)| {
            (l.as_deref() == Some(local)).then_some(())?;
            Some((imported.as_deref()?, (*target)?))
        })
    }
}

impl Imports for ModuleLookup<'_> {
    fn constant(&self, local: &str) -> Option<String> {
        let (imported, target) = self.import_of(local)?;
        self.constants.get(target)?.get(imported).cloned()
    }

    fn wrapper(&self, local: &str, member: Option<&str>) -> Option<Wrapper> {
        let (imported, target) = self.import_of(local)?;
        let wrappers = self.wrappers.get(target)?;
        let find = |name: &str| wrappers.iter().find(|w| w.name == name).cloned();
        match (imported, member) {
            ("default", Some(m)) => find(&format!("default.{m}")).or_else(|| find("default").filter(|w| w.instance)),
            ("default", None) => find("default").filter(|w| !w.instance),
            ("*", Some(m)) => find(m),
            (name, None) => find(name).filter(|w| !w.instance),
            (name, Some(_)) => find(name).filter(|w| w.instance),
        }
    }
}

pub fn cmd_http_calls(root: &Path, path_prefix: Option<&str>, format: &str) -> Result<()> {
    if !db::db_exists(root) {
        println!("{}", "Index not found. Run 'ast-index rebuild' first.".red());
        return Ok(());
    }
    let _lease = db::acquire_project_lease(root)?;
    let conn = db::open_db_leased(root)?;
    let modules: Vec<ModuleImports> = modules_with_imports(&conn, root)?;

    let mut sources: Vec<Source> = Vec::new();
    for (i, module) in modules.iter().enumerate() {
        if skipped(&module.path) {
            continue;
        }
        let Some(content) = read_lossy(&module.abs) else {
            continue;
        };
        let script = if module.path.ends_with(".vue") {
            extract_vue_script(&content)
        } else {
            content
        };
        sources.push(Source {
            path: module.path.clone(),
            script,
            placeholders: Vec::new(),
            template: None,
            module: Some(i),
        });
    }
    let mut stmt = conn.prepare(
        "SELECT path, root_path FROM files WHERE external = 0 AND path LIKE '%.php'
           AND (path LIKE '%/templates/%' OR path LIKE 'templates/%') ORDER BY path",
    )?;
    let templates = stmt
        .query_map(params![], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    for (path, root_path) in templates {
        let base = match root_path.as_deref() {
            Some(r) if !r.is_empty() => Path::new(r).to_path_buf(),
            _ => root.to_path_buf(),
        };
        let Some(content) = read_lossy(&base.join(&path)) else {
            continue;
        };
        if let Some((script, placeholders)) = js_http::template_scripts(&content) {
            sources.push(Source {
                path,
                script,
                placeholders,
                template: Some(content),
                module: None,
            });
        }
    }

    // Pass 1: what modules export for others (wrappers, constants) and the globals written.
    let mut constants: Vec<HashMap<String, String>> = vec![HashMap::new(); modules.len()];
    let mut wrappers: Vec<Vec<Wrapper>> = vec![Vec::new(); modules.len()];
    let mut globals: HashSet<String> = HashSet::new();
    for source in &sources {
        // Only exports and `window.X =` matter here.
        if !source.script.contains("export") && !source.script.contains("window.") {
            continue;
        }
        let facts = js_http::analyze(&source.script, &NoImports, &source.placeholders, &HashSet::new());
        if let Some(i) = source.module {
            constants[i] = facts.constants.into_iter().collect();
            wrappers[i] = facts.wrappers;
        }
        globals.extend(facts.global_writes.into_iter().map(|w| w.name));
    }

    // Pass 2: calls with imports resolved, reads of the globals.
    let prefix = path_prefix.map(|p| p.trim_start_matches("./").trim_end_matches('/').to_string());
    let mut calls = Vec::new();
    let mut literals = Vec::new();
    let mut by_global: BTreeMap<String, GlobalEntry> = BTreeMap::new();
    for source in &sources {
        let in_scope = prefix
            .as_deref()
            .is_none_or(|p| source.path == p || source.path.starts_with(&format!("{p}/")));
        let facts: ScriptFacts = match source.module {
            Some(i) => {
                let lookup = ModuleLookup {
                    imports: &modules[i].imports,
                    constants: &constants,
                    wrappers: &wrappers,
                };
                js_http::analyze(&source.script, &lookup, &source.placeholders, &globals)
            }
            None => js_http::analyze(&source.script, &NoImports, &source.placeholders, &globals),
        };
        for write in facts.global_writes {
            let keys = match (&write.keys, &source.template, &write.expr) {
                (Some(k), _, _) => Some(k.clone()),
                (None, Some(template), Some(expr)) => php_keys(template, expr),
                _ => None,
            };
            by_global.entry(write.name.clone()).or_default().writes.push(GlobalWriteEntry {
                path: source.path.clone(),
                line: write.line,
                keys,
                expr: write.expr,
            });
        }
        for read in facts.global_reads {
            by_global.entry(read.name.clone()).or_default().reads.push(GlobalReadEntry {
                path: source.path.clone(),
                line: read.line,
                key: read.path,
            });
        }
        if !in_scope {
            continue;
        }
        for call in facts.calls {
            calls.push(entry(&source.path, call));
        }
        for literal in facts.literals {
            literals.push(LiteralEntry {
                path: source.path.clone(),
                line: literal.line,
                url: literal.url,
            });
        }
    }
    calls.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    literals.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));

    if format == "json" {
        return print_json(&serde_json::json!({
            "calls": calls,
            "literals": literals,
            "globals": by_global,
        }));
    }
    for c in &calls {
        let target = match (&c.component, &c.action) {
            (Some(comp), Some(action)) => format!("{comp} · {action}"),
            (None, Some(action)) => action.clone(),
            _ => c
                .url
                .clone()
                .or_else(|| c.url_expr.as_ref().map(|e| format!("{{{e}}}")))
                .unwrap_or_default(),
        };
        let mut notes = vec![c.client.to_string()];
        if let Some(via) = &c.via {
            notes.push(format!("via {via}"));
        }
        if !c.params.is_empty() {
            notes.push(format!("sends {}", c.params.join(", ")));
        }
        if !c.response_keys.is_empty() {
            notes.push(format!("reads {}", c.response_keys.join(", ")));
        }
        println!(
            "{} {}  [{}]  {}",
            c.method.as_deref().unwrap_or("?").bold(),
            target.yellow(),
            notes.join("; "),
            format!("{}:{}", c.path, c.line).dimmed()
        );
    }
    if !literals.is_empty() {
        println!("addresses outside recognised calls:");
        for l in &literals {
            println!("  {}  {}", l.url, format!("{}:{}", l.path, l.line).dimmed());
        }
    }
    for (name, g) in &by_global {
        println!(
            "window.{name}: {} write(s), {} read(s)",
            g.writes.len(),
            g.reads.len()
        );
    }
    if calls.is_empty() {
        println!("No backend calls found.");
    }
    Ok(())
}

fn entry(path: &str, c: HttpCall) -> CallEntry {
    CallEntry {
        path: path.to_string(),
        line: c.line,
        client: c.client,
        via: c.via,
        method: c.method,
        url: c.url,
        url_expr: c.url_expr,
        component: c.component,
        action: c.action,
        mode: c.mode,
        params: c.params,
        reads: c.reads,
        response_keys: c.response_keys,
    }
}

static ENCODER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)^\{php:\s*(?:\\?(?:[\w\\]*\\)?Json::encode|json_encode|\\?CUtil::PhpToJSObject)\s*\((.*)\)\s*\}$")
        .expect("valid regex")
});

/// Keys of the array a template serialises into `window.X = <?= Json::encode(…) ?>`, when the
/// array is visible in the template: a literal, or a variable assigned one literal.
fn php_keys(template: &str, expr: &str) -> Option<Vec<String>> {
    let inner = ENCODER.captures(expr)?.get(1)?.as_str();
    let first = first_argument(inner).trim();
    let snippet = format!("<?php return {first};");
    let tree = php_ast::parse(&snippet).ok()?;
    let root = tree.root_node();
    let names = php_ast::Names::of(root, &snippet);
    let value_node = {
        let mut stack = vec![root];
        let mut found = None;
        while let Some(n) = stack.pop() {
            if n.kind() == "return_statement" {
                found = php_ast::named_children(n).into_iter().next();
                break;
            }
            stack.extend(php_ast::named_children(n));
        }
        found?
    };
    match value_node.kind() {
        "array_creation_expression" => map_keys(Evaluator::new(&snippet, &names, &NoConstants).eval(value_node)),
        "variable_name" => {
            let var = php_ast::text(value_node, &snippet).to_string();
            let tree = php_ast::parse(template).ok()?;
            let root = tree.root_node();
            let names = php_ast::Names::of(root, template);
            let mut assigned = Vec::new();
            let mut stack = vec![root];
            while let Some(n) = stack.pop() {
                if n.kind() == "assignment_expression"
                    && n.child_by_field_name("left").is_some_and(|l| php_ast::text(l, template) == var)
                {
                    assigned.push(n);
                }
                stack.extend(php_ast::named_children(n));
            }
            let [only] = assigned.as_slice() else {
                return None;
            };
            let right = only.child_by_field_name("right")?;
            (right.kind() == "array_creation_expression")
                .then(|| map_keys(Evaluator::new(template, &names, &NoConstants).eval(right)))
                .flatten()
        }
        _ => None,
    }
}

fn map_keys(value: Val) -> Option<Vec<String>> {
    match value {
        Val::Map(pairs) => Some(
            pairs
                .into_iter()
                .filter_map(|(k, _)| match k {
                    Val::Str(s) => Some(s),
                    Val::Int(i) => Some(i.to_string()),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// The first argument of an argument list, split at a top-level comma.
fn first_argument(args: &str) -> &str {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (i, c) in args.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ',') if depth == 0 => return &args[..i],
            _ => {}
        }
    }
    args
}
