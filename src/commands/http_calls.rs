//! `http-calls`: backend calls made by the frontend — JS/TS/Vue modules and the inline scripts
//! of PHP templates — with method, address, component action, keys sent and keys read from the
//! answer; and the `window.X` globals a template writes and scripts read.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use colored::Colorize;
use regex::Regex;
use serde::Serialize;

use super::js::{modules_with_imports, ModuleImports};
use super::php::{locate, print_json, read_lossy};
use super::under_path_prefix;
use crate::db;
use crate::parsers::js_http::{self, Client, ExportedClient, HttpCall, Imports, NoImports, ScriptFacts, Wrapper};
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
    #[serde(skip)]
    module: Option<usize>,
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

/// Rounds of re-reading exports with imports resolved: the length of the longest chain of
/// exports built from imported ones that is followed.
const MAX_EXPORT_ROUNDS: usize = 4;

/// What each module exports for the others.
#[derive(Default, Clone, PartialEq)]
struct Exports {
    constants: Vec<(String, String)>,
    wrappers: Vec<Wrapper>,
    clients: Vec<ExportedClient>,
}

struct ModuleLookup<'a> {
    module: usize,
    modules: &'a [ModuleImports],
    exports: &'a [Exports],
}

impl ModuleLookup<'_> {
    fn import_of(&self, local: &str) -> Option<(&str, usize)> {
        self.modules[self.module].imports.iter().find_map(|i| {
            (!i.reexport && i.local.as_deref() == Some(local)).then_some(())?;
            Some((i.imported.as_deref()?, i.target?))
        })
    }

    /// `find` on the export `name` of `module`, through re-exports.
    fn exported<T>(&self, module: usize, name: &str, find: &dyn Fn(&Exports, usize, &str) -> Option<T>) -> Option<T> {
        self.exported_at(module, name, find, 0)
    }

    fn exported_at<T>(
        &self,
        module: usize,
        name: &str,
        find: &dyn Fn(&Exports, usize, &str) -> Option<T>,
        depth: usize,
    ) -> Option<T> {
        if let Some(found) = find(&self.exports[module], module, name) {
            return Some(found);
        }
        if depth > 5 {
            return None;
        }
        // `ENDPOINTS.list` re-exported as `ENDPOINTS`.
        let (head, rest) = match name.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (name, None),
        };
        for r in self.modules[module].imports.iter().filter(|i| i.reexport) {
            let Some(target) = r.target else {
                continue;
            };
            match (r.imported.as_deref(), r.local.as_deref()) {
                (Some("*"), None) => {
                    if let Some(found) = self.exported_at(target, name, find, depth + 1) {
                        return Some(found);
                    }
                }
                (Some(imported), Some(alias)) if alias == head && imported != "*" => {
                    let inner = match rest {
                        Some(rest) => format!("{imported}.{rest}"),
                        None => imported.to_string(),
                    };
                    return self.exported_at(target, &inner, find, depth + 1);
                }
                _ => {}
            }
        }
        None
    }

    /// The imported constant `local` (`NAME`, `NAME.key`) and the module that declares it.
    fn find_constant(&self, local: &str) -> Option<(usize, String)> {
        let (head, rest) = match local.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (local, None),
        };
        let (imported, target) = self.import_of(head)?;
        let name = match (imported, rest) {
            ("*", Some(rest)) => rest.to_string(),
            (_, Some(rest)) => format!("{imported}.{rest}"),
            (_, None) => imported.to_string(),
        };
        self.exported(target, &name, &|e, m, n| {
            e.constants.iter().find(|(k, _)| k == n).map(|(_, v)| (m, v.clone()))
        })
    }

    /// The values of the imported object constant `local`, with the module that declares it.
    fn find_constants_of(&self, local: &str) -> Vec<(usize, String)> {
        let Some((imported, target)) = self.import_of(local).filter(|(i, _)| *i != "*") else {
            return Vec::new();
        };
        self.exported(target, imported, &|e, m, n| {
            let prefix = format!("{n}.");
            let values: Vec<(usize, String)> = e
                .constants
                .iter()
                .filter(|(k, _)| k.strip_prefix(&prefix).is_some_and(|key| !key.contains('.')))
                .map(|(_, v)| (m, v.clone()))
                .collect();
            (!values.is_empty()).then_some(values)
        })
        .unwrap_or_default()
    }
}

