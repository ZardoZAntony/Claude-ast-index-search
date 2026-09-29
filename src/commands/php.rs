//! PHP refactoring queries built on fully qualified names.
//!
//! - `usages <FQN>`: references of one class, not of every class sharing its short name;
//! - `impact <FQN>`: every place a rename or signature change touches, with the kind of use;
//! - `move-plan <FQN> <namespace>`: the edits that keep code working after a namespace move;
//! - `duplicates`: same-named classes whose bodies are near copies, with usage of each copy;
//! - `callers Type::method`: calls whose receiver resolves to the type or one of its subtypes.
//!
//! PHP resolves class names per file (`namespace` + `use`); the index keeps that resolution in
//! `refs.fqn` and `symbols.qualified_name`, so these answers need no grep over the tree. Config
//! files (.neon, .yaml, .json, .xml) are not indexed; they are scanned live for the FQN.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{bail, Result};
use colored::Colorize;
use regex::Regex;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::db;
use crate::indexer;

const CONFIG_EXTENSIONS: &[&str] = &["neon", "yaml", "yml", "json", "xml"];
const MAX_CONFIG_FILE_BYTES: u64 = 1_000_000;
const CLASS_KINDS: &str = "('class', 'interface', 'enum', 'object')";
const COVERAGE: &str = "PHP code, use imports, PHPDoc types, FQN strings in PHP, \
config files (.neon .yaml .yml .json .xml); not covered: dynamically built class names, \
files excluded from the index";

// ---------------------------------------------------------------------------
// Shared lookups
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct RefRow {
    path: String,
    line: i64,
    kind: &'static str,
    test: bool,
    context: String,
    #[serde(skip)]
    file_id: i64,
}

#[derive(Serialize, Clone)]
struct DefRow {
    path: String,
    line: i64,
    kind: String,
    #[serde(skip)]
    file_id: i64,
    #[serde(skip)]
    abs: PathBuf,
}

#[derive(Serialize, Clone)]
struct ConfigMention {
    path: String,
    line: usize,
    text: String,
}

fn open(root: &Path) -> Result<db::LeasedConnection> {
    if !db::db_exists(root) {
        bail!("Index not found. Run 'ast-index rebuild' first.");
    }
    let conn = db::open_db_leased(root)?;
    let has_fqn: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM refs WHERE fqn IS NOT NULL)",
        [],
        |row| row.get(0),
    )?;
    if !has_fqn {
        bail!("The index has no fully qualified names yet. Run 'ast-index rebuild'.");
    }
    Ok(conn)
}

fn normalize_fqn(fqn: &str) -> String {
    fqn.trim().trim_start_matches('\\').to_string()
}

fn short_name(fqn: &str) -> &str {
    fqn.rsplit('\\').next().unwrap_or(fqn)
}

fn namespace_of(fqn: &str) -> &str {
    fqn.rsplit_once('\\').map(|(ns, _)| ns).unwrap_or("")
}

/// Path for output (relative to the project root when inside it) and the absolute path.
fn locate(root: &Path, root_path: &str, path: &str) -> (String, PathBuf) {
    let base = if root_path.is_empty() {
        root.to_path_buf()
    } else {
        PathBuf::from(root_path)
    };
    let abs = base.join(path);
    let shown = abs
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| abs.to_string_lossy().into_owned());
    (shown, abs)
}

fn is_test_path(path: &str) -> bool {
    path.starts_with(".tests/")
        || path.starts_with("tests/")
        || path.contains("/tests/")
        || path.contains("/Tests/")
        || path.ends_with("Test.php")
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

static NEW_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bnew\s+\\?[\w\\]+").unwrap());

/// Kind of use on a reference line, from its text.
fn classify(context: &str, short: &str) -> &'static str {
    let t = context.trim_start();
    if t.starts_with("use ") {
        return "import";
    }
    if t.starts_with('*') || t.starts_with("/*") {
        return "phpdoc";
    }
    if t.starts_with("#[") {
        return "attribute";
    }
    if t.contains(&format!("{short}::class")) {
        return "::class";
    }
    if NEW_RE.find_iter(t).any(|m| {
        m.as_str()
            .rsplit('\\')
            .next()
            .unwrap_or("")
            .ends_with(short)
    }) {
        return "new";
    }
    if t.contains(&format!("{short}::")) {
        return "static";
    }
    if t.contains(" extends ") || t.contains(" implements ") {
        return "inheritance";
    }
    if t.contains("instanceof") || t.contains("catch (") || t.starts_with("catch") {
        return "check";
    }
    if (t.contains('\'') || t.contains('"')) && t.contains(&format!("\\{short}")) {
        return "string";
    }
    "type"
}

