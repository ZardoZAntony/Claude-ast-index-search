//! `callees 'Type::method'`: what a PHP method calls, as a tree. Each call names the type its
//! receiver resolves to and the method it lands in; an abstract or interface method lists its
//! implementations. The opposite direction of `callers 'Type::method'`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Result};
use colored::Colorize;
use rusqlite::Connection;
use serde::Serialize;

use super::php::{normalize_fqn, open, print_json, resolve_one_class, with_subtypes};
use super::php_classes::IndexClasses;
use crate::parsers::php_calls::{self, CallSite, Classes};
use crate::parsers::php_model::ClassModel;

#[derive(Serialize)]
struct Constant {
    name: String,
    fqn: String,
}

#[derive(Serialize)]
struct CallNode {
    /// `Type::method` the call lands on (the receiver's type, not where the method is
    /// declared), a function name, or `null` when the receiver's type is unknown.
    call: Option<String>,
    /// `method`, `static`, `new`, `function`, or `implementation` (a child listing one
    /// implementation of an abstract method).
    kind: &'static str,
    method: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    receiver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    receiver_type: Option<String>,
    resolved_by: &'static str,
    path: String,
    line: usize,
    /// The class that declares the method, when it is not the receiver's type itself
    /// (inherited from a parent or a trait, declared by an interface).
    #[serde(skip_serializing_if = "Option::is_none")]
    declared_in: Option<String>,
    /// `path:start-end` of the declaration.
    #[serde(skip_serializing_if = "Option::is_none")]
    declared_at: Option<String>,
    /// The receiver's type is known but declares no such method in the index (framework
    /// core, magic `__call`, a typo).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    undeclared: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    r#abstract: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    implementations: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    constants: Vec<Constant>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    classes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Option<Vec<CallNode>>,
    /// Already expanded earlier in this tree: its calls are listed there.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    expanded_above: bool,
    /// Matched `--stop`: not expanded.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stopped: bool,
}

#[derive(Clone)]
struct Implementor {
    fqn: String,
    path: String,
    line: usize,
    end_line: usize,
}

impl Implementor {
    fn at(&self) -> String {
        format!("{}:{}-{}", self.path, self.line, self.end_line)
    }
}

struct Walk<'a> {
    classes: &'a IndexClasses<'a>,
    conn: &'a Connection,
    implementations: bool,
    stop: Vec<String>,
    expanded: HashSet<String>,
    implementors: HashMap<String, Vec<Implementor>>,
}

