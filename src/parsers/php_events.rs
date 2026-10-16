//! Bitrix event subscriptions in a PHP file: `EventManager::addEventHandler`,
//! `addEventHandlerCompatible`, `registerEventHandler`, `registerEventHandlerCompatible` and
//! the legacy `AddEventHandler()`, `RegisterModuleDependences()` — with the module, the event
//! names (a `foreach` over a literal array around the call unrolled), and the handler.

use std::collections::HashMap;

use anyhow::Result;
use tree_sitter::Node;

use super::php_ast::{self, Names};
use super::php_eval::{Constants, Evaluator, Val};
use super::php_model;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handler {
    Method { class: String, method: String },
    Function(String),
    Closure { line: usize, end_line: usize },
    /// Built at run time: the callback as written.
    Dynamic(String),
}

#[derive(Debug, Clone)]
pub struct Registration {
    pub line: usize,
    /// The registering call as written in the source (`addEventHandler`, `AddEventHandler`…).
    pub call: String,
    pub module: Val,
    pub events: Vec<Val>,
    pub handler: Handler,
    /// `registerEventHandler`: the module whose class handles the event.
    pub to_module: Option<Val>,
    pub sort: Option<Val>,
}

/// How a registering call lays out its arguments.
struct Shape {
    module: usize,
    event: usize,
    /// Position of the callback, or of `toModule`/`toClass`/`toMethod` for a persistent one.
    callback: Option<usize>,
    to_module: Option<usize>,
    to_class: Option<usize>,
    to_method: Option<usize>,
    sort: Option<usize>,
    /// Parameter names, for named arguments.
    names: &'static [&'static str],
}

fn shape(call: &str, is_method: bool) -> Option<Shape> {
    let runtime_names: &[&str] = &["fromModuleId", "eventType", "callback", "includeFile", "sort"];
    let persistent_names: &[&str] = &[
        "fromModuleId",
        "eventType",
        "toModuleId",
        "toClass",
        "toMethod",
        "sort",
        "toPath",
        "toMethodArg",
    ];
    match (call.to_ascii_lowercase().as_str(), is_method) {
        ("addeventhandler" | "addeventhandlercompatible", true) => Some(Shape {
            module: 0,
            event: 1,
            callback: Some(2),
            to_module: None,
            to_class: None,
            to_method: None,
            sort: Some(4),
            names: runtime_names,
        }),
        // Legacy global function: sort comes before the include file.
        ("addeventhandler", false) => Some(Shape {
            module: 0,
            event: 1,
            callback: Some(2),
            to_module: None,
            to_class: None,
            to_method: None,
            sort: Some(3),
            names: &["FROM_MODULE_ID", "MESSAGE_ID", "CALLBACK", "SORT", "FULL_PATH"],
        }),
        ("registereventhandler" | "registereventhandlercompatible", true)
        | ("registermoduledependences", false) => Some(Shape {
            module: 0,
            event: 1,
            callback: None,
            to_module: Some(2),
            to_class: Some(3),
            to_method: Some(4),
            sort: Some(5),
            names: persistent_names,
        }),
        _ => None,
    }
}

/// Unrolled loops stop here: a subscription in nested loops over long arrays is not worth
/// thousands of entries.
const MAX_ITERATIONS: usize = 64;