fn refs_by_fqn(conn: &Connection, root: &Path, fqn: &str) -> Result<Vec<RefRow>> {
    let short = short_name(fqn).to_string();
    let mut stmt = conn.prepare(
        "SELECT f.path, f.root_path, r.line, r.context, r.file_id
         FROM refs r JOIN files f ON f.id = r.file_id
         WHERE r.fqn = ?1 COLLATE NOCASE
         ORDER BY f.path, r.line",
    )?;
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    let mapped = stmt.query_map(params![fqn], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    for item in mapped {
        let (path, root_path, line, context, file_id) = item?;
        if !seen.insert((file_id, line)) {
            continue;
        }
        let (shown, _) = locate(root, root_path.as_deref().unwrap_or(""), &path);
        let context = context.unwrap_or_default();
        rows.push(RefRow {
            test: is_test_path(&shown),
            kind: classify(&context, &short),
            context,
            path: shown,
            line,
            file_id,
        });
    }
    rows.sort_by(|a, b| (a.test, &a.path, a.line).cmp(&(b.test, &b.path, b.line)));
    Ok(rows)
}

fn definitions(conn: &Connection, root: &Path, fqn: &str) -> Result<Vec<DefRow>> {
    let sql = format!(
        "SELECT f.path, f.root_path, s.line, s.kind, s.file_id
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.qualified_name = ?1 COLLATE NOCASE AND s.kind IN {CLASS_KINDS}
         ORDER BY f.path"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![fqn], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(path, root_path, line, kind, file_id)| {
            let (shown, abs) = locate(root, root_path.as_deref().unwrap_or(""), &path);
            DefRow {
                path: shown,
                line,
                kind,
                file_id,
                abs,
            }
        })
        .collect())
}

/// FQN a class name on a given line resolved to, if the index recorded it.
fn fqn_at(conn: &Connection, file_id: i64, line: i64, name: &str) -> Option<String> {
    conn.query_row(
        "SELECT fqn FROM refs WHERE file_id = ?1 AND line = ?2 AND name = ?3 COLLATE NOCASE
           AND fqn IS NOT NULL LIMIT 1",
        params![file_id, line, name],
        |row| row.get(0),
    )
    .ok()
}

