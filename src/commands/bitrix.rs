//! Bitrix wiring read from the code: event subscriptions (`events`), routes and component
//! actions with the prefilters in effect (`routes`).

use std::path::Path;

use anyhow::Result;
use colored::Colorize;
use rusqlite::params;
use serde::Serialize;
use serde_json::Value as Json;

use super::php::{is_test_path, open, print_json};
use super::php_classes::IndexClasses;
use crate::parsers::php_ast::{self, Names};
use crate::parsers::php_calls::{self, Classes};
use crate::parsers::php_eval::{Evaluator, Val};
use crate::parsers::php_events::{self, Handler};
use crate::parsers::php_model;
use crate::parsers::php_routes::RouteHandler;

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

// ---------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct Filter {
    /// Short class name: `LoggerFilter`.
    class: String,
    fqn: String,
    /// Where the filter comes from: the class declaring `getDefaultPreFilters()`,
    /// `configureActions`, or `Bitrix\Main\Engine\Controller` for the core defaults.
    from: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<Json>,
    /// Added by the core: an action allowing POST without a CSRF filter gets one for POST
    /// requests.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    implicit: bool,
}

#[derive(Serialize)]
struct ClassAttribute {
    attribute: String,
    from: String,
}

#[derive(Serialize)]
struct RouteEntry {
    /// `route` (a routing file) or `component` (an action of a component class).
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_expr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// `vendor:name` of a component under `local/components` or `bitrix/components`.
    #[serde(skip_serializing_if = "Option::is_none")]
    component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    component_path: Option<String>,
    /// Component action mode: `class` (class.php) or `ajax` (ajax.php).
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'static str>,
    file: String,
    line: usize,
    /// `Controller::xAction`, a closure, an action class.
    handler: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    handler_at: Option<String>,
    /// The action name Bitrix runs (the method without its `Action` suffix).
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attributes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    class_attributes: Vec<ClassAttribute>,
    /// The prefilters in effect, in order; `null` when they could not be read.
    prefilters: Option<Vec<Filter>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefilters_note: Option<String>,
}

const BITRIX_CONTROLLER: &str = "Bitrix\\Main\\Engine\\Controller";

fn short(fqn: &str) -> String {
    crate::parsers::php_ast::short_name(fqn).to_string()
}

/// `Bitrix\Main\Engine\Controller::getDefaultPreFilters()` as the core declares it.
fn core_default_prefilters() -> Vec<Filter> {
    let filter = |name: &str, args: Vec<Json>| Filter {
        class: name.to_string(),
        fqn: format!("Bitrix\\Main\\Engine\\ActionFilter\\{name}"),
        from: BITRIX_CONTROLLER.to_string(),
        args,
        implicit: false,
    };
    vec![
        filter("Authentication", Vec::new()),
        filter("HttpMethod", vec![serde_json::json!(["GET", "POST"])]),
        filter("Csrf", Vec::new()),
    ]
}

fn filter_of(value: &Val, from: &str) -> Option<Filter> {
    match value {
        Val::New { class, args, named } => {
            let mut all: Vec<Json> = args.iter().map(Val::to_json).collect();
            if !named.is_empty() {
                let map: serde_json::Map<String, Json> =
                    named.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
                all.push(Json::Object(map));
            }
            Some(Filter {
                class: short(class),
                fqn: class.clone(),
                from: from.to_string(),
                args: all,
                implicit: false,
            })
        }
        _ => None,
    }
}

/// The method node `name` of `class` (declared there), with the file it is in.
fn with_method<T>(
    classes: &IndexClasses,
    class: &str,
    name: &str,
    f: impl FnOnce(tree_sitter::Node, &str, &Names) -> T,
) -> Option<T> {
    let (_, content) = classes.source(class)?;
    let tree = php_ast::parse(&content).ok()?;
    let root = tree.root_node();
    let names = Names::of(root, &content);
    let class_node = php_ast::class_like_nodes(root).into_iter().find(|n| {
        php_model::declared_fqn(*n, &content, &names).is_some_and(|f| f.eq_ignore_ascii_case(class))
    })?;
    let method = class_node.child_by_field_name("body").and_then(|body| {
        php_ast::named_children(body).into_iter().find(|m| {
            m.kind() == "method_declaration"
                && m.child_by_field_name("name")
                    .is_some_and(|n| php_ast::text(n, &content).eq_ignore_ascii_case(name))
        })
    })?;
    Some(f(method, &content, &names))
}