impl Walk<'_> {
    fn stops(&self, class: &str, method: &str) -> bool {
        let class = class.to_ascii_lowercase();
        let call = format!("{class}::{}", method.to_ascii_lowercase());
        self.stop.iter().any(|s| {
            if s.contains("::") {
                *s == call
            } else if s.ends_with('\\') {
                class.starts_with(s.as_str())
            } else {
                *s == class
            }
        })
    }

    /// Classes among the subtypes of `owner` that declare `method` with a body.
    fn implementors_of(&mut self, owner: &str, method: &str) -> Vec<Implementor> {
        let key = format!("{}::{}", owner.to_ascii_lowercase(), method.to_ascii_lowercase());
        if let Some(found) = self.implementors.get(&key) {
            return found.clone();
        }
        let mut out: Vec<Implementor> = Vec::new();
        let subtypes = with_subtypes(self.conn, &[owner.to_string()]).unwrap_or_default();
        for sub in subtypes.iter().skip(1) {
            let Some(model) = self.classes.class(sub) else {
                continue;
            };
            let Some(m) = model.method(method) else {
                continue;
            };
            if m.is_abstract || out.iter().any(|i| i.fqn.eq_ignore_ascii_case(&model.fqn)) {
                continue;
            }
            out.push(Implementor {
                fqn: model.fqn.clone(),
                path: self.classes.source(&model.fqn).map(|(p, _)| p).unwrap_or_default(),
                line: m.line,
                end_line: m.end_line,
            });
        }
        self.implementors.insert(key, out.clone());
        out
    }

    /// Calls of `method` declared in `class`, read with `$this` standing for `this_class` (the
    /// class using the trait when `class` is one).
    fn expand(&mut self, class: &str, this_class: &str, method: &str, depth: usize) -> Result<Vec<CallNode>> {
        self.expanded.insert(expansion_key(class, this_class, method));
        let Some((path, content)) = self.classes.source(class) else {
            return Ok(Vec::new());
        };
        let Some(found) = php_calls::method_calls(&content, class, method, this_class, self.classes)? else {
            return Ok(Vec::new());
        };
        let mut nodes = Vec::new();
        for site in found.calls {
            nodes.push(self.node(site, &path, depth)?);
        }
        Ok(nodes)
    }

    fn node(&mut self, site: CallSite, path: &str, depth: usize) -> Result<CallNode> {
        let mut node = CallNode {
            call: None,
            kind: site.kind,
            method: site.method.clone(),
            receiver: site.receiver.clone(),
            receiver_type: site.receiver_type.clone(),
            resolved_by: site.resolved_by,
            path: path.to_string(),
            line: site.line,
            declared_in: None,
            declared_at: None,
            undeclared: false,
            r#abstract: false,
            implementations: Vec::new(),
            constants: site
                .constants
                .iter()
                .map(|c| Constant {
                    name: c.written.clone(),
                    fqn: c.fqn.clone(),
                })
                .collect(),
            classes: site.classes.clone(),
            children: None,
            expanded_above: false,
            stopped: false,
        };
        if site.kind == "function" {
            node.call = Some(site.method);
            return Ok(node);
        }
        let Some(owner) = site.receiver_type else {
            return Ok(node);
        };
        node.call = Some(format!("{owner}::{}", site.method));
        let Some((declaring, method)) = php_calls::find_method(self.classes, &owner, &site.method)
        else {
            // `new Foo()` on a class without a constructor is no call to follow, not an error.
            node.undeclared = site.kind != "new" || self.classes.class(&owner).is_none();
            return Ok(node);
        };
        if !declaring.fqn.eq_ignore_ascii_case(&owner) {
            node.declared_in = Some(declaring.fqn.clone());
        }
        node.declared_at = self
            .classes
            .source(&declaring.fqn)
            .map(|(p, _)| format!("{p}:{}-{}", method.line, method.end_line));
        node.r#abstract = method.is_abstract;
        if method.is_abstract {
            node.implementations = self
                .implementors_of(&declaring.fqn, &method.name)
                .into_iter()
                .map(|i| i.fqn)
                .collect();
        }
        if depth <= 1 {
            return Ok(node);
        }
        if self.stops(&owner, &method.name) || self.stops(&declaring.fqn, &method.name) {
            node.stopped = true;
            return Ok(node);
        }
        if !method.is_abstract {
            let this_class = this_for(&declaring, &owner);
            if self.expanded.contains(&expansion_key(&declaring.fqn, &this_class, &method.name)) {
                node.expanded_above = true;
            } else {
                node.children = Some(self.expand(&declaring.fqn, &this_class, &method.name, depth - 1)?);
            }
        } else if self.implementations {
            let mut children = Vec::new();
            for implementor in self.implementors_of(&declaring.fqn, &method.name) {
                let fqn = &implementor.fqn;
                let already = self.expanded.contains(&expansion_key(fqn, fqn, &method.name));
                let stopped = self.stops(fqn, &method.name);
                let grandchildren = if already || stopped {
                    None
                } else {
                    Some(self.expand(fqn, fqn, &method.name, depth - 1)?)
                };
                children.push(CallNode {
                    call: Some(format!("{fqn}::{}", method.name)),
                    kind: "implementation",
                    method: method.name.clone(),
                    receiver: String::new(),
                    receiver_type: Some(fqn.clone()),
                    resolved_by: "implementation",
                    path: implementor.path.clone(),
                    line: implementor.line,
                    declared_in: None,
                    declared_at: Some(implementor.at()),
                    undeclared: false,
                    r#abstract: false,
                    implementations: Vec::new(),
                    constants: Vec::new(),
                    classes: Vec::new(),
                    children: grandchildren,
                    expanded_above: already,
                    stopped,
                });
            }
            node.children = Some(children);
        }
        Ok(node)
    }
}

/// The class `$this` stands for in a method of `declaring` reached through `owner`: the owner
/// for a trait method, the declaring class otherwise.
fn this_for(declaring: &ClassModel, owner: &str) -> String {
    if declaring.kind == "trait" {
        owner.to_string()
    } else {
        declaring.fqn.clone()
    }
}

fn expansion_key(class: &str, this_class: &str, method: &str) -> String {
    format!(
        "{}::{}@{}",
        class.to_ascii_lowercase(),
        method.to_ascii_lowercase(),
        this_class.to_ascii_lowercase()
    )
}