pub fn registrations(content: &str, constants: &dyn Constants) -> Result<Vec<Registration>> {
    let tree = php_ast::parse(content)?;
    let root = tree.root_node();
    let names = Names::of(root, content);
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if let Some(found) = registration(node, content, &names, constants) {
            out.extend(found);
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    Ok(out)
}

fn registration(
    node: Node,
    src: &str,
    names: &Names,
    constants: &dyn Constants,
) -> Option<Vec<Registration>> {
    let (call, is_method, arguments) = match node.kind() {
        "member_call_expression" | "nullsafe_member_call_expression" => (
            php_ast::text(node.child_by_field_name("name")?, src),
            true,
            node.child_by_field_name("arguments")?,
        ),
        "scoped_call_expression" => (
            php_ast::text(node.child_by_field_name("name")?, src),
            true,
            node.child_by_field_name("arguments")?,
        ),
        "function_call_expression" => {
            let function = node.child_by_field_name("function")?;
            (
                php_ast::short_name(php_ast::text(function, src)),
                false,
                node.child_by_field_name("arguments")?,
            )
        }
        _ => return None,
    };
    let shape = shape(call, is_method)?;
    let argument_nodes = positional(arguments, &shape, src);
    let class = enclosing_class(node, src, names);

    let mut combos: Vec<HashMap<String, Val>> = vec![HashMap::new()];
    for each in loop_bindings(node, src) {
        let mut next = Vec::new();
        for combo in &combos {
            let evaluator = Evaluator {
                class: class.clone(),
                bindings: combo.clone(),
                ..Evaluator::new(src, names, constants)
            };
            let items: Vec<(Val, Val)> = match evaluator.eval(each.iterable) {
                Val::List(items) => items
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| (Val::Int(i as i64), v))
                    .collect(),
                Val::Map(pairs) => pairs,
                _ => {
                    next.push(combo.clone());
                    continue;
                }
            };
            for (key, value) in items {
                let mut bound = combo.clone();
                if let Some(k) = &each.key {
                    bound.insert(k.clone(), key.clone());
                }
                if let Some(v) = &each.value {
                    bound.insert(v.clone(), value.clone());
                }
                next.push(bound);
                if next.len() >= MAX_ITERATIONS {
                    break;
                }
            }
        }
        combos = next;
    }

    let arg = |i: Option<usize>| i.and_then(|i| argument_nodes.get(i).copied().flatten());
    // One entry per module and handler: a loop usually varies only the event name.
    let mut grouped: Vec<Registration> = Vec::new();
    for combo in combos {
        let evaluator = Evaluator {
            class: class.clone(),
            bindings: combo,
            ..Evaluator::new(src, names, constants)
        };
        let value = |i: Option<usize>| arg(i).map(|n| evaluator.eval(n));
        let module = value(Some(shape.module)).unwrap_or(Val::Null);
        let event = value(Some(shape.event)).unwrap_or(Val::Null);
        let handler = match shape.callback {
            Some(i) => arg(Some(i))
                .map(|n| callback(n, &evaluator, class.as_deref(), src))
                .unwrap_or(Handler::Dynamic(String::new())),
            None => persistent_handler(value(shape.to_class), value(shape.to_method), arg(shape.to_class), src),
        };
        let to_module = value(shape.to_module);
        let sort = value(shape.sort);
        match grouped
            .iter_mut()
            .find(|r| r.module == module && r.handler == handler && r.to_module == to_module)
        {
            Some(existing) => {
                if !existing.events.contains(&event) {
                    existing.events.push(event);
                }
            }
            None => grouped.push(Registration {
                line: php_ast::line(node),
                call: call.to_string(),
                module,
                events: vec![event],
                handler,
                to_module,
                sort,
            }),
        }
    }
    Some(grouped)
}

/// Argument nodes by parameter position, named arguments placed by their name.
fn positional<'t>(arguments: Node<'t>, shape: &Shape, src: &str) -> Vec<Option<Node<'t>>> {
    let mut out: Vec<Option<Node>> = Vec::new();
    for (i, argument) in php_ast::named_children(arguments)
        .into_iter()
        .filter(|a| a.kind() == "argument")
        .enumerate()
    {
        let at = match argument.child_by_field_name("name") {
            Some(name) => {
                let label = php_ast::text(name, src);
                match shape.names.iter().position(|n| n.eq_ignore_ascii_case(label)) {
                    Some(p) => p,
                    None => continue,
                }
            }
            None => i,
        };
        if out.len() <= at {
            out.resize(at + 1, None);
        }
        out[at] = Some(argument);
    }
    out
}

/// A `foreach` around a call: the variables it binds and what it iterates.
struct Loop<'t> {
    key: Option<String>,
    value: Option<String>,
    iterable: Node<'t>,
}