/// The expression of the last `return` of a method body.
fn returned_expr(method: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let body = method.child_by_field_name("body")?;
    let mut last = None;
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        if n.kind() == "return_statement" {
            last = Some(n);
        }
        if n != body
            && matches!(n.kind(), "anonymous_function" | "arrow_function" | "function_definition")
        {
            continue;
        }
        stack.extend(php_ast::named_children(n));
    }
    php_ast::named_children(last?).into_iter().next()
}

/// Prefilters `getDefaultPreFilters()` returns for `class`: the first declaration up the
/// parents, `parent::getDefaultPreFilters()` inside it followed.
fn default_prefilters(classes: &IndexClasses, class: &str, depth: usize) -> Result<Vec<Filter>, String> {
    if depth > 8 {
        return Err("getDefaultPreFilters() calls nest too deep".into());
    }
    let Some((declaring, _)) = php_calls::find_method(classes, class, "getDefaultPreFilters") else {
        // The parents leave the index (the core is not indexed): Bitrix's own defaults.
        return Ok(core_default_prefilters());
    };
    let parent = declaring.parent.clone();
    with_method(classes, &declaring.fqn, "getDefaultPreFilters", |method, src, names| {
        let expr = returned_expr(method).ok_or("getDefaultPreFilters() returns nothing readable")?;
        let evaluator = Evaluator {
            class: Some(declaring.fqn.clone()),
            ..Evaluator::new(src, names, classes)
        };
        let parent_defaults = || match &parent {
            Some(p) => default_prefilters(classes, p, depth + 1),
            None => Ok(core_default_prefilters()),
        };
        let is_parent_call = |n: tree_sitter::Node| {
            let n = if n.kind() == "argument" || n.kind() == "variadic_unpacking" {
                php_ast::named_children(n).into_iter().last().unwrap_or(n)
            } else {
                n
            };
            n.kind() == "scoped_call_expression"
                && php_ast::text(n, src).to_ascii_lowercase().starts_with("parent::getdefaultprefilters")
        };
        let mut filters = Vec::new();
        let mut take = |n: tree_sitter::Node| -> Result<(), String> {
            if is_parent_call(n) {
                filters.extend(parent_defaults()?);
                return Ok(());
            }
            match evaluator.eval(n) {
                Val::List(items) => {
                    for item in items {
                        filters.push(filter_of(&item, &declaring.fqn).ok_or_else(|| {
                            format!("a prefilter is not a `new` expression: {}", item.to_json())
                        })?);
                    }
                    Ok(())
                }
                other => Err(format!("prefilters not readable: {}", other.to_json())),
            }
        };
        match expr.kind() {
            "array_creation_expression" => {
                for element in php_ast::named_children(expr) {
                    let inner = php_ast::named_children(element);
                    match inner.first() {
                        Some(spread) if spread.kind() == "variadic_unpacking" || php_ast::has_child_kind(element, "...") => {
                            let target = php_ast::named_children(*spread).into_iter().last().unwrap_or(*spread);
                            if is_parent_call(target) {
                                filters.extend(parent_defaults()?);
                            } else {
                                return Err(format!("spread of {}", php_ast::text(target, src)));
                            }
                        }
                        Some(value) => match evaluator.eval(*value) {
                            v @ Val::New { .. } => filters.push(filter_of(&v, &declaring.fqn).unwrap()),
                            other => return Err(format!("a prefilter is not a `new` expression: {}", other.to_json())),
                        },
                        None => {}
                    }
                }
                Ok(filters)
            }
            "function_call_expression"
                if expr
                    .child_by_field_name("function")
                    .is_some_and(|f| php_ast::text(f, src).eq_ignore_ascii_case("array_merge")) =>
            {
                for argument in expr
                    .child_by_field_name("arguments")
                    .map(php_ast::named_children)
                    .unwrap_or_default()
                {
                    take(argument)?;
                }
                Ok(filters)
            }
            _ if is_parent_call(expr) => parent_defaults(),
            _ => Err(format!(
                "getDefaultPreFilters() returns {}",
                php_ast::render(expr, src, None)
            )),
        }
    })
    .unwrap_or_else(|| Ok(core_default_prefilters()))
}