pub fn cmd_callees(
    root: &Path,
    spec: &str,
    depth: usize,
    implementations: bool,
    stop: &[String],
    format: &str,
) -> Result<()> {
    let _lease = crate::db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let (type_part, method) = spec
        .rsplit_once("::")
        .map(|(t, m)| (normalize_fqn(t), m.trim().trim_end_matches("()").to_string()))
        .unwrap_or_default();
    if type_part.is_empty() || method.is_empty() {
        bail!("expected Type::method");
    }
    let owner = resolve_one_class(&conn, &type_part)?;
    let classes = IndexClasses::new(&conn, root);
    let Some((declaring, declared)) = php_calls::find_method(&classes, &owner, &method) else {
        let mut known: Vec<String> = classes
            .class(&owner)
            .map(|c| c.methods.values().map(|m| m.name.clone()).collect())
            .unwrap_or_default();
        known.sort();
        bail!(
            "{owner} declares no method {method} (searched its parents and traits); its own methods: {}",
            if known.is_empty() { "none".to_string() } else { known.join(", ") }
        );
    };
    let (path, _) = classes.source(&declaring.fqn).unwrap_or_default();
    let mut walk = Walk {
        classes: &classes,
        conn: &conn,
        implementations,
        stop: stop
            .iter()
            .map(|s| normalize_fqn(s).to_ascii_lowercase())
            .collect(),
        expanded: HashSet::new(),
        implementors: HashMap::new(),
    };
    let calls = if declared.is_abstract {
        Vec::new()
    } else {
        walk.expand(&declaring.fqn, &this_for(&declaring, &owner), &declared.name, depth.max(1))?
    };
    let implementations_list = if declared.is_abstract {
        walk.implementors_of(&declaring.fqn, &declared.name)
    } else {
        Vec::new()
    };
    let shown = format!("{}::{}", declaring.fqn, declared.name);

    if format == "json" {
        return print_json(&serde_json::json!({
            "method": shown,
            "path": path,
            "line": declared.line,
            "end_line": declared.end_line,
            "abstract": declared.is_abstract,
            "implementations": implementations_list.iter().map(|i| &i.fqn).collect::<Vec<_>>(),
            "depth": depth.max(1),
            "calls": calls,
        }));
    }
    println!(
        "{}  {}",
        shown.bold(),
        format!("{path}:{}-{}", declared.line, declared.end_line).cyan()
    );
    if declared.is_abstract {
        println!("  abstract — implementations:");
        for implementor in &implementations_list {
            println!("    {}  {}", implementor.fqn, implementor.at());
        }
    }
    print_tree(&calls, 1);
    Ok(())
}

fn print_tree(nodes: &[CallNode], level: usize) {
    let indent = "  ".repeat(level);
    for n in nodes {
        let target = match &n.call {
            Some(call) => short_call(call),
            None => format!("? {}->{}()", n.receiver, n.method),
        };
        let mut notes = Vec::new();
        if n.resolved_by != "unknown" && n.kind != "function" {
            notes.push(n.resolved_by.to_string());
        }
        if let Some(d) = &n.declared_in {
            notes.push(format!("in {}", short_call(d)));
        }
        if n.undeclared {
            notes.push("not declared in the index".into());
        }
        if !n.constants.is_empty() {
            let c: Vec<&str> = n.constants.iter().map(|c| c.name.as_str()).collect();
            notes.push(c.join(", "));
        }
        if !n.implementations.is_empty() {
            let i: Vec<String> = n.implementations.iter().map(|f| short_call(f)).collect();
            notes.push(format!("impl: {}", i.join(", ")));
        }
        if n.expanded_above {
            notes.push("expanded above".into());
        }
        if n.stopped {
            notes.push("stopped".into());
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!("  [{}]", notes.join("; "))
        };
        let target = if n.call.is_some() {
            target.yellow().to_string()
        } else {
            target.dimmed().to_string()
        };
        println!("{indent}-> {target}  {}:{}{notes}", n.path, n.line);
        if let Some(children) = &n.children {
            print_tree(children, level + 1);
        }
    }
}

/// `Ns\Class::method` → `Class::method`.
fn short_call(call: &str) -> String {
    match call.rsplit_once("::") {
        Some((class, method)) => format!("{}::{method}", crate::parsers::php_ast::short_name(class)),
        None => crate::parsers::php_ast::short_name(call).to_string(),
    }
}
