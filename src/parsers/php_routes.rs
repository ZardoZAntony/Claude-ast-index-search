//! Routes of a Bitrix routing file (`return function (RoutingConfigurator $routes) {…}`):
//! `get`/`post`/`put`/`patch`/`delete`/`options`/`any`/`match` calls with the prefixes of the
//! `prefix(…)->group(function …)` blocks around them, and the handler each one names.

use anyhow::Result;
use tree_sitter::Node;

use super::php_ast::{self, Names};
use super::php_eval::{Constants, Evaluator, Val};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteHandler {
    /// `[Controller::class, 'addAction']`: the action as written.
    Action { class: String, action: String },
    /// A routable action class given by name.
    Class(String),
    Closure { line: usize, end_line: usize },
    Dynamic(String),
}

#[derive(Debug, Clone)]
pub struct RouteDef {
    pub line: usize,
    /// `GET`, `POST`…, `ANY`.
    pub methods: Vec<String>,
    /// The full path with group prefixes, when it folds to a string.
    pub path: Option<String>,
    /// The path expression otherwise.
    pub path_expr: Option<String>,
    pub name: Option<String>,
    pub handler: RouteHandler,
}

const ROUTE_CALLS: [&str; 8] = ["get", "post", "put", "patch", "delete", "options", "any", "match"];

pub fn route_definitions(content: &str, constants: &dyn Constants) -> Result<Vec<RouteDef>> {
    let tree = php_ast::parse(content)?;
    let root = tree.root_node();
    let names = Names::of(root, content);
    let evaluator = Evaluator::new(content, &names, constants);
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if let Some(route) = route(node, content, &evaluator) {
            out.push(route);
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    out.sort_by_key(|r| r.line);
    Ok(out)
}

fn call_name<'s>(node: Node, src: &'s str) -> Option<&'s str> {
    if node.kind() != "member_call_expression" {
        return None;
    }
    Some(php_ast::text(node.child_by_field_name("name")?, src))
}

fn arguments(node: Node) -> Vec<Node> {
    node.child_by_field_name("arguments")
        .map(php_ast::named_children)
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.kind() == "argument")
        .collect()
}

fn route(node: Node, src: &str, evaluator: &Evaluator) -> Option<RouteDef> {
    let call = call_name(node, src)?.to_ascii_lowercase();
    if !ROUTE_CALLS.contains(&call.as_str()) {
        return None;
    }
    let args = arguments(node);
    let (methods, uri, handler) = if call == "match" {
        let [methods, uri, handler, ..] = args.as_slice() else {
            return None;
        };
        let methods = match evaluator.eval(*methods) {
            Val::List(items) => items
                .iter()
                .filter_map(|m| m.as_str().map(str::to_ascii_uppercase))
                .collect(),
            Val::Str(s) => vec![s.to_ascii_uppercase()],
            _ => Vec::new(),
        };
        (methods, *uri, *handler)
    } else {
        let [uri, handler, ..] = args.as_slice() else {
            return None;
        };
        (vec![call.to_ascii_uppercase()], *uri, *handler)
    };

    // Prefixes: from the groups around the call (outermost first), then the call's own chain.
    let mut prefixes: Vec<String> = Vec::new();
    let mut groups = Vec::new();
    let mut current = node.parent();
    while let Some(n) = current {
        if matches!(n.kind(), "anonymous_function" | "arrow_function") {
            // The closure is an argument of `…->group(closure)`.
            let group_call = n
                .parent()
                .and_then(|a| a.parent())
                .and_then(|args| args.parent())
                .filter(|c| call_name(*c, src).is_some_and(|name| name.eq_ignore_ascii_case("group")));
            if let Some(group) = group_call {
                groups.push(group);
            }
        }
        current = n.parent();
    }
    for group in groups.iter().rev() {
        prefixes.extend(chain_prefixes(*group, src, evaluator));
    }
    prefixes.extend(chain_prefixes(node, src, evaluator));

    // Modifiers chained after the route: `->name('x')`, `->methods([...])`.
    let mut name = chain_value(node, "name", src, evaluator);
    let mut methods = methods;
    let mut outer = node;
    while let Some(parent) = outer.parent() {
        if parent.kind() != "member_call_expression" || parent.child_by_field_name("object") != Some(outer) {
            break;
        }
        match call_name(parent, src).map(str::to_ascii_lowercase).as_deref() {
            Some("name") => {
                if let Some(Val::Str(n)) = arguments(parent).first().map(|a| evaluator.eval(*a)) {
                    name = Some(n);
                }
            }
            Some("methods") => {
                if let Some(Val::List(items)) = arguments(parent).first().map(|a| evaluator.eval(*a)) {
                    methods = items
                        .iter()
                        .filter_map(|m| m.as_str().map(str::to_ascii_uppercase))
                        .collect();
                }
            }
            _ => {}
        }
        outer = parent;
    }

    let (path, path_expr) = match evaluator.eval(uri) {
        Val::Str(uri) => (Some(full_path(&prefixes, &uri)), None),
        other => (None, Some(match other {
            Val::Expr(text) => text,
            v => v.to_json().to_string(),
        })),
    };
    let handler = match evaluator.eval(handler) {
        Val::List(items) => match items.as_slice() {
            [Val::Str(class), Val::Str(action)] => RouteHandler::Action {
                class: class.trim_start_matches('\\').to_string(),
                action: action.clone(),
            },
            _ => RouteHandler::Dynamic(php_ast::render(handler, src, None)),
        },
        Val::Str(class) => RouteHandler::Class(class.trim_start_matches('\\').to_string()),
        Val::Closure { line, end_line } => RouteHandler::Closure { line, end_line },
        _ => RouteHandler::Dynamic(php_ast::render(handler, src, None)),
    };
    Some(RouteDef {
        line: php_ast::line(node),
        methods,
        path,
        path_expr,
        name,
        handler,
    })
}