/// `configureActions()` of `class` as data: lowercased action name → its configuration.
fn configured_actions(classes: &IndexClasses, class: &str) -> Result<Vec<(String, Val)>, String> {
    let Some((declaring, _)) = php_calls::find_method(classes, class, "configureActions") else {
        return Ok(Vec::new());
    };
    with_method(classes, &declaring.fqn, "configureActions", |method, src, names| {
        let Some(expr) = returned_expr(method) else {
            return Ok(Vec::new());
        };
        let evaluator = Evaluator {
            class: Some(class.to_string()),
            ..Evaluator::new(src, names, classes)
        };
        match evaluator.eval(expr) {
            Val::Map(pairs) => Ok(pairs
                .into_iter()
                .filter_map(|(k, v)| k.as_str().map(|k| (k.to_ascii_lowercase(), v)))
                .collect()),
            Val::List(items) if items.is_empty() => Ok(Vec::new()),
            other => Err(format!(
                "configureActions() of {} is computed: {}",
                declaring.fqn,
                match other {
                    Val::Expr(text) => text,
                    v => v.to_json().to_string(),
                }
            )),
        }
    })
    .unwrap_or_else(|| Ok(Vec::new()))
}

/// The prefilters Bitrix runs for `action` of `class`.
fn prefilters_for(
    classes: &IndexClasses,
    class: &str,
    action: &str,
    component_class: bool,
) -> (Option<Vec<Filter>>, Option<String>) {
    let configured = match configured_actions(classes, class) {
        Ok(c) => c,
        Err(note) => return (None, Some(note)),
    };
    let config = configured
        .iter()
        .find(|(k, _)| *k == action.to_ascii_lowercase())
        .map(|(_, v)| v.clone());
    let entry = |key: &str| -> Option<Val> {
        match &config {
            Some(Val::Map(pairs)) => pairs
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v.clone()),
            _ => None,
        }
    };
    let as_filters = |value: Val| -> Result<Vec<Filter>, String> {
        match value {
            Val::List(items) => items
                .iter()
                .map(|i| filter_of(i, "configureActions").ok_or_else(|| format!("{}", i.to_json())))
                .collect(),
            other => Err(format!("{}", other.to_json())),
        }
    };
    let mut filters = match entry("prefilters") {
        Some(listed) => match as_filters(listed) {
            Ok(f) => f,
            Err(note) => return (None, Some(format!("configureActions() prefilters: {note}"))),
        },
        None if component_class => core_default_prefilters(),
        None => match default_prefilters(classes, class, 0) {
            Ok(f) => f,
            Err(note) => return (None, Some(note)),
        },
    };
    // The order of `Controller::buildFilters()`: the implicit CSRF check joins the base list,
    // then `-prefilters` are removed, then `+prefilters` appended.
    let allows_post = filters.iter().any(|f| {
        f.class.eq_ignore_ascii_case("HttpMethod")
            // `"POST"`, or `HttpMethod::METHOD_POST` left unfolded when the core is not indexed.
            && f.args.first().is_some_and(|a| a.to_string().to_ascii_uppercase().contains("POST"))
    });
    let has_csrf = filters.iter().any(|f| f.class.eq_ignore_ascii_case("Csrf"));
    if allows_post && !has_csrf {
        filters.push(Filter {
            class: "Csrf".into(),
            fqn: "Bitrix\\Main\\Engine\\ActionFilter\\Csrf".into(),
            from: BITRIX_CONTROLLER.into(),
            args: Vec::new(),
            implicit: true,
        });
    }
    if let Some(Val::List(removed)) = entry("-prefilters") {
        let removed: Vec<String> = removed
            .iter()
            .filter_map(|r| r.as_str().map(|s| s.trim_start_matches('\\').to_ascii_lowercase()))
            .collect();
        filters.retain(|f| {
            !removed
                .iter()
                .any(|r| *r == f.fqn.to_ascii_lowercase() || *r == f.class.to_ascii_lowercase())
        });
    }
    if let Some(added) = entry("+prefilters") {
        match as_filters(added) {
            Ok(f) => filters.extend(f),
            Err(note) => return (None, Some(format!("configureActions() +prefilters: {note}"))),
        }
    }
    (Some(filters), None)
}

fn attribute_texts(node: tree_sitter::Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for list in php_ast::named_children(node)
        .into_iter()
        .filter(|c| c.kind() == "attribute_list")
    {
        for group in php_ast::named_children(list) {
            out.push(php_ast::render(group, src, None));
        }
    }
    out
}