fn read_lossy(path: &Path) -> Option<String> {
    fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Lines of non-indexed config files that mention any of `needles`.
fn config_mentions(root: &Path, needles: &[String]) -> Vec<ConfigMention> {
    let config = indexer::load_config_quiet(root);
    let hidden = indexer::HiddenPolicy::for_root(root);
    let exclude = config
        .and_then(|c| c.exclude)
        .filter(|patterns| !patterns.is_empty())
        .and_then(|patterns| {
            let mut gb = ignore::gitignore::GitignoreBuilder::new(root);
            for p in &patterns {
                gb.add_line(None, p).ok();
            }
            gb.build().ok()
        });
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(hidden.skips_all_hidden())
        .git_ignore(true)
        .git_exclude(true)
        .filter_entry(move |entry| {
            if !hidden.allows(entry) || indexer::is_excluded_dir(entry) {
                return false;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            !exclude
                .as_ref()
                .is_some_and(|m| m.matched(entry.path(), is_dir).is_ignore())
        });

    let mut out = Vec::new();
    for entry in builder.build().filter_map(|e| e.ok()) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let is_config = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| CONFIG_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !is_config
            || entry
                .metadata()
                .map_or(true, |m| m.len() > MAX_CONFIG_FILE_BYTES)
        {
            continue;
        }
        let Some(content) = read_lossy(path) else {
            continue;
        };
        if !needles.iter().any(|n| content.contains(n.as_str())) {
            continue;
        }
        let shown = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        for (i, line) in content.lines().enumerate() {
            if needles.iter().any(|n| line.contains(n.as_str())) {
                out.push(ConfigMention {
                    path: shown.clone(),
                    line: i + 1,
                    text: truncate(line, 160),
                });
            }
        }
    }
    out.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    out
}

/// How an FQN appears in config text: plain, JSON/PHP-escaped, and regex-escaped (PHPStan).
fn fqn_needles(fqn: &str) -> Vec<String> {
    let mut needles = vec![
        fqn.to_string(),
        fqn.replace('\\', "\\\\"),
        fqn.replace('\\', "\\\\\\\\"),
    ];
    needles.dedup();
    needles
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// usages <FQN>
// ---------------------------------------------------------------------------

pub fn cmd_usages_fqn(root: &Path, fqn: &str, limit: usize, format: &str) -> Result<()> {
    let _lease = db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let fqn = normalize_fqn(fqn);
    let refs = refs_by_fqn(&conn, root, &fqn)?;
    let total = refs.len();
    let shown: Vec<&RefRow> = refs.iter().take(limit).collect();

    if format == "json" {
        return print_json(&serde_json::json!({
            "fqn": fqn,
            "items": shown,
            "pagination": {"total": total, "returned": shown.len(), "truncated": total > shown.len(), "limit": limit},
        }));
    }
    println!(
        "{}",
        format!("Usages of {fqn} (showing {} of {total}):", shown.len()).bold()
    );
    for r in &shown {
        println!(
            "  {}:{}  {}",
            r.path.cyan(),
            r.line,
            truncate(&r.context, 100)
        );
    }
    if total > shown.len() {
        println!(
            "  … {} more; rerun with a larger --limit",
            total - shown.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// impact <FQN>
// ---------------------------------------------------------------------------

pub fn cmd_impact(root: &Path, fqn: &str, format: &str) -> Result<()> {
    let _lease = db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let fqn = normalize_fqn(fqn);
    let defs = definitions(&conn, root, &fqn)?;
    let refs = refs_by_fqn(&conn, root, &fqn)?;
    let mut needles = fqn_needles(&fqn);
    for d in &defs {
        needles.push(path_tail(&d.path));
    }
    let config = config_mentions(root, &needles);

    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &refs {
        *by_kind.entry(r.kind).or_default() += 1;
    }
    let files: HashSet<&str> = refs.iter().map(|r| r.path.as_str()).collect();
    let tests = refs.iter().filter(|r| r.test).count();

    if format == "json" {
        return print_json(&serde_json::json!({
            "fqn": fqn,
            "definitions": defs,
            "references": refs,
            "summary": {"references": refs.len(), "files": files.len(), "tests": tests, "by_kind": by_kind},
            "config_mentions": config,
            "coverage": COVERAGE,
        }));
    }

    println!("{}", format!("Impact of {fqn}").bold());
    if defs.is_empty() {
        println!("  definition: not found in the index");
    }
    for d in &defs {
        println!("  definition: {}:{} ({})", d.path.cyan(), d.line, d.kind);
    }
    let kinds: Vec<String> = by_kind.iter().map(|(k, n)| format!("{k} {n}")).collect();
    println!(
        "  {} references in {} files: {} (tests: {tests})",
        refs.len(),
        files.len(),
        kinds.join(", ")
    );
    let mut current = "";
    for r in &refs {
        if r.path != current {
            current = &r.path;
            println!("{}", r.path.cyan());
        }
        println!(
            "  {:>5} {:<11} {}",
            r.line,
            r.kind,
            truncate(&r.context, 100)
        );
    }
    println!("config mentions: {}", config.len());
    for c in &config {
        println!("  {}:{}  {}", c.path.cyan(), c.line, c.text);
    }
    println!("coverage: {COVERAGE}");
    Ok(())
}

/// Last two path components (`Elastic/Foo.php`): how config files usually point at a class file.
fn path_tail(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

// ---------------------------------------------------------------------------
// move-plan <FQN> <namespace>
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct FileEdits {
    path: String,
    edits: Vec<Edit>,
}

#[derive(Serialize)]
struct Edit {
    line: Option<i64>,
    action: String,
}

pub fn cmd_move_plan(root: &Path, fqn: &str, new_namespace: &str, format: &str) -> Result<()> {
    let _lease = db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let fqn = normalize_fqn(fqn);
    let new_ns = normalize_fqn(new_namespace)
        .trim_end_matches('\\')
        .to_string();
    let short = short_name(&fqn).to_string();
    let old_ns = namespace_of(&fqn).to_string();
    let new_fqn = if new_ns.is_empty() {
        short.clone()
    } else {
        format!("{new_ns}\\{short}")
    };

    let defs = definitions(&conn, root, &fqn)?;
    let Some(def) = defs.first() else {
        bail!("{fqn} is not defined in the index");
    };
    let refs = refs_by_fqn(&conn, root, &fqn)?;
    let mut plan: Vec<FileEdits> = Vec::new();

    // The class file: its path, namespace line, and same-namespace classes it used without `use`.
    let mut own = Vec::new();
    let new_path = psr4_path(&def.path, &old_ns, &new_ns);
    own.push(Edit {
        line: None,
        action: match &new_path {
            Some(p) => format!("move file to {p}"),
            None => "move file (new directory: namespace is outside this file's PSR-4 root)".into(),
        },
    });
    let content = read_lossy(&def.abs).unwrap_or_default();
    if let Some(n) = content
        .lines()
        .position(|l| l.trim_start().starts_with("namespace "))
    {
        own.push(Edit {
            line: Some(n as i64 + 1),
            action: format!("namespace {new_ns};"),
        });
    }
    for (dep, lines) in implicit_same_namespace_deps(&conn, def.file_id, &old_ns, &fqn)? {
        own.push(Edit {
            line: None,
            action: format!(
                "add `use {dep};` — same-namespace class used without import on lines {lines}"
            ),
        });
    }
    plan.push(FileEdits {
        path: def.path.clone(),
        edits: own,
    });

    // Referencing files.
    let mut by_file: BTreeMap<(bool, String), Vec<&RefRow>> = BTreeMap::new();
    for r in refs.iter().filter(|r| r.file_id != def.file_id) {
        by_file.entry((r.test, r.path.clone())).or_default().push(r);
    }
    let mut untouched = 0usize;
    for ((_, path), rows) in by_file {
        let file_id = rows[0].file_id;
        let mut edits = Vec::new();
        let imports: Vec<&&RefRow> = rows.iter().filter(|r| r.kind == "import").collect();
        for r in &imports {
            edits.push(Edit {
                line: Some(r.line),
                action: format!("use {new_fqn}; (was: {})", truncate(&r.context, 90)),
            });
        }
        let mut inline = 0;
        for r in rows.iter().filter(|r| r.kind != "import") {
            let has_fqn =
                r.context.contains(&fqn) || r.context.contains(&fqn.replace('\\', "\\\\"));
            if has_fqn {
                inline += 1;
                edits.push(Edit {
                    line: Some(r.line),
                    action: format!("replace {fqn} → {new_fqn} ({})", r.kind),
                });
            }
        }
        let resolved_by_namespace = imports.is_empty()
            && file_namespace(&conn, file_id)?.eq_ignore_ascii_case(&old_ns)
            && rows.len() > inline;
        if resolved_by_namespace {
            let lines: Vec<String> = rows.iter().map(|r| r.line.to_string()).collect();
            edits.push(Edit {
                line: None,
                action: format!(
                    "add `use {new_fqn};` — same namespace, no import (lines {})",
                    lines.join(", ")
                ),
            });
        }
        untouched += rows.len().saturating_sub(edits.len());
        if !edits.is_empty() {
            plan.push(FileEdits { path, edits });
        }
    }

    let mut needles = fqn_needles(&fqn);
    needles.push(path_tail(&def.path));
    let config = config_mentions(root, &needles);
    let mut config_files: BTreeMap<String, Vec<Edit>> = BTreeMap::new();
    for c in &config {
        config_files.entry(c.path.clone()).or_default().push(Edit {
            line: Some(c.line as i64),
            action: format!("update: {}", c.text),
        });
    }
    for (path, edits) in config_files {
        plan.push(FileEdits { path, edits });
    }

    let edit_count: usize = plan.iter().map(|f| f.edits.len()).sum();
    if format == "json" {
        return print_json(&serde_json::json!({
            "fqn": fqn,
            "new_fqn": new_fqn,
            "files": plan,
            "summary": {"edits": edit_count, "files": plan.len(), "references_resolved_through_import": untouched},
            "coverage": COVERAGE,
        }));
    }
    println!("{}", format!("Move {fqn} → {new_fqn}").bold());
    for f in &plan {
        println!("{}", f.path.cyan());
        for e in &f.edits {
            match e.line {
                Some(l) => println!("  {l:>5}  {}", e.action),
                None => println!("         {}", e.action),
            }
        }
    }
    println!(
        "{edit_count} edits in {} files; {untouched} other references resolve through the updated imports",
        plan.len()
    );
    println!("coverage: {COVERAGE}");
    Ok(())
}

/// New file path under PSR-4: the directory tail that mirrors the namespace tail is re-rooted.
fn psr4_path(path: &str, old_ns: &str, new_ns: &str) -> Option<String> {
    let (dir, file) = path.rsplit_once('/')?;
    let dir_parts: Vec<&str> = dir.split('/').collect();
    let ns_parts: Vec<&str> = old_ns.split('\\').filter(|s| !s.is_empty()).collect();
    let mut matched = 0;
    while matched < dir_parts.len()
        && matched < ns_parts.len()
        && dir_parts[dir_parts.len() - 1 - matched] == ns_parts[ns_parts.len() - 1 - matched]
    {
        matched += 1;
    }
    let base = dir_parts[..dir_parts.len() - matched].join("/");
    let prefix = ns_parts[..ns_parts.len() - matched].join("\\");
    let rest = if prefix.is_empty() {
        new_ns
    } else {
        new_ns
            .strip_prefix(&prefix)?
            .strip_prefix('\\')
            .unwrap_or("")
    };
    let rel = rest.replace('\\', "/");
    Some(
        [base.as_str(), rel.as_str(), file]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn file_namespace(conn: &Connection, file_id: i64) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT name FROM symbols WHERE file_id = ?1 AND kind = 'package' ORDER BY line LIMIT 1",
            params![file_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default())
}

/// Classes of `old_ns` that the file uses without importing them (they resolve by namespace).
fn implicit_same_namespace_deps(
    conn: &Connection,
    file_id: i64,
    old_ns: &str,
    own_fqn: &str,
) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT fqn, line, context FROM refs WHERE file_id = ?1 AND fqn IS NOT NULL ORDER BY line",
    )?;
    let rows = stmt
        .query_map(params![file_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let imported: HashSet<String> = rows
        .iter()
        .filter(|(_, _, c)| c.trim_start().starts_with("use "))
        .map(|(f, _, _)| f.to_ascii_lowercase())
        .collect();
    let known_class = |fqn: &str| -> bool {
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM symbols WHERE qualified_name = ?1 COLLATE NOCASE AND kind IN {CLASS_KINDS})"
        );
        conn.query_row(&sql, params![fqn], |row| row.get(0))
            .unwrap_or(false)
    };
    let mut deps: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for (fqn, line, _) in &rows {
        if fqn.eq_ignore_ascii_case(own_fqn)
            || !namespace_of(fqn).eq_ignore_ascii_case(old_ns)
            || imported.contains(&fqn.to_ascii_lowercase())
            || !known_class(fqn)
        {
            continue;
        }
        let lines = deps.entry(fqn.clone()).or_default();
        if !lines.contains(line) {
            lines.push(*line);
        }
    }
    Ok(deps
        .into_iter()
        .map(|(fqn, lines)| {
            let lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            (fqn, lines.join(", "))
        })
        .collect())
}

// ---------------------------------------------------------------------------
// duplicates
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CopyInfo {
    fqn: String,
    path: String,
    line: i64,
    references: usize,
    test_references: usize,
}

#[derive(Serialize)]
struct DuplicatePair {
    name: String,
    similarity: f64,
    lines: usize,
    contract_mirror: bool,
    a: CopyInfo,
    b: CopyInfo,
}

pub fn cmd_duplicates(
    root: &Path,
    path_prefix: Option<&str>,
    min_similarity: f64,
    min_lines: usize,
    limit: usize,
    format: &str,
) -> Result<()> {
    let _lease = db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let sql = format!(
        "SELECT s.name, s.qualified_name, s.line, f.path, f.root_path
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.qualified_name IS NOT NULL AND s.kind IN {CLASS_KINDS}
           AND (f.path LIKE '%.php' OR f.path LIKE '%.phtml')
           AND (?1 IS NULL OR f.path LIKE ?1 || '%')"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![path_prefix], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut groups: HashMap<String, Vec<(String, String, i64, PathBuf)>> = HashMap::new();
    for (name, fqn, line, path, root_path) in rows {
        let (shown, abs) = locate(root, root_path.as_deref().unwrap_or(""), &path);
        groups
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push((fqn, shown, line, abs));
    }

    let mut usage_cache: HashMap<String, (usize, usize)> = HashMap::new();
    let mut usage = |fqn: &str, own_path: &str| -> Result<(usize, usize)> {
        if let Some(v) = usage_cache.get(fqn) {
            return Ok(*v);
        }
        let refs = refs_by_fqn(&conn, root, fqn)?;
        let outside: Vec<&RefRow> = refs.iter().filter(|r| r.path != own_path).collect();
        let tests = outside.iter().filter(|r| r.test).count();
        let v = (outside.len() - tests, tests);
        usage_cache.insert(fqn.to_string(), v);
        Ok(v)
    };

    let mut pairs = Vec::new();
    for members in groups.values().filter(|m| m.len() > 1) {
        let bodies: Vec<Option<ClassBody>> = members
            .iter()
            .map(|(_, _, line, abs)| {
                read_lossy(abs).map(|c| {
                    let lines = class_body(&c, *line as usize);
                    let tokens = token_counts(&lines);
                    (lines, tokens)
                })
            })
            .collect();
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let (Some((la, ta)), Some((lb, tb))) = (&bodies[i], &bodies[j]) else {
                    continue;
                };
                let size = la.len().max(lb.len());
                if size < min_lines {
                    continue;
                }
                let similarity = dice(ta, tb);
                if similarity < min_similarity {
                    continue;
                }
                let (fa, pa, linea, _) = &members[i];
                let (fb, pb, lineb, _) = &members[j];
                let (ra, rta) = usage(fa, pa)?;
                let (rb, rtb) = usage(fb, pb)?;
                pairs.push(DuplicatePair {
                    name: short_name(fa).to_string(),
                    similarity: (similarity * 100.0).round() / 100.0,
                    lines: size,
                    contract_mirror: pa.contains("/Contracts/") || pb.contains("/Contracts/"),
                    a: CopyInfo {
                        fqn: fa.clone(),
                        path: pa.clone(),
                        line: *linea,
                        references: ra,
                        test_references: rta,
                    },
                    b: CopyInfo {
                        fqn: fb.clone(),
                        path: pb.clone(),
                        line: *lineb,
                        references: rb,
                        test_references: rtb,
                    },
                });
            }
        }
    }
    pairs.sort_by(|x, y| {
        (
            x.contract_mirror,
            -(x.similarity * 100.0) as i64,
            -(x.lines as i64),
            &x.name,
        )
            .cmp(&(
                y.contract_mirror,
                -(y.similarity * 100.0) as i64,
                -(y.lines as i64),
                &y.name,
            ))
    });
    let total = pairs.len();
    pairs.truncate(limit);

    if format == "json" {
        return print_json(&serde_json::json!({
            "pairs": pairs,
            "pagination": {"total": total, "returned": pairs.len(), "truncated": total > pairs.len(), "limit": limit},
        }));
    }
    println!(
        "{}",
        format!(
            "Duplicate classes (similarity ≥ {:.0}%): showing {} of {total} pairs",
            min_similarity * 100.0,
            pairs.len()
        )
        .bold()
    );
    for p in &pairs {
        let tag = if p.contract_mirror {
            " [contract mirror]"
        } else {
            ""
        };
        println!(
            "{:>4.0}%  {}  {} lines{tag}",
            p.similarity * 100.0,
            p.name.bold(),
            p.lines
        );
        for c in [&p.a, &p.b] {
            let unused = if c.references == 0 {
                if c.test_references == 0 {
                    "  UNUSED".red().to_string()
                } else {
                    "  tests only".yellow().to_string()
                }
            } else {
                String::new()
            };
            println!(
                "       {}:{}  refs {} (tests {}){unused}",
                c.path.cyan(),
                c.line,
                c.references,
                c.test_references
            );
        }
    }
    Ok(())
}

/// Normalized class lines and their token counts.
type ClassBody = (Vec<String>, HashMap<String, usize>);

/// Normalized lines of a class: declaration through the matching closing brace, without
/// comments and blank lines.
fn class_body(content: &str, decl_line: usize) -> Vec<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    let mut depth = 0i64;
    let mut opened = false;
    for line in lines.iter().skip(decl_line.saturating_sub(1)) {
        let t = line.trim();
        let is_comment =
            t.starts_with("//") || t.starts_with('#') || t.starts_with('*') || t.starts_with("/*");
        if !t.is_empty() && !is_comment {
            out.push(t.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        if !is_comment {
            for c in t.chars() {
                match c {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
        }
        if opened && depth <= 0 {
            break;
        }
        if out.len() > 5000 {
            break;
        }
    }
    out
}

static TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*|\d+|[^\sA-Za-z0-9_]").unwrap());

fn token_counts(lines: &[String]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for line in lines {
        for m in TOKEN_RE.find_iter(line) {
            *counts.entry(m.as_str().to_string()).or_insert(0) += 1;
        }
    }
    counts
}

/// Dice coefficient over token multisets.
fn dice(a: &HashMap<String, usize>, b: &HashMap<String, usize>) -> f64 {
    let total: usize = a.values().sum::<usize>() + b.values().sum::<usize>();
    if total == 0 {
        return 0.0;
    }
    let common: usize = a
        .iter()
        .map(|(k, n)| (*n).min(*b.get(k).unwrap_or(&0)))
        .sum();
    2.0 * common as f64 / total as f64
}

// ---------------------------------------------------------------------------
// callers Type::method
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CallSite {
    path: String,
    line: i64,
    receiver: Option<String>,
    context: String,
}

#[derive(Serialize)]
struct Declaration {
    path: String,
    line: i64,
    owner: String,
}

pub fn cmd_typed_callers(root: &Path, spec: &str, limit: usize, format: &str) -> Result<()> {
    let _lease = db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let (type_part, method) = spec
        .rsplit_once("::")
        .map(|(t, m)| {
            (
                normalize_fqn(t),
                m.trim().trim_end_matches("()").to_string(),
            )
        })
        .unwrap_or_default();
    if type_part.is_empty() || method.is_empty() {
        bail!("expected Type::method");
    }

    let roots = type_fqns(&conn, &type_part)?;
    if roots.is_empty() {
        bail!("type {type_part} is not defined in the index");
    }
    let types = with_subtypes(&conn, &roots)?;
    let types_lower: HashSet<String> = types.iter().map(|t| t.to_ascii_lowercase()).collect();

    // Declarations: the method in each type, plus anonymous classes implementing one of them.
    let mut declarations = Vec::new();
    for fqn in &types {
        for d in definitions(&conn, root, fqn)? {
            let content = read_lossy(&d.abs).unwrap_or_default();
            if let Some(n) = find_method_decl(&content, d.line as usize, &method) {
                declarations.push(Declaration {
                    path: d.path.clone(),
                    line: n as i64,
                    owner: fqn.clone(),
                });
            }
        }
        for r in refs_by_fqn(&conn, root, fqn)? {
            if r.context.contains("new class") {
                let abs = root.join(&r.path);
                let content = read_lossy(&abs).unwrap_or_default();
                if let Some(n) = find_method_decl(&content, r.line as usize, &method) {
                    declarations.push(Declaration {
                        path: r.path.clone(),
                        line: n as i64,
                        owner: format!("anonymous class implementing {}", short_name(fqn)),
                    });
                }
            }
        }
    }

    // Call sites: files with a recorded `method(` reference, plus files that define a method of
    // that name (the generic extractor skips references to names defined in the same file).
    let mut stmt = conn.prepare(
        "SELECT DISTINCT r.file_id, f.path, f.root_path FROM refs r JOIN files f ON f.id = r.file_id
         WHERE r.name = ?1 AND (f.path LIKE '%.php' OR f.path LIKE '%.phtml')
         UNION
         SELECT DISTINCT s.file_id, f.path, f.root_path FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = ?1 AND s.kind = 'function' AND (f.path LIKE '%.php' OR f.path LIKE '%.phtml')",
    )?;
    let files = stmt
        .query_map(params![method], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let call_re = Regex::new(&format!(r"(->|\?->|::)\s*{}\s*\(", regex::escape(&method)))?;
    let mut calls = Vec::new();
    let mut unresolved = Vec::new();
    let mut excluded = Vec::new();
    for (file_id, path, root_path) in files {
        let (shown, abs) = locate(root, root_path.as_deref().unwrap_or(""), &path);
        let Some(content) = read_lossy(&abs) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            let Some(m) = call_re.find(line) else {
                continue;
            };
            if line.contains("function ") {
                continue;
            }
            let line_no = idx as i64 + 1;
            let receiver =
                receiver_type(&conn, file_id, &lines, idx, &line[..m.start()], m.as_str());
            let site = CallSite {
                path: shown.clone(),
                line: line_no,
                receiver: receiver.clone(),
                context: truncate(line, 110),
            };
            match receiver {
                Some(t) if types_lower.contains(&t.to_ascii_lowercase()) => calls.push(site),
                Some(_) => excluded.push(site),
                None => unresolved.push(site),
            }
        }
    }
    let sort = |v: &mut Vec<CallSite>| v.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    sort(&mut calls);
    sort(&mut unresolved);
    sort(&mut excluded);
    let total_calls = calls.len();
    calls.truncate(limit);

    if format == "json" {
        return print_json(&serde_json::json!({
            "type": roots,
            "types": types,
            "method": method,
            "declarations": declarations,
            "calls": calls,
            "unresolved": unresolved,
            "excluded": excluded,
            "pagination": {"total": total_calls, "returned": calls.len(), "truncated": total_calls > calls.len(), "limit": limit},
        }));
    }
    println!(
        "{}",
        format!(
            "{}::{method} — {} type(s) incl. subtypes",
            roots.join(", "),
            types.len()
        )
        .bold()
    );
    println!("declarations: {}", declarations.len());
    for d in &declarations {
        println!("  {}:{}  {}", d.path.cyan(), d.line, d.owner);
    }
    println!(
        "calls on these types: {} (showing {})",
        total_calls,
        calls.len()
    );
    for c in &calls {
        println!(
            "  {}:{}  [{}]  {}",
            c.path.cyan(),
            c.line,
            short_name(c.receiver.as_deref().unwrap_or("")),
            c.context
        );
    }
    println!(
        "receiver not inferred — check by hand: {}",
        unresolved.len()
    );
    for c in &unresolved {
        println!("  {}:{}  {}", c.path.cyan(), c.line, c.context);
    }
    println!("excluded (other types): {}", excluded.len());
    for c in &excluded {
        println!(
            "  {}:{}  [{}]",
            c.path.cyan(),
            c.line,
            c.receiver.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn type_fqns(conn: &Connection, type_part: &str) -> Result<Vec<String>> {
    if type_part.contains('\\') {
        let sql = format!(
            "SELECT DISTINCT qualified_name FROM symbols WHERE qualified_name = ?1 COLLATE NOCASE AND kind IN {CLASS_KINDS}"
        );
        let mut stmt = conn.prepare(&sql)?;
        return Ok(stmt
            .query_map(params![type_part], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?);
    }
    let sql = format!(
        "SELECT DISTINCT qualified_name FROM symbols
         WHERE name = ?1 COLLATE NOCASE AND qualified_name IS NOT NULL AND kind IN {CLASS_KINDS}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let found = stmt
        .query_map(params![type_part], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(found)
}

/// The types plus every class/interface that extends or implements one of them, transitively.
/// A child counts only if its `extends`/`implements` name resolves to the parent's FQN.
fn with_subtypes(conn: &Connection, roots: &[String]) -> Result<Vec<String>> {
    let mut all: Vec<String> = roots.to_vec();
    let mut seen: HashSet<String> = roots.iter().map(|r| r.to_ascii_lowercase()).collect();
    let mut queue: Vec<String> = roots.to_vec();
    let sql = format!(
        "SELECT s.qualified_name, s.file_id, s.line FROM inheritance i
         JOIN symbols s ON s.id = i.child_id
         WHERE (i.parent_name = ?1 COLLATE NOCASE OR i.parent_name LIKE '%\\' || ?1)
           AND s.qualified_name IS NOT NULL AND s.kind IN {CLASS_KINDS}"
    );
    let mut stmt = conn.prepare(&sql)?;
    while let Some(parent) = queue.pop() {
        let short = short_name(&parent).to_string();
        let children = stmt
            .query_map(params![short], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (child, file_id, line) in children {
            let resolved = (line..=line + 5).any(|l| {
                fqn_at(conn, file_id, l, &short).is_some_and(|f| f.eq_ignore_ascii_case(&parent))
            });
            if resolved && seen.insert(child.to_ascii_lowercase()) {
                all.push(child.clone());
                queue.push(child);
            }
        }
    }
    Ok(all)
}

/// Line of `function <method>(` at or after `from_line` (1-based) within the next 400 lines.
fn find_method_decl(content: &str, from_line: usize, method: &str) -> Option<usize> {
    let re = Regex::new(&format!(r"\bfunction\s+&?{}\s*\(", regex::escape(method))).ok()?;
    content
        .lines()
        .enumerate()
        .skip(from_line.saturating_sub(1))
        .take(400)
        .find(|(_, l)| re.is_match(l))
        .map(|(i, _)| i + 1)
}

static TRAILING_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\\?[A-Za-z_][\w\\]*)\s*$").unwrap());
static TRAILING_THIS_PROP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$this\s*\??->\s*(\w+)\s*$").unwrap());
static TRAILING_VAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$(\w+)\s*$").unwrap());
static NEW_IN_PARENS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(\s*new\s+(\\?[A-Za-z_][\w\\]*)").unwrap());

/// Best-effort receiver type of a call: `$this`, `$this->prop`, `$var`, `(new T)`, `T::`.
fn receiver_type(
    conn: &Connection,
    file_id: i64,
    lines: &[&str],
    idx: usize,
    prefix: &str,
    operator: &str,
) -> Option<String> {
    let line_no = idx as i64 + 1;
    let prefix = prefix.trim_end();
    if operator.starts_with("::") {
        let token = TRAILING_NAME.captures(prefix)?.get(1)?.as_str();
        return match token.to_ascii_lowercase().as_str() {
            "self" | "static" => enclosing_class(conn, file_id, line_no),
            "parent" => None,
            _ => resolve_name(conn, file_id, line_no, token),
        };
    }
    if prefix.ends_with("$this") {
        return enclosing_class(conn, file_id, line_no);
    }
    if let Some(c) = TRAILING_THIS_PROP.captures(prefix) {
        let prop = c.get(1)?.as_str();
        return property_type(conn, file_id, lines, prop);
    }
    if let Some(c) = TRAILING_VAR.captures(prefix) {
        let var = c.get(1)?.as_str();
        return variable_type(conn, file_id, lines, idx, var);
    }
    if prefix.ends_with(')') {
        if let Some(c) = NEW_IN_PARENS.captures_iter(prefix).last() {
            return resolve_name(conn, file_id, line_no, c.get(1)?.as_str());
        }
    }
    None
}

fn resolve_name(conn: &Connection, file_id: i64, line: i64, token: &str) -> Option<String> {
    let token = token.trim_start_matches('?');
    if let Some(stripped) = token.strip_prefix('\\') {
        return Some(stripped.to_string());
    }
    fqn_at(conn, file_id, line, short_name(token))
}

fn enclosing_class(conn: &Connection, file_id: i64, line: i64) -> Option<String> {
    let sql = format!(
        "SELECT qualified_name FROM symbols WHERE file_id = ?1 AND line <= ?2
           AND qualified_name IS NOT NULL AND kind IN {CLASS_KINDS} ORDER BY line DESC LIMIT 1"
    );
    conn.query_row(&sql, params![file_id, line], |row| row.get(0))
        .ok()
}

/// Declared type of `$prop`: typed property / promoted constructor parameter, or `@var`.
fn property_type(conn: &Connection, file_id: i64, lines: &[&str], prop: &str) -> Option<String> {
    let decl = Regex::new(&format!(
        r"(?:private|protected|public|readonly|var)\b[^$;=]*?(\??\\?[A-Za-z_][\w\\|]*)\s+\${}\b",
        regex::escape(prop)
    ))
    .ok()?;
    let doc = Regex::new(&format!(
        r"@var\s+(\??\\?[A-Za-z_][\w\\|]*)(?:\s+\${})?",
        regex::escape(prop)
    ))
    .ok()?;
    for (i, line) in lines.iter().enumerate() {
        if let Some(c) = decl.captures(line) {
            if let Some(t) = first_type(conn, file_id, i as i64 + 1, c.get(1)?.as_str()) {
                return Some(t);
            }
            // Untyped declaration: fall back to a `@var` right above it.
            if i > 0 {
                if let Some(c) = doc.captures(lines[i - 1]) {
                    return first_type(conn, file_id, i as i64, c.get(1)?.as_str());
                }
            }
        }
    }
    None
}

/// Type of `$var` from the nearest earlier typed parameter, `@var`, `new`, or `get(T::class)`.
fn variable_type(
    conn: &Connection,
    file_id: i64,
    lines: &[&str],
    idx: usize,
    var: &str,
) -> Option<String> {
    let v = regex::escape(var);
    let patterns = [
        format!(r"\$(?:{v})\s*=\s*\(?\s*new\s+(\\?[A-Za-z_][\w\\]*)"),
        format!(r"\$(?:{v})\s*=\s*.*?\bget\(\s*(\\?[A-Za-z_][\w\\]*)::class"),
        format!(r"@var\s+(\??\\?[A-Za-z_][\w\\|]*)\s+\$(?:{v})\b"),
        format!(r"[(,]\s*(\??\\?[A-Za-z_][\w\\|]*)\s+&?(?:\.\.\.)?\$(?:{v})\b"),
    ];
    let regexes: Vec<Regex> = patterns.iter().filter_map(|p| Regex::new(p).ok()).collect();
    for i in (0..=idx).rev().take(300) {
        for re in &regexes {
            if let Some(c) = re.captures(lines[i]) {
                if let Some(t) = first_type(conn, file_id, i as i64 + 1, c.get(1)?.as_str()) {
                    return Some(t);
                }
            }
        }
    }
    None
}

/// First class type of a declaration like `?Foo`, `Foo|null`, `\App\Foo`.
fn first_type(conn: &Connection, file_id: i64, line: i64, raw: &str) -> Option<String> {
    raw.split('|')
        .map(|t| t.trim().trim_start_matches('?'))
        .filter(|t| {
            !matches!(
                t.to_ascii_lowercase().as_str(),
                "null"
                    | "array"
                    | "int"
                    | "string"
                    | "bool"
                    | "float"
                    | "mixed"
                    | "callable"
                    | "iterable"
                    | "object"
                    | "false"
                    | "true"
                    | "void"
                    | "self"
                    | "static"
            )
        })
        .find_map(|t| resolve_name(conn, file_id, line, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psr4_path_reroots_the_namespace_tail() {
        assert_eq!(
            psr4_path(
                "local/modules/orteka.catalog/lib/Infrastructure/Facet/Elastic/Foo.php",
                "Orteka\\Catalog\\Infrastructure\\Facet\\Elastic",
                "Orteka\\Catalog\\Infrastructure\\Facet\\Elastic\\Filter"
            )
            .as_deref(),
            Some("local/modules/orteka.catalog/lib/Infrastructure/Facet/Elastic/Filter/Foo.php")
        );
        assert_eq!(psr4_path("src/A/Foo.php", "App\\A", "Other\\B"), None);
    }

    #[test]
    fn classify_reference_lines() {
        assert_eq!(classify("use App\\Foo;", "Foo"), "import");
        assert_eq!(classify("* @var Foo[]", "Foo"), "phpdoc");
        assert_eq!(classify("Foo::class => [", "Foo"), "::class");
        assert_eq!(classify("$x = new \\App\\Foo();", "Foo"), "new");
        assert_eq!(classify("return Foo::make();", "Foo"), "static");
        assert_eq!(classify("private Foo $foo,", "Foo"), "type");
        assert_eq!(classify("#[CoversClass(Foo::class)]", "Foo"), "attribute");
    }

    #[test]
    fn class_body_and_similarity() {
        let a = "<?php\nfinal class A extends B\n{\n    // note\n    public int $x = 1;\n}\nclass Other {}\n";
        let body = class_body(a, 2);
        assert_eq!(
            body,
            vec!["final class A extends B", "{", "public int $x = 1;", "}"]
        );
        let t = token_counts(&body);
        assert!((dice(&t, &t) - 1.0).abs() < f64::EPSILON);
    }
}