/// `foreach` statements around `node`, outermost first.
fn loop_bindings<'t>(node: Node<'t>, src: &str) -> Vec<Loop<'t>> {
    let mut loops = Vec::new();
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "foreach_statement" {
            let parts = php_ast::named_children(n);
            let body = n.child_by_field_name("body");
            let inside_body = body.is_some_and(|b| b.start_byte() <= node.start_byte());
            if let (Some(iterable), Some(binding), true) = (parts.first(), parts.get(1), inside_body) {
                let var = |x: Node| -> Option<String> {
                    let x = if x.kind() == "by_ref" {
                        php_ast::named_children(x).into_iter().next()?
                    } else {
                        x
                    };
                    (x.kind() == "variable_name")
                        .then(|| php_ast::text(x, src).trim_start_matches('$').to_string())
                };
                let (key, value) = if binding.kind() == "pair" {
                    let kv = php_ast::named_children(*binding);
                    (kv.first().and_then(|k| var(*k)), kv.get(1).and_then(|v| var(*v)))
                } else {
                    (None, var(*binding))
                };
                loops.push(Loop {
                    key,
                    value,
                    iterable: *iterable,
                });
            }
        }
        if matches!(
            n.kind(),
            "method_declaration" | "function_definition" | "anonymous_function" | "arrow_function"
        ) {
            break;
        }
        current = n.parent();
    }
    loops.reverse();
    loops
}

fn enclosing_class(node: Node, src: &str, names: &Names) -> Option<String> {
    let mut current = node.parent();
    while let Some(n) = current {
        if php_ast::CLASS_LIKE_KINDS.contains(&n.kind()) {
            return php_model::declared_fqn(n, src, names);
        }
        current = n.parent();
    }
    None
}

fn callback(node: Node, evaluator: &Evaluator, class: Option<&str>, src: &str) -> Handler {
    // `[$this, 'handle']`
    let value_node = php_ast::named_children(node)
        .into_iter()
        .rfind(|c| Some(*c) != node.child_by_field_name("name"))
        .unwrap_or(node);
    if value_node.kind() == "array_creation_expression" {
        let elements: Vec<Node> = php_ast::named_children(value_node)
            .into_iter()
            .filter(|e| e.kind() == "array_element_initializer")
            .collect();
        if let [first, second] = elements.as_slice() {
            let first_value = php_ast::named_children(*first).into_iter().next();
            let is_this = first_value
                .is_some_and(|f| f.kind() == "variable_name" && php_ast::text(f, src) == "$this");
            if let (true, Some(class), Val::Str(method)) = (
                is_this,
                class,
                php_ast::named_children(*second)
                    .into_iter()
                    .next()
                    .map(|s| evaluator.eval(s))
                    .unwrap_or(Val::Null),
            ) {
                return Handler::Method {
                    class: class.to_string(),
                    method,
                };
            }
        }
    }
    match evaluator.eval(node) {
        Val::List(items) => match items.as_slice() {
            [Val::Str(class), Val::Str(method)] if !class.is_empty() => Handler::Method {
                class: class.trim_start_matches('\\').to_string(),
                method: method.clone(),
            },
            _ => Handler::Dynamic(php_ast::render(value_node, src, None)),
        },
        Val::Str(s) => match s.split_once("::") {
            Some((class, method)) => Handler::Method {
                class: class.trim_start_matches('\\').to_string(),
                method: method.to_string(),
            },
            None => Handler::Function(s.trim_start_matches('\\').to_string()),
        },
        Val::Closure { line, end_line } => Handler::Closure { line, end_line },
        _ => Handler::Dynamic(php_ast::render(value_node, src, None)),
    }
}