/// Attributes on the class and its parents, each with the class it is written on.
fn class_attributes(classes: &IndexClasses, class: &str) -> Vec<ClassAttribute> {
    let mut out = Vec::new();
    let mut current = Some(class.to_string());
    let mut seen = std::collections::HashSet::new();
    while let Some(c) = current {
        if !seen.insert(c.to_ascii_lowercase()) {
            break;
        }
        let Some(model) = classes.class(&c) else {
            break;
        };
        if let Some((_, content)) = classes.source(&model.fqn) {
            if let Ok(tree) = php_ast::parse(&content) {
                let root = tree.root_node();
                let names = Names::of(root, &content);
                if let Some(node) = php_ast::class_like_nodes(root).into_iter().find(|n| {
                    php_model::declared_fqn(*n, &content, &names)
                        .is_some_and(|f| f.eq_ignore_ascii_case(&model.fqn))
                }) {
                    for attribute in attribute_texts(node, &content) {
                        out.push(ClassAttribute {
                            attribute,
                            from: model.fqn.clone(),
                        });
                    }
                }
            }
        }
        current = model.parent.clone();
    }
    out
}

struct ActionDetails {
    handler: Option<String>,
    handler_at: Option<String>,
    attributes: Vec<String>,
    class_attributes: Vec<ClassAttribute>,
    prefilters: Option<Vec<Filter>>,
    prefilters_note: Option<String>,
}

/// Handler, its location, attributes and prefilters for action `action` of `class`.
fn action_details(classes: &IndexClasses, class: &str, action: &str, component_class: bool) -> ActionDetails {
    let method_name = format!("{action}Action");
    let class = classes.class(class).map(|m| m.fqn.clone()).unwrap_or_else(|| class.to_string());
    let found = php_calls::find_method(classes, &class, &method_name);
    let (handler, at, attributes) = match &found {
        Some((declaring, m)) => (
            Some(format!("{class}::{}", m.name)),
            classes
                .source(&declaring.fqn)
                .map(|(p, _)| format!("{p}:{}-{}", m.line, m.end_line)),
            with_method(classes, &declaring.fqn, &m.name, |node, src, _| attribute_texts(node, src))
                .unwrap_or_default(),
        ),
        None => (Some(format!("{class}::{method_name}")), None, Vec::new()),
    };
    let (prefilters, prefilters_note) = prefilters_for(classes, &class, action, component_class);
    ActionDetails {
        handler,
        handler_at: at,
        attributes,
        class_attributes: class_attributes(classes, &class),
        prefilters,
        prefilters_note,
    }
}

