//! Code analysis commands
//!
//! - unused-symbols: Find potentially unused public symbols

use std::path::Path;

use anyhow::Result;
use colored::Colorize;
use rusqlite::params;

use crate::db;

/// Find potentially unused symbols in a module or project
pub fn cmd_unused_symbols(
    root: &Path,
    module: Option<&str>,
    export_only: bool,
    limit: usize,
    format: &str,
) -> Result<()> {
    if !db::db_exists(root) {
        println!(
            "{}",
            "Index not found. Run 'ast-index rebuild' first.".red()
        );
        return Ok(());
    }

    let conn = db::open_db_leased(root)?;

    // `--module` accepts a module name (`features.surge.impl`, `:core:utils`)
    // as well as a raw path prefix; names resolve to the module's directory.
    let module_path = match module {
        Some(m) => match db::find_module_id_by_name(&conn, m)? {
            Some(id) => {
                db::get_module_path(&conn, id)?.map(|p| format!("{}/", p.trim_end_matches('/')))
            }
            None => Some(m.to_string()),
        },
        None => None,
    };

    // Build query based on filters
    let (sql, filter_param) = if let Some(mod_path) = module_path.as_deref() {
        // `--export-only` narrows a module scan too; it used to be ignored with `--module`.
        let sql = if export_only {
            r#"
            SELECT s.name, s.qualified_name, s.kind, s.line, s.signature, f.path
            FROM symbols s
            JOIN files f ON s.file_id = f.id
            WHERE f.path LIKE ?1
              AND s.kind IN ('class', 'interface', 'trait', 'function', 'object', 'enum', 'protocol', 'struct')
              AND s.name GLOB '[A-Z]*'
            ORDER BY f.path, s.line
            "#
        } else {
            r#"
            SELECT s.name, s.qualified_name, s.kind, s.line, s.signature, f.path
            FROM symbols s
            JOIN files f ON s.file_id = f.id
            WHERE f.path LIKE ?1
              AND s.kind IN ('class', 'interface', 'trait', 'function', 'object', 'enum', 'protocol', 'struct')
            ORDER BY f.path, s.line
            "#
        };
        (sql, Some(format!("{}%", mod_path)))
    } else if export_only {
        (
            r#"
            SELECT s.name, s.qualified_name, s.kind, s.line, s.signature, f.path
            FROM symbols s
            JOIN files f ON s.file_id = f.id
            WHERE s.kind IN ('class', 'interface', 'trait', 'function', 'object', 'enum', 'protocol', 'struct')
              AND s.name GLOB '[A-Z]*'
            ORDER BY f.path, s.line
            "#,
            None,
        )
    } else {
        (
            r#"
            SELECT s.name, s.qualified_name, s.kind, s.line, s.signature, f.path
            FROM symbols s
            JOIN files f ON s.file_id = f.id
            WHERE s.kind IN ('class', 'interface', 'trait', 'function', 'object', 'enum', 'protocol', 'struct')
            ORDER BY f.path, s.line
            "#,
            None,
        )
    };

    let mut stmt = conn.prepare(sql)?;
    let symbols: Vec<db::SearchResult> = if let Some(ref pattern) = filter_param {
        stmt.query_map(params![pattern], |row| {
            Ok(db::SearchResult {
                name: row.get(0)?,
                qualified_name: row.get(1)?,
                kind: row.get(2)?,
                line: row.get(3)?,
                signature: row.get(4)?,
                path: row.get(5)?,
                root_path: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
    } else {
        stmt.query_map([], |row| {
            Ok(db::SearchResult {
                name: row.get(0)?,
                qualified_name: row.get(1)?,
                kind: row.get(2)?,
                line: row.get(3)?,
                signature: row.get(4)?,
                path: row.get(5)?,
                root_path: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
    };

    // Entry points a framework calls by convention: files in `unused_ignore`, names in
    // `unused_ignore_names`.
    let config = crate::indexer::load_config_quiet(root);
    let ignore_names = config
        .as_ref()
        .and_then(|c| c.unused_ignore_names.clone())
        .unwrap_or_default();
    let ignore = config
        .and_then(|c| c.unused_ignore)
        .filter(|patterns| !patterns.is_empty())
        .and_then(|patterns| {
            let mut gb = ignore::gitignore::GitignoreBuilder::new(root);
            for p in &patterns {
                gb.add_line(None, p).ok();
            }
            gb.build().ok()
        });

    // Check each symbol for references
    let mut unused: Vec<&db::SearchResult> = Vec::new();
    // PHP classes referenced only where a DI config registers them, never requested.
    let mut di_only: Vec<&db::SearchResult> = Vec::new();

    for sym in &symbols {
        let is_php = sym.path.ends_with(".php") || sym.path.ends_with(".phtml");
        // Magic methods (`__construct`, `__invoke`, …) are called by the runtime.
        if is_php && sym.name.starts_with("__") {
            continue;
        }
        if ignore_names.iter().any(|p| wildcard_match(p, &sym.name))
            || ignore.as_ref().is_some_and(|m| {
                m.matched_path_or_any_parents(root.join(&sym.path), false)
                    .is_ignore()
            })
        {
            continue;
        }
        // PHP classes: references resolved to this exact FQN from other files, so a dead class
        // with a live namesake elsewhere is still reported.
        let php_class = sym.qualified_name.is_some()
            && is_php
            && matches!(
                sym.kind.as_str(),
                "class" | "interface" | "trait" | "enum" | "object"
            );
        if php_class {
            let mut stmt = conn.prepare_cached(
                "SELECT f.path, r.context FROM refs r JOIN files f ON f.id = r.file_id
                 WHERE r.fqn = ?1 COLLATE NOCASE AND f.path <> ?2",
            )?;
            let refs = stmt
                .query_map(params![sym.qualified_name, sym.path], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            if refs.is_empty() {
                unused.push(sym);
            } else if refs
                .iter()
                .all(|(path, context)| is_di_registration(path, context))
            {
                di_only.push(sym);
            } else {
                continue;
            }
            if unused.len() + di_only.len() >= limit {
                break;
            }
            continue;
        }

        // Check refs table
        let ref_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM refs WHERE name = ?1 LIMIT 1",
                params![sym.name],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if ref_count > 0 {
            continue;
        }

        // Check xml_usages
        let xml_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM xml_usages WHERE class_name = ?1 LIMIT 1",
                params![sym.name],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if xml_count > 0 {
            continue;
        }

        // Check storyboard_usages
        let sb_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM storyboard_usages WHERE class_name = ?1 LIMIT 1",
                params![sym.name],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if sb_count > 0 {
            continue;
        }

        unused.push(sym);
        if unused.len() >= limit {
            break;
        }
    }

    if format == "json" {
        #[derive(serde::Serialize)]
        struct Row<'a> {
            #[serde(flatten)]
            symbol: &'a db::SearchResult,
            #[serde(skip_serializing_if = "Option::is_none")]
            reason: Option<&'static str>,
        }
        let rows: Vec<Row> = unused
            .iter()
            .map(|s| Row {
                symbol: s,
                reason: None,
            })
            .chain(di_only.iter().map(|s| Row {
                symbol: s,
                reason: Some("registered in DI only"),
            }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    let scope = module.unwrap_or("project");
    println!(
        "{}",
        format!(
            "Potentially unused symbols in '{}' ({}/{} checked):",
            scope,
            unused.len(),
            symbols.len()
        )
        .bold()
    );

    for s in &unused {
        println!("  {} [{}]: {}:{}", s.name.yellow(), s.kind, s.path, s.line);
    }

    if unused.is_empty() {
        println!("  No unused symbols found.");
    }
    if !di_only.is_empty() {
        println!(
            "{}",
            format!(
                "Registered in DI config only, never requested ({}):",
                di_only.len()
            )
            .bold()
        );
        for s in &di_only {
            println!("  {} [{}]: {}:{}", s.name.yellow(), s.kind, s.path, s.line);
        }
    }

    Ok(())
}

/// `pattern` with `*` matching any run of characters, against the whole `name`.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(pos) => rest = &rest[pos + part.len()..],
            None => return false,
        }
    }
    true
}

/// A reference in a DI config that registers the class (`Foo::class => [...]`,
/// `'className' => Foo::class`) rather than requesting it (`->get(Foo::class)`, `new Foo`).
fn is_di_registration(path: &str, context: &str) -> bool {
    let di_config = path.contains("/di/") || path.ends_with(".settings.php");
    di_config && !context.contains("get(") && !context.contains("new ")
}

#[cfg(test)]
mod tests {
    use super::wildcard_match;

    #[test]
    fn wildcard_names() {
        assert!(wildcard_match("*Action", "getListAction"));
        assert!(!wildcard_match("*Action", "Actions"));
        assert!(wildcard_match("getObjectClass", "getObjectClass"));
        assert!(wildcard_match("on*Handler", "onBeforeSaveHandler"));
        assert!(!wildcard_match("on*Handler", "onSave"));
        assert!(wildcard_match("*", "anything"));
    }
}