fn persistent_handler(class: Option<Val>, method: Option<Val>, class_node: Option<Node>, src: &str) -> Handler {
    match (class, method) {
        (Some(Val::Str(class)), Some(Val::Str(method))) if !class.is_empty() && !method.is_empty() => {
            Handler::Method {
                class: class.trim_start_matches('\\').to_string(),
                method,
            }
        }
        _ => Handler::Dynamic(
            class_node
                .map(|n| php_ast::render(n, src, None))
                .unwrap_or_default(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::php_eval::NoConstants;

    fn strings(values: &[Val]) -> Vec<&str> {
        values.iter().filter_map(Val::as_str).collect()
    }

    #[test]
    fn unrolls_a_foreach_over_literal_event_types() {
        let src = r#"<?php
use Bitrix\Main\EventManager;
use Orteka\Delivery\Bridge;

$eventManager = EventManager::getInstance();
foreach (['OnAfterAdd', 'OnAfterUpdate', 'OnAfterDelete'] as $eventType) {
    $eventManager->addEventHandler('', 'Tariff' . $eventType, [Bridge::class, 'onTariff']);
    $eventManager->addEventHandler('orteka.delivery', 'Calendar' . $eventType, [Bridge::class, 'onCalendar']);
}
$eventManager->addEventHandler('iblock', 'OnAfterIBlockElementAdd', [Bridge::class, 'onSalon']);
"#;
        let found = registrations(src, &NoConstants).unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].line, 7);
        assert_eq!(found[0].module, Val::Str(String::new()));
        assert_eq!(
            strings(&found[0].events),
            vec!["TariffOnAfterAdd", "TariffOnAfterUpdate", "TariffOnAfterDelete"]
        );
        assert_eq!(
            found[0].handler,
            Handler::Method {
                class: "Orteka\\Delivery\\Bridge".into(),
                method: "onTariff".into()
            }
        );
        assert_eq!(found[1].module, Val::Str("orteka.delivery".into()));
        assert_eq!(strings(&found[2].events), vec!["OnAfterIBlockElementAdd"]);
    }

    #[test]
    fn multiline_call_and_callback_shapes() {
        let src = r#"<?php
namespace App;
class Init
{
    public function on(): void
    {
        $em = \Bitrix\Main\EventManager::getInstance();
        $em->addEventHandler(
            'sale',
            'OnSaleBasketItemSaved',
            [$this, 'onSaved'],
        );
        $em->addEventHandler('main', 'OnProlog', 'App\Handlers::prolog');
        $em->addEventHandler('main', 'OnEpilog', ['MainHandlers', 'epilog']);
        $em->addEventHandler('main', 'OnPageStart', function () { return 1; });
        $em->addEventHandler('main', 'OnEnd', is_array($cb) ? $cb : [$cls, $cb]);
        AddEventHandler('main', 'OnBeforeProlog', 'legacyHandler', 50);
        $em->registerEventHandler('main', 'OnAfterUserAuthorize', 'my.module', '\My\Favorites', 'eventHandler');
    }
}
"#;
        let found = registrations(src, &NoConstants).unwrap();
        let handler = |event: &str| {
            found
                .iter()
                .find(|r| strings(&r.events) == vec![event])
                .unwrap()
        };
        let saved = handler("OnSaleBasketItemSaved");
        assert_eq!(saved.line, 8);
        assert_eq!(
            saved.handler,
            Handler::Method { class: "App\\Init".into(), method: "onSaved".into() }
        );
        assert_eq!(
            handler("OnProlog").handler,
            Handler::Method { class: "App\\Handlers".into(), method: "prolog".into() }
        );
        assert_eq!(
            handler("OnEpilog").handler,
            Handler::Method { class: "MainHandlers".into(), method: "epilog".into() }
        );
        assert_eq!(handler("OnPageStart").handler, Handler::Closure { line: 15, end_line: 15 });
        assert!(matches!(handler("OnEnd").handler, Handler::Dynamic(_)));
        let legacy = handler("OnBeforeProlog");
        assert_eq!(legacy.handler, Handler::Function("legacyHandler".into()));
        assert_eq!(legacy.sort, Some(Val::Int(50)));
        let persistent = handler("OnAfterUserAuthorize");
        assert_eq!(persistent.to_module, Some(Val::Str("my.module".into())));
        assert_eq!(
            persistent.handler,
            Handler::Method { class: "My\\Favorites".into(), method: "eventHandler".into() }
        );
    }
}
