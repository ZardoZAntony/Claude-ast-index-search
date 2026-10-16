//! PHP classes of the index read structurally on demand: class models for following calls,
//! class constants and enum cases for folding values, functions by name.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rusqlite::{params, Connection};

use super::php::{definitions, read_lossy};
use crate::parsers::php_ast::{self, Names};
use crate::parsers::php_calls::Classes;
use crate::parsers::php_eval::{Constants, Evaluator, Val};
use crate::parsers::php_model::{self, ClassModel};

/// Classes of the index, parsed on demand from their files.
pub(super) struct IndexClasses<'a> {
    conn: &'a Connection,
    root: &'a Path,
    models: RefCell<HashMap<String, Option<Rc<ClassModel>>>>,
    /// Lowercased FQN → (path for output, file content).
    sources: RefCell<HashMap<String, (String, Rc<String>)>>,
    files: RefCell<HashMap<PathBuf, Rc<String>>>,
}

impl<'a> IndexClasses<'a> {
    pub(super) fn new(conn: &'a Connection, root: &'a Path) -> Self {
        IndexClasses {
            conn,
            root,
            models: RefCell::new(HashMap::new()),
            sources: RefCell::new(HashMap::new()),
            files: RefCell::new(HashMap::new()),
        }
    }

    /// Path (for output) and content of the file declaring `fqn`.
    pub(super) fn source(&self, fqn: &str) -> Option<(String, Rc<String>)> {
        self.class(fqn)?;
        self.sources.borrow().get(&fqn.to_ascii_lowercase()).cloned()
    }

    pub(super) fn read(&self, abs: &Path) -> Option<Rc<String>> {
        if let Some(content) = self.files.borrow().get(abs) {
            return Some(content.clone());
        }
        let content = Rc::new(read_lossy(abs)?);
        self.files
            .borrow_mut()
            .insert(abs.to_path_buf(), content.clone());
        Some(content)
    }

    /// `path:start-end` of a function declared outside any class.
    pub(super) fn function_at(&self, name: &str) -> Option<String> {
        let short = php_ast::short_name(name);
        self.conn
            .query_row(
                "SELECT f.path, s.line, s.end_line FROM symbols s JOIN files f ON f.id = s.file_id
                 WHERE s.kind = 'function' AND s.name = ?1 COLLATE NOCASE
                   AND NOT EXISTS (SELECT 1 FROM symbols c WHERE c.file_id = s.file_id
                       AND c.kind IN ('class', 'interface', 'trait', 'enum')
                       AND c.line <= s.line AND COALESCE(c.end_line, c.line) >= s.line)
                 ORDER BY f.path LIMIT 1",
                params![short],
                |row| {
                    let path: String = row.get(0)?;
                    let line: i64 = row.get(1)?;
                    let end: Option<i64> = row.get(2)?;
                    Ok(format!("{path}:{line}-{}", end.unwrap_or(line)))
                },
            )
            .ok()
    }

    /// The value of constant or enum case `name` declared in `fqn` itself.
    fn own_constant(&self, fqn: &str, name: &str, depth: usize) -> Option<Val> {
        let (_, content) = self.source(fqn)?;
        let tree = php_ast::parse(&content).ok()?;
        let root = tree.root_node();
        let names = Names::of(root, &content);
        let class = php_ast::class_like_nodes(root).into_iter().find(|n| {
            php_model::declared_fqn(*n, &content, &names)
                .is_some_and(|f| f.eq_ignore_ascii_case(fqn))
        })?;
        let body = class.child_by_field_name("body")?;
        let evaluator = Evaluator {
            class: Some(fqn.to_string()),
            depth,
            ..Evaluator::new(&content, &names, self)
        };
        for member in php_ast::named_children(body) {
            match member.kind() {
                "const_declaration" => {
                    for element in php_ast::named_children(member) {
                        if element.kind() != "const_element" {
                            continue;
                        }
                        let parts = php_ast::named_children(element);
                        if let (Some(n), Some(value)) = (parts.first(), parts.get(1)) {
                            if php_ast::text(*n, &content) == name {
                                return Some(evaluator.eval(*value));
                            }
                        }
                    }
                }
                "enum_case" => {
                    let matches = member
                        .child_by_field_name("name")
                        .is_some_and(|n| php_ast::text(n, &content) == name);
                    if matches {
                        let value = member
                            .child_by_field_name("value")
                            .map(|v| Box::new(evaluator.eval(v)));
                        return Some(Val::Case {
                            fqn: format!("{fqn}::{name}"),
                            value,
                        });
                    }
                }
                _ => {}
            }
        }
        None
    }
}

impl Classes for IndexClasses<'_> {
    fn class(&self, fqn: &str) -> Option<Rc<ClassModel>> {
        let key = fqn.to_ascii_lowercase();
        if let Some(found) = self.models.borrow().get(&key) {
            return found.clone();
        }
        let mut found = None;
        for def in definitions(self.conn, self.root, fqn).unwrap_or_default() {
            let Some(content) = self.read(&def.abs) else {
                continue;
            };
            let Ok(models) = php_model::class_models(&content) else {
                continue;
            };
            if let Some(model) = models.into_iter().find(|m| m.fqn.eq_ignore_ascii_case(fqn)) {
                self.sources
                    .borrow_mut()
                    .insert(key.clone(), (def.path.clone(), content));
                found = Some(Rc::new(model));
                break;
            }
        }
        self.models.borrow_mut().insert(key, found.clone());
        found
    }
}

impl Constants for IndexClasses<'_> {
    /// Declared in the class, or inherited from a parent, an interface or a trait.
    fn class_constant(&self, class: &str, name: &str, depth: usize) -> Option<Val> {
        let mut seen = HashSet::new();
        let mut queue = vec![class.to_string()];
        while let Some(current) = queue.pop() {
            if !seen.insert(current.to_ascii_lowercase()) {
                continue;
            }
            let Some(model) = self.class(&current) else {
                continue;
            };
            let declares = model.constants.iter().any(|c| c.name == name)
                || model.cases.iter().any(|c| c.name == name);
            if declares {
                return self.own_constant(&model.fqn, name, depth);
            }
            queue.extend(model.parent.iter().cloned());
            queue.extend(model.interfaces.iter().cloned());
            queue.extend(model.traits.iter().cloned());
        }
        None
    }
}
