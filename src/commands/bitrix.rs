//! Bitrix wiring read from the code: event subscriptions (`events`).

use std::path::Path;

use anyhow::Result;
use colored::Colorize;
use rusqlite::params;
use serde::Serialize;
use serde_json::Value as Json;

use super::php::{is_test_path, open, print_json};
use super::php_classes::IndexClasses;
use crate::parsers::php_calls::{self, Classes};
use crate::parsers::php_eval::Val;
use crate::parsers::php_events::{self, Handler};

const REGISTERING_CALLS: &str = "('addEventHandler', 'addEventHandlerCompatible', 'registerEventHandler', \
     'registerEventHandlerCompatible', 'RegisterModuleDependences')";

#[derive(Serialize)]
struct EventEntry {
    path: String,
    line: usize,
    /// The registering call: `addEventHandler`, `AddEventHandler`, `registerEventHandler`…
    call: String,
    /// `null` when the module id is computed at run time (see `module_expr`).
    module: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    module_expr: Option<String>,
    events: Vec<String>,
    /// Event names computed at run time, as written.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    events_unresolved: Vec<String>,
    /// `Class::method`, a function name, or `null` for a closure or a callback built at run time.
    handler: Option<String>,
    /// `method`, `function`, `closure` or `dynamic`.
    handler_kind: &'static str,
    /// `path:start-end` of the handler's body.
    #[serde(skip_serializing_if = "Option::is_none")]
    handler_at: Option<String>,
    /// The class declaring the handler method when it is inherited.
    #[serde(skip_serializing_if = "Option::is_none")]
    handler_declared_in: Option<String>,
    /// A dynamic callback as written.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    /// `registerEventHandler`: the module whose class handles the event.
    #[serde(skip_serializing_if = "Option::is_none")]
    to_module: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sort: Option<Json>,
}

fn string_or_expr(value: &Val) -> (Option<String>, Option<String>) {
    match value {
        Val::Str(s) => (Some(s.clone()), None),
        Val::Int(i) => (Some(i.to_string()), None),
        Val::Expr(text) => (None, Some(text.clone())),
        other => (None, Some(other.to_json().to_string())),
    }
}

pub fn cmd_events(
    root: &Path,
    module: Option<&str>,
    event: Option<&str>,
    no_tests: bool,
    format: &str,
) -> Result<()> {
    let _lease = crate::db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let sql = format!(
        "SELECT DISTINCT f.path, f.root_path FROM refs r JOIN files f ON f.id = r.file_id
         WHERE r.name COLLATE NOCASE IN {REGISTERING_CALLS} AND f.external = 0
           AND (f.path LIKE '%.php' OR f.path LIKE '%.phtml')
         ORDER BY f.path"
    );
    let mut stmt = conn.prepare(&sql)?;
    let files = stmt
        .query_map(params![], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let prefix = module.map(|m| m.trim_start_matches("./").trim_end_matches('/').to_string());
    let event_filter = event.map(str::to_ascii_lowercase);
    let classes = IndexClasses::new(&conn, root);

    let mut entries = Vec::new();
    for (path, root_path) in files {
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
        let Ok(found) = php_events::registrations(&content, &classes) else {
            continue;
        };
        for registration in found {
            let entry = entry(&classes, &path, registration);
            let wanted = event_filter.as_deref().is_none_or(|e| {
                entry
                    .events
                    .iter()
                    .chain(entry.events_unresolved.iter())
                    .any(|name| name.to_ascii_lowercase().contains(e))
            });
            if wanted {
                entries.push(entry);
            }
        }
    }

    if format == "json" {
        return print_json(&entries);
    }
    let mut last = String::new();
    for e in &entries {
        if e.path != last {
            println!("{}", e.path.cyan());
            last = e.path.clone();
        }
        let module = e
            .module
            .as_deref()
            .map(|m| if m.is_empty() { "''".to_string() } else { m.to_string() })
            .or_else(|| e.module_expr.clone())
            .unwrap_or_default();
        let mut events: Vec<String> = e.events.clone();
        events.extend(e.events_unresolved.iter().map(|x| format!("{{{x}}}")));
        let handler = e
            .handler
            .clone()
            .or_else(|| e.source.clone())
            .unwrap_or_else(|| e.handler_kind.to_string());
        let at = e.handler_at.as_deref().map(|a| format!("  {a}")).unwrap_or_default();
        println!(
            "  :{} {} {}  {} -> {}{}",
            e.line,
            e.call,
            module,
            events.join(", "),
            handler.yellow(),
            at.dimmed()
        );
    }
    if entries.is_empty() {
        println!("No event subscriptions found.");
    }
    Ok(())
}

fn entry(classes: &IndexClasses, path: &str, r: php_events::Registration) -> EventEntry {
    let (module, module_expr) = string_or_expr(&r.module);
    let mut events = Vec::new();
    let mut events_unresolved = Vec::new();
    for e in &r.events {
        match string_or_expr(e) {
            (Some(name), _) => events.push(name),
            (None, Some(expr)) => events_unresolved.push(expr),
            _ => {}
        }
    }
    let (handler, handler_kind, handler_at, handler_declared_in, source) = match &r.handler {
        Handler::Method { class, method } => {
            let found = php_calls::find_method(classes, class, method);
            let at = found.as_ref().and_then(|(declaring, m)| {
                classes
                    .source(&declaring.fqn)
                    .map(|(p, _)| format!("{p}:{}-{}", m.line, m.end_line))
            });
            let declared_in = found
                .as_ref()
                .filter(|(d, _)| !d.fqn.eq_ignore_ascii_case(class))
                .map(|(d, _)| d.fqn.clone());
            // The class as declared, when the code spells it in another case.
            let class = classes
                .class(class)
                .map(|m| m.fqn.clone())
                .unwrap_or_else(|| class.clone());
            (Some(format!("{class}::{method}")), "method", at, declared_in, None)
        }
        Handler::Function(name) => (
            Some(name.clone()),
            "function",
            classes.function_at(name),
            None,
            None,
        ),
        Handler::Closure { line, end_line } => (
            None,
            "closure",
            Some(format!("{path}:{line}-{end_line}")),
            None,
            None,
        ),
        Handler::Dynamic(text) => (None, "dynamic", None, None, Some(text.clone())),
    };
    let (to_module, _) = r
        .to_module
        .as_ref()
        .map(string_or_expr)
        .unwrap_or((None, None));
    EventEntry {
        path: path.to_string(),
        line: r.line,
        call: r.call,
        module,
        module_expr,
        events,
        events_unresolved,
        handler,
        handler_kind,
        handler_at,
        handler_declared_in,
        source,
        to_module,
        sort: r.sort.as_ref().filter(|s| s.is_known()).map(Val::to_json),
    }
}