/// `prefix(…)` values in the object chain of a call: `$routes->prefix('api')->name('x')->group`.
fn chain_prefixes(call: Node, src: &str, evaluator: &Evaluator) -> Vec<String> {
    let mut found = Vec::new();
    let mut object = call.child_by_field_name("object");
    while let Some(o) = object {
        if call_name(o, src).is_some_and(|n| n.eq_ignore_ascii_case("prefix")) {
            if let Some(Val::Str(p)) = arguments(o).first().map(|a| evaluator.eval(*a)) {
                found.push(p);
            }
        }
        object = if o.kind() == "member_call_expression" {
            o.child_by_field_name("object")
        } else {
            None
        };
    }
    found.reverse();
    found
}

/// The value of the last `<method>(…)` in the object chain of a call.
fn chain_value(call: Node, method: &str, src: &str, evaluator: &Evaluator) -> Option<String> {
    let mut object = call.child_by_field_name("object");
    while let Some(o) = object {
        if call_name(o, src).is_some_and(|n| n.eq_ignore_ascii_case(method)) {
            if let Some(Val::Str(v)) = arguments(o).first().map(|a| evaluator.eval(*a)) {
                return Some(v);
            }
        }
        object = if o.kind() == "member_call_expression" {
            o.child_by_field_name("object")
        } else {
            None
        };
    }
    None
}

/// Bitrix joins prefixes and the route's uri with `/`.
fn full_path(prefixes: &[String], uri: &str) -> String {
    if prefixes.is_empty() {
        return uri.to_string();
    }
    let mut joined = format!("/{}/{}", prefixes.join("/"), uri);
    while joined.contains("//") {
        joined = joined.replace("//", "/");
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::php_eval::NoConstants;

    #[test]
    fn routes_with_nested_prefix_groups_and_comments() {
        let src = r#"<?php
use Bitrix\Main\Routing\RoutingConfigurator;
use App\Controller\BasketController;
use App\Controller\FormController;

return function (RoutingConfigurator $routes) {
    $routes->post('/api/v1/basket/items', [BasketController::class, 'addAction']);
    $routes->delete('/api/v1/basket/items/{basketItemId}', [BasketController::class, 'removeAction']);
    // $routes->post('/api/v1/old/', [BasketController::class, 'oldAction']);
    /* $routes->get('/api/v1/hidden/', [BasketController::class, 'hiddenAction']); */
    $routes->prefix('api')->group(function (RoutingConfigurator $routes) {
        $routes->prefix('form')->group(function (RoutingConfigurator $routes) {
            $routes->post('partner/question/', [FormController::class, 'saveQuestion'])->name('partner');
        });
    });
    $routes->any('/connector/{type}/', fn() => 1)->methods(['GET', 'POST']);
};
"#;
        let routes = route_definitions(src, &NoConstants).unwrap();
        assert_eq!(routes.len(), 4, "{routes:#?}");
        assert_eq!(routes[0].methods, vec!["POST"]);
        assert_eq!(routes[0].path.as_deref(), Some("/api/v1/basket/items"));
        assert_eq!(
            routes[0].handler,
            RouteHandler::Action {
                class: "App\\Controller\\BasketController".into(),
                action: "addAction".into()
            }
        );
        assert_eq!(routes[1].methods, vec!["DELETE"]);
        assert_eq!(routes[2].line, 13);
        assert_eq!(routes[2].path.as_deref(), Some("/api/form/partner/question/"));
        assert_eq!(routes[2].name.as_deref(), Some("partner"));
        assert_eq!(routes[3].methods, vec!["GET", "POST"]);
        assert!(matches!(routes[3].handler, RouteHandler::Closure { .. }));
    }
}