impl Imports for ModuleLookup<'_> {
    fn constant(&self, local: &str) -> Option<String> {
        self.find_constant(local).map(|(_, v)| v)
    }

    fn constants_of(&self, local: &str) -> Vec<String> {
        self.find_constants_of(local).into_iter().map(|(_, v)| v).collect()
    }

    fn wrapper(&self, local: &str, member: Option<&str>) -> Option<Wrapper> {
        let (imported, target) = self.import_of(local)?;
        let find = |name: &str| {
            self.exported(target, name, &|e, _, n| e.wrappers.iter().find(|w| w.name == n).cloned())
        };
        match (imported, member) {
            ("default", Some(m)) => find(&format!("default.{m}")).or_else(|| find("default").filter(|w| w.instance)),
            ("default", None) => find("default").filter(|w| !w.instance),
            ("*", Some(m)) => find(m),
            (name, None) => find(name).filter(|w| !w.instance),
            (name, Some(_)) => find(name).filter(|w| w.instance),
        }
    }

    fn client(&self, local: &str, member: Option<&str>, called: bool) -> Option<Client> {
        let (imported, target) = self.import_of(local)?;
        let name = match (imported, member) {
            ("*", Some(m)) => m,
            (_, Some(_)) => return None,
            (name, None) => name,
        };
        self.exported(target, name, &|e, _, n| {
            e.clients
                .iter()
                .find(|c| c.name == n && c.factory == called)
                .map(|c| c.client.clone())
        })
    }
}

/// Module `t` changed its exports, or re-exports one that did: an index passes a change on
/// without changing itself.
fn passes_change(modules: &[ModuleImports], changed: &[bool], t: usize, depth: usize) -> bool {
    changed[t]
        || (depth < 5
            && modules[t]
                .imports
                .iter()
                .any(|i| i.reexport && i.target.is_some_and(|u| passes_change(modules, changed, u, depth + 1))))
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
    for file in db::php_template_files(&conn)? {
        let (path, abs) = locate(root, file.root_path.as_deref().unwrap_or(""), &file.path);
        let Some(content) = read_lossy(&abs) else {
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

    // Pass 1: what modules export for others (wrappers, clients, constants) and the globals
    // written.
    let mut exports: Vec<Exports> = vec![Exports::default(); modules.len()];
    let mut globals: HashSet<String> = HashSet::new();
    let mut exporting: Vec<&Source> = Vec::new();
    for source in &sources {
        // Only exports and `window.X =` matter here.
        if !source.script.contains("export") && !source.script.contains("window.") {
            continue;
        }
        let facts = js_http::exports(&source.script, &NoImports, &source.placeholders);
        if let Some(i) = source.module {
            exports[i] = Exports {
                constants: facts.constants,
                wrappers: facts.wrappers,
                clients: facts.clients,
            };
            if source.script.contains("export") {
                exporting.push(source);
            }
        }
        globals.extend(facts.global_writes.into_iter().map(|w| w.name));
    }
    // Again with imports resolved, for exports built from imported ones: a client made by an
    // imported factory (`export const http = createClient()`), a wrapper around that client, a
    // name an index re-exports. Each round reaches one link further along such a chain.
    let mut changed: Vec<bool> = exports.iter().map(|e| *e != Exports::default()).collect();
    for _ in 0..MAX_EXPORT_ROUNDS {
        let feeds = |t: usize| passes_change(&modules, &changed, t, 0);
        let mut rebuilt = Vec::new();
        for source in &exporting {
            let Some(i) = source.module else {
                continue;
            };
            if !modules[i].imports.iter().any(|imp| !imp.reexport && imp.target.is_some_and(feeds)) {
                continue;
            }
            let lookup = ModuleLookup {
                module: i,
                modules: &modules,
                exports: &exports,
            };
            let facts = js_http::exports(&source.script, &lookup, &source.placeholders);
            let found = Exports {
                constants: facts.constants,
                wrappers: facts.wrappers,
                clients: facts.clients,
            };
            if found != exports[i] {
                rebuilt.push((i, found));
            }
        }
        if rebuilt.is_empty() {
            break;
        }
        changed = vec![false; modules.len()];
        for (i, e) in rebuilt {
            changed[i] = true;
            exports[i] = e;
        }
    }

    // Pass 2: calls with imports resolved, reads of the globals.
    // Imported constants the calls' addresses use: (module, value), no addresses outside calls
    // there.
    let mut used: HashSet<(usize, String)> = HashSet::new();
    let mut calls = Vec::new();
    let mut literals = Vec::new();
    let mut by_global: BTreeMap<String, GlobalEntry> = BTreeMap::new();
    for source in &sources {
        let in_scope = under_path_prefix(&source.path, path_prefix);
        let facts: ScriptFacts = match source.module {
            Some(i) => {
                let lookup = ModuleLookup {
                    module: i,
                    modules: &modules,
                    exports: &exports,
                };
                let facts = js_http::analyze(&source.script, &lookup, &source.placeholders, &globals);
                for (name, all) in &facts.imported_constants {
                    if *all {
                        used.extend(lookup.find_constants_of(name));
                    } else {
                        used.extend(lookup.find_constant(name));
                    }
                }
                facts
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
                module: source.module,
            });
        }
    }
    literals.retain(|l| !l.module.is_some_and(|m| used.contains(&(m, l.url.clone()))));
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