pub fn cmd_routes(root: &Path, kind: Option<&str>, path_prefix: Option<&str>, format: &str) -> Result<()> {
    let _lease = crate::db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let classes = IndexClasses::new(&conn, root);
    let prefix = path_prefix.map(|p| p.trim_start_matches("./").trim_end_matches('/').to_string());
    let in_scope = |path: &str| {
        prefix
            .as_deref()
            .is_none_or(|p| path == p || path.starts_with(&format!("{p}/")))
    };
    let read = |path: &str, root_path: &Option<String>| {
        let base = match root_path.as_deref() {
            Some(r) if !r.is_empty() => Path::new(r).to_path_buf(),
            _ => root.to_path_buf(),
        };
        classes.read(&base.join(path))
    };
    let mut entries = Vec::new();

    if kind.is_none_or(|k| k == "route") {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT f.path, f.root_path FROM refs r JOIN files f ON f.id = r.file_id
             WHERE r.fqn = 'Bitrix\\Main\\Routing\\RoutingConfigurator' COLLATE NOCASE AND f.external = 0
             ORDER BY f.path",
        )?;
        let files = stmt
            .query_map(params![], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (path, root_path) in files {
            if !in_scope(&path) {
                continue;
            }
            let Some(content) = read(&path, &root_path) else {
                continue;
            };
            let Ok(routes) = crate::parsers::php_routes::route_definitions(&content, &classes) else {
                continue;
            };
            for r in routes {
                let method = Some(if r.methods.is_empty() { "ANY".to_string() } else { r.methods.join("|") });
                let mut entry = RouteEntry {
                    kind: "route",
                    method,
                    path: r.path.clone(),
                    path_expr: r.path_expr.clone(),
                    name: r.name.clone(),
                    component: None,
                    component_path: None,
                    mode: None,
                    file: path.clone(),
                    line: r.line,
                    handler: None,
                    handler_at: None,
                    action: None,
                    attributes: Vec::new(),
                    class_attributes: Vec::new(),
                    prefilters: None,
                    prefilters_note: None,
                };
                match &r.handler {
                    RouteHandler::Action { class, action } => {
                        // The core strips an `Action` suffix and runs `<name>Action`.
                        let name = action.strip_suffix("Action").unwrap_or(action).to_string();
                        let details = action_details(&classes, class, &name, false);
                        entry.handler = details.handler;
                        entry.handler_at = details.handler_at;
                        entry.action = Some(name);
                        entry.attributes = details.attributes;
                        entry.class_attributes = details.class_attributes;
                        entry.prefilters = details.prefilters;
                        entry.prefilters_note = details.prefilters_note;
                    }
                    RouteHandler::Class(class) => {
                        entry.handler = Some(class.clone());
                        entry.prefilters_note = Some("an action class: its controller decides".into());
                    }
                    RouteHandler::Closure { line, end_line } => {
                        entry.handler_at = Some(format!("{path}:{line}-{end_line}"));
                        entry.prefilters_note = Some("a closure runs without action filters".into());
                    }
                    RouteHandler::Dynamic(text) => {
                        entry.prefilters_note = Some(format!("handler built at run time: {text}"));
                    }
                }
                entries.push(entry);
            }
        }
    }

    if kind.is_none_or(|k| k == "component") {
        let mut stmt = conn.prepare(
            "SELECT f.path, f.root_path FROM files f
             WHERE f.external = 0 AND f.path LIKE '%components/%'
               AND (f.path LIKE '%/class.php' OR f.path LIKE '%/ajax.php')
             ORDER BY f.path",
        )?;
        let files = stmt
            .query_map(params![], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (path, root_path) in files {
            if !in_scope(&path) {
                continue;
            }
            let Some(content) = read(&path, &root_path) else {
                continue;
            };
            let Ok(models) = php_model::class_models(&content) else {
                continue;
            };
            let mode = if path.ends_with("/ajax.php") { "ajax" } else { "class" };
            let dir = path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
            let component = component_name(&dir);
            for model in models {
                let mut actions: Vec<&php_model::MethodModel> = model
                    .methods
                    .values()
                    .filter(|m| m.name.len() > 6 && m.name.ends_with("Action") && m.visibility == "public")
                    .collect();
                actions.sort_by_key(|m| m.line);
                for m in actions {
                    let name = m.name[..m.name.len() - 6].to_string();
                    let details = action_details(&classes, &model.fqn, &name, mode == "class");
                    entries.push(RouteEntry {
                        kind: "component",
                        method: None,
                        path: None,
                        path_expr: None,
                        name: None,
                        component: component.clone(),
                        component_path: Some(dir.clone()),
                        mode: Some(mode),
                        file: path.clone(),
                        line: m.line,
                        handler: Some(format!("{}::{}", model.fqn, m.name)),
                        handler_at: Some(format!("{path}:{}-{}", m.line, m.end_line)),
                        action: Some(name),
                        attributes: details.attributes,
                        class_attributes: details.class_attributes,
                        prefilters: details.prefilters,
                        prefilters_note: details.prefilters_note,
                    });
                }
            }
        }
    }

    if format == "json" {
        return print_json(&entries);
    }
    for e in &entries {
        let target = match e.kind {
            "route" => format!(
                "{} {}",
                e.method.as_deref().unwrap_or("ANY"),
                e.path.as_deref().or(e.path_expr.as_deref()).unwrap_or("?")
            ),
            _ => format!(
                "{} · {} ({})",
                e.component.as_deref().or(e.component_path.as_deref()).unwrap_or("?"),
                e.action.as_deref().unwrap_or("?"),
                e.mode.unwrap_or("")
            ),
        };
        let filters = match &e.prefilters {
            Some(f) => f
                .iter()
                .map(|f| if f.implicit { format!("{}*", f.class) } else { f.class.clone() })
                .collect::<Vec<_>>()
                .join(", "),
            None => format!("? {}", e.prefilters_note.as_deref().unwrap_or("")),
        };
        let attrs = if e.attributes.is_empty() {
            String::new()
        } else {
            format!(" {}", e.attributes.join(" "))
        };
        println!(
            "{}  -> {}{}  [{}]  {}",
            target.bold(),
            e.handler.as_deref().unwrap_or("-").yellow(),
            attrs,
            filters,
            format!("{}:{}", e.file, e.line).dimmed()
        );
    }
    if entries.is_empty() {
        println!("No routes or component actions found.");
    }
    Ok(())
}

/// `vendor:name` for a component directory under `local/components` or `bitrix/components`.
fn component_name(dir: &str) -> Option<String> {
    let parts: Vec<&str> = dir.split('/').collect();
    let at = parts
        .windows(2)
        .position(|w| matches!(w[0], "local" | "bitrix") && w[1] == "components")?;
    let vendor = parts.get(at + 2)?;
    let name = parts.get(at + 3)?;
    Some(format!("{vendor}:{name}"))
}
