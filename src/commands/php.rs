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
const CLASS_KINDS: &str = "('class', 'interface', 'trait', 'enum', 'object')";
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
    kind: String,
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
    /// In an `external:` directory (framework core, vendor).
    #[serde(skip)]
    external: bool,
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
    let has_kinds: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM refs WHERE ref_kind IS NOT NULL)",
        [],
        |row| row.get(0),
    )?;
    if !has_kinds {
        bail!("The index has no resolved PHP names yet. Run 'ast-index rebuild'.");
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

/// Classes with the short name of an FQN that is not in the index: an agent that guessed the namespace
/// gets the right FQN instead of an empty answer.
fn namesakes(conn: &Connection, fqn: &str) -> Vec<String> {
    let sql = format!(
        "SELECT DISTINCT qualified_name FROM symbols
         WHERE name = ?1 COLLATE NOCASE AND qualified_name IS NOT NULL AND kind IN {CLASS_KINDS}
         ORDER BY qualified_name"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    stmt.query_map(params![short_name(fqn)], |row| row.get::<_, String>(0))
        .map(|rows| rows.filter_map(|r| r.ok()).filter(|q| !q.eq_ignore_ascii_case(fqn)).collect())
        .unwrap_or_default()
}

/// "X is not defined in the index", with the classes of the same short name when there are any.
fn not_defined(conn: &Connection, what: &str, fqn: &str) -> String {
    let same = namesakes(conn, fqn);
    if same.is_empty() {
        format!("{what}{fqn} is not defined in the index")
    } else {
        format!(
            "{what}{fqn} is not defined in the index; classes named {}: {}",
            short_name(fqn),
            same.join(", ")
        )
    }
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

fn refs_by_fqn(conn: &Connection, root: &Path, fqn: &str) -> Result<Vec<RefRow>> {
    // external code keeps only its `extends`/`implements` lines: shown with --external
    let mut stmt = conn.prepare(&format!(
        "SELECT f.path, f.root_path, r.line, r.context, r.file_id, r.ref_kind
         FROM refs r JOIN files f ON f.id = r.file_id
         WHERE r.fqn = ?1 COLLATE NOCASE{}
         ORDER BY f.path, r.line",
        db::external_filter("f")
    ))?;
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    let mapped = stmt.query_map(params![fqn], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        ))
    })?;
    for item in mapped {
        let (path, root_path, line, context, file_id, kind) = item?;
        if !seen.insert((file_id, line)) {
            continue;
        }
        let (shown, _) = locate(root, root_path.as_deref().unwrap_or(""), &path);
        let context = context.unwrap_or_default();
        rows.push(RefRow {
            test: is_test_path(&shown),
            kind,
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
        "SELECT f.path, f.root_path, s.line, s.kind, s.file_id, f.external
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
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(path, root_path, line, kind, file_id, external)| {
            let (shown, abs) = locate(root, root_path.as_deref().unwrap_or(""), &path);
            DefRow {
                path: shown,
                line,
                kind,
                file_id,
                external: external != 0,
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
    let unknown = refs.is_empty() && definitions(&conn, root, &fqn)?.is_empty();
    let did_you_mean = if unknown { namesakes(&conn, &fqn) } else { Vec::new() };

    if format == "json" {
        return print_json(&serde_json::json!({
            "fqn": fqn,
            "items": shown,
            "did_you_mean": did_you_mean,
            "pagination": {"total": total, "returned": shown.len(), "truncated": total > shown.len(), "limit": limit},
        }));
    }
    println!(
        "{}",
        format!("Usages of {fqn} (showing {} of {total}):", shown.len()).bold()
    );
    if unknown {
        println!("  {}", not_defined(&conn, "", &fqn));
    }
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

/// A short name shared by several PHP classes: `usages Name` mixes them, the FQN form does not.
pub fn print_namesakes_notice(conn: &Connection, name: &str) {
    let sql = format!(
        "SELECT DISTINCT qualified_name FROM symbols
         WHERE name = ?1 COLLATE NOCASE AND qualified_name IS NOT NULL AND kind IN {CLASS_KINDS}
         ORDER BY qualified_name"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return;
    };
    let fqns: Vec<String> = stmt
        .query_map(params![name], |row| row.get::<_, String>(0))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    if fqns.len() > 1 {
        println!(
            "{} {} classes are named {name}; these usages mix them. For one class: usages '<FQN>' ({})",
            "note:".yellow(),
            fqns.len(),
            fqns.join(", ")
        );
    }
}

// ---------------------------------------------------------------------------
// impact <FQN>
// ---------------------------------------------------------------------------

/// Above this many references the text output lists files with counts instead of every line.
const IMPACT_FULL_LIMIT: usize = 60;

pub fn cmd_impact(root: &Path, fqn: &str, full: bool, format: &str) -> Result<()> {
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
        *by_kind.entry(&r.kind).or_default() += 1;
    }
    let files: HashSet<&str> = refs.iter().map(|r| r.path.as_str()).collect();
    let tests = refs.iter().filter(|r| r.test).count();

    let did_you_mean = if defs.is_empty() && refs.is_empty() {
        namesakes(&conn, &fqn)
    } else {
        Vec::new()
    };

    if format == "json" {
        return print_json(&serde_json::json!({
            "fqn": fqn,
            "definitions": defs,
            "did_you_mean": did_you_mean,
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
    if !did_you_mean.is_empty() {
        println!(
            "  {} classes named {}: {}",
            "did you mean:".yellow(),
            short_name(&fqn),
            did_you_mean.join(", ")
        );
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
    if full || refs.len() <= IMPACT_FULL_LIMIT {
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
    } else {
        println!(
            "  per file (more than {IMPACT_FULL_LIMIT} references; --full or --format json lists every line):"
        );
        print_file_summary(&refs);
    }
    println!("config mentions: {}", config.len());
    for c in &config {
        println!("  {}:{}  {}", c.path.cyan(), c.line, c.text);
    }
    println!("coverage: {COVERAGE}");
    Ok(())
}

/// One line per file: counts by kind and the first line numbers.
fn print_file_summary(refs: &[RefRow]) {
    let mut files: Vec<(&str, BTreeMap<&str, usize>, Vec<i64>)> = Vec::new();
    for r in refs {
        if files.last().is_none_or(|(p, _, _)| *p != r.path) {
            files.push((&r.path, BTreeMap::new(), Vec::new()));
        }
        let (_, kinds, lines) = files.last_mut().expect("pushed above");
        *kinds.entry(&r.kind).or_default() += 1;
        lines.push(r.line);
    }
    for (path, kinds, lines) in files {
        let kinds: Vec<String> = kinds.iter().map(|(k, n)| format!("{k}×{n}")).collect();
        let mut shown: Vec<String> = lines.iter().take(8).map(|l| l.to_string()).collect();
        if lines.len() > 8 {
            shown.push("…".into());
        }
        println!(
            "  {}  {}  (lines {})",
            path.cyan(),
            kinds.join(" "),
            shown.join(", ")
        );
    }
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
        bail!("{}", not_defined(&conn, "", &fqn));
    };
    let mut warnings = Vec::new();
    if defs.len() > 1 {
        let places: Vec<String> = defs
            .iter()
            .map(|d| format!("{}:{}", d.path, d.line))
            .collect();
        warnings.push(format!(
            "{fqn} is defined {} times ({}); the plan moves the first one, references cannot be \
             told apart",
            defs.len(),
            places.join(", ")
        ));
    }
    if let Some(taken) = definitions(&conn, root, &new_fqn)?.first() {
        warnings.push(format!(
            "{new_fqn} already exists at {}:{} — the move would collide",
            taken.path, taken.line
        ));
    }
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
        let fqn_lower = fqn.to_ascii_lowercase();
        let escaped_lower = fqn_lower.replace('\\', "\\\\");
        for r in rows.iter().filter(|r| r.kind != "import") {
            let context = r.context.to_ascii_lowercase();
            let has_fqn = context.contains(&fqn_lower) || context.contains(&escaped_lower);
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
            "warnings": warnings,
            "coverage": COVERAGE,
        }));
    }
    println!("{}", format!("Move {fqn} → {new_fqn}").bold());
    for w in &warnings {
        println!("  {} {w}", "warning:".yellow());
    }
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
    // Case-insensitive: Bitrix module autoload maps `Orteka\Main\Metro` to `lib/metro/`.
    let mut matched = 0;
    let mut lowercase_dirs = false;
    while matched < dir_parts.len() && matched < ns_parts.len() {
        let dir_part = dir_parts[dir_parts.len() - 1 - matched];
        let ns_part = ns_parts[ns_parts.len() - 1 - matched];
        if !dir_part.eq_ignore_ascii_case(ns_part) {
            break;
        }
        lowercase_dirs |= dir_part != ns_part && dir_part == ns_part.to_ascii_lowercase();
        matched += 1;
    }
    let base = dir_parts[..dir_parts.len() - matched].join("/");
    let prefix = ns_parts[..ns_parts.len() - matched].join("\\");
    let rest = if prefix.is_empty() {
        new_ns
    } else if new_ns.len() >= prefix.len() && new_ns[..prefix.len()].eq_ignore_ascii_case(&prefix) {
        let rest = &new_ns[prefix.len()..];
        if !rest.is_empty() && !rest.starts_with('\\') {
            return None;
        }
        rest.trim_start_matches('\\')
    } else {
        return None;
    };
    let rel = rest.replace('\\', "/");
    let rel = if lowercase_dirs {
        rel.to_ascii_lowercase()
    } else {
        rel
    };
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
        "SELECT fqn, line, ref_kind FROM refs
         WHERE file_id = ?1 AND fqn IS NOT NULL ORDER BY line",
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
    // `use Trait;` inside a class is not an import: the trait resolves by namespace too.
    let imported: HashSet<String> = rows
        .iter()
        .filter(|(_, _, kind)| kind == "import")
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
    /// One of the paths is a deliberate mirror (`duplicate_mirrors` in the config).
    mirror: bool,
    /// Both copies declare the same FQN (one of them is usually a leftover): references
    /// cannot be told apart, so the counts are shared.
    same_fqn: bool,
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
         WHERE s.qualified_name IS NOT NULL AND s.kind IN {CLASS_KINDS} AND f.external = 0
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
    // The PHP parser records a class once per implemented interface; keep one entry per class.
    let mut seen = HashSet::new();
    for (name, fqn, line, path, root_path) in rows {
        if !seen.insert((path.clone(), line)) {
            continue;
        }
        let (shown, abs) = locate(root, root_path.as_deref().unwrap_or(""), &path);
        groups
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push((fqn, shown, line, abs));
    }

    let mirrors = indexer::load_config_quiet(root)
        .and_then(|c| c.duplicate_mirrors)
        .unwrap_or_default();
    let mut usage_cache: HashMap<(String, String), (usize, usize)> = HashMap::new();
    let mut usage = |fqn: &str, own_path: &str| -> Result<(usize, usize)> {
        let key = (fqn.to_ascii_lowercase(), own_path.to_string());
        if let Some(v) = usage_cache.get(&key) {
            return Ok(*v);
        }
        let refs = refs_by_fqn(&conn, root, fqn)?;
        let outside: Vec<&RefRow> = refs.iter().filter(|r| r.path != own_path).collect();
        let tests = outside.iter().filter(|r| r.test).count();
        let v = (outside.len() - tests, tests);
        usage_cache.insert(key, v);
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
                let same_fqn = fa.eq_ignore_ascii_case(fb);
                let (ra, rta) = usage(fa, pa)?;
                let (rb, rtb) = usage(fb, pb)?;
                pairs.push(DuplicatePair {
                    name: short_name(fa).to_string(),
                    similarity: (similarity * 100.0).round() / 100.0,
                    lines: size,
                    mirror: mirrors.iter().any(|m| pa.contains(m) || pb.contains(m)),
                    same_fqn,
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
            x.mirror,
            -(x.similarity * 100.0) as i64,
            -(x.lines as i64),
            &x.name,
        )
            .cmp(&(
                y.mirror,
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
        let tag = match (p.mirror, p.same_fqn) {
            (_, true) => " [same FQN — reference counts are shared]",
            (true, false) => " [mirror]",
            _ => "",
        };
        println!(
            "{:>4.0}%  {}  {} lines{tag}",
            p.similarity * 100.0,
            p.name.bold(),
            p.lines
        );
        for c in [&p.a, &p.b] {
            let unused = if p.same_fqn {
                String::new()
            } else if c.references == 0 {
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
    let end = class_end_line(content, decl_line).unwrap_or(usize::MAX);
    content
        .lines()
        .enumerate()
        .skip(decl_line.saturating_sub(1))
        .take_while(|(i, _)| *i < end)
        .map(|(_, line)| line.trim())
        .filter(|t| !t.is_empty() && !is_comment_line(t))
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .take(5000)
        .collect()
}

/// Line (1-based) where the braces of the class declared at `decl_line` close.
pub(crate) fn class_end_line(content: &str, decl_line: usize) -> Option<usize> {
    let mut depth = 0i64;
    let mut opened = false;
    for (i, line) in content
        .lines()
        .enumerate()
        .skip(decl_line.saturating_sub(1))
    {
        let t = line.trim();
        if is_comment_line(t) {
            continue;
        }
        for c in code_chars(t) {
            match c {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if opened && depth <= 0 {
            return Some(i + 1);
        }
    }
    None
}

fn is_comment_line(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        || (trimmed.starts_with('#') && !trimmed.starts_with("#["))
        || trimmed.starts_with('*')
        || trimmed.starts_with("/*")
}

/// Characters of a line outside string literals and a trailing `//` comment, so braces in
/// `'{'` or `"{$x}"` do not end a class early.
fn code_chars(line: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '/' if chars.peek() == Some(&'/') => break,
                '#' if chars.peek() != Some(&'[') => break,
                _ => out.push(c),
            },
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
// implementations <FQN>
// ---------------------------------------------------------------------------

/// Classes that extend or implement the type, transitively, checked by FQN.
pub fn cmd_implementations_fqn(root: &Path, fqn: &str, limit: usize, format: &str) -> Result<()> {
    let _lease = db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let fqn = normalize_fqn(fqn);
    let roots = type_fqns(&conn, &fqn)?;
    if roots.is_empty() {
        bail!("{}", not_defined(&conn, "type ", &fqn));
    }
    let root_lower: HashSet<String> = roots.iter().map(|r| r.to_ascii_lowercase()).collect();
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    // subtypes are followed through external classes; external ones themselves are shown with --external
    let mut external_hidden = 0;
    for sub in with_subtypes(&conn, &roots)? {
        if root_lower.contains(&sub.to_ascii_lowercase()) {
            continue;
        }
        for d in definitions(&conn, root, &sub)? {
            if d.external && db::external_hidden() {
                external_hidden += 1;
                continue;
            }
            if seen.insert((d.path.clone(), d.line)) {
                items.push(
                    serde_json::json!({"fqn": sub, "kind": d.kind, "path": d.path, "line": d.line}),
                );
            }
        }
    }
    let total = items.len();
    items.truncate(limit);
    if format == "json" {
        return print_json(&serde_json::json!({
            "items": items,
            "external_hidden": external_hidden,
            "pagination": {"total": total, "returned": items.len(), "truncated": total > items.len(), "limit": limit},
        }));
    }
    println!(
        "{}",
        format!(
            "Implementations of {} (showing {} of {total}):",
            roots.join(", "),
            items.len()
        )
        .bold()
    );
    if external_hidden > 0 {
        println!("  {} {external_hidden} more in external code (--external)", "note:".yellow());
    }
    for i in &items {
        println!(
            "  {} [{}]: {}:{}",
            i["fqn"].as_str().unwrap_or(""),
            i["kind"].as_str().unwrap_or(""),
            i["path"].as_str().unwrap_or("").cyan(),
            i["line"]
        );
    }
    Ok(())
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
        bail!("{}", not_defined(&conn, "type ", &type_part));
    }
    let types = with_subtypes(&conn, &roots)?;
    let types_lower: HashSet<String> = types.iter().map(|t| t.to_ascii_lowercase()).collect();

    // Declarations: the method in each type, plus anonymous classes implementing one of them.
    let mut declarations = Vec::new();
    for fqn in &types {
        for d in definitions(&conn, root, fqn)? {
            if let Some(line) = method_in_class(&conn, d.file_id, d.line, &method) {
                declarations.push(Declaration {
                    path: d.path.clone(),
                    line,
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

    // Calls typed as a supertype that declares the method (or lies outside the index) may
    // dispatch to these types at run time: listed separately, not excluded.
    let mut via_supertypes = Vec::new();
    for sup in supertypes(&conn, &roots)? {
        let defs = definitions(&conn, root, &sup)?;
        let declares = defs.is_empty()
            || defs
                .iter()
                .any(|d| method_in_class(&conn, d.file_id, d.line, &method).is_some());
        if declares {
            via_supertypes.push(sup);
        }
    }
    let supers_lower: HashSet<String> = via_supertypes
        .iter()
        .map(|t| t.to_ascii_lowercase())
        .collect();

    let mut stmt = conn.prepare(
        "SELECT DISTINCT r.file_id, f.path, f.root_path FROM refs r JOIN files f ON f.id = r.file_id
         WHERE r.name = ?1 COLLATE NOCASE AND (f.path LIKE '%.php' OR f.path LIKE '%.phtml')",
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

    // Method names are case-insensitive in PHP: legacy code calls `GetList` as `getList`.
    let call_re = Regex::new(&format!(
        r"(->|\?->|::)\s*(?i:{})\s*\(",
        regex::escape(&method)
    ))?;
    let decl_re = Regex::new(&format!(
        r"\bfunction\s+&?(?i:{})\s*\(",
        regex::escape(&method)
    ))?;
    let mut calls = Vec::new();
    let mut via_supertype = Vec::new();
    let mut unresolved = Vec::new();
    let mut excluded = Vec::new();
    for (file_id, path, root_path) in files {
        let (shown, abs) = locate(root, root_path.as_deref().unwrap_or(""), &path);
        let Some(content) = read_lossy(&abs) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            let comment = t.starts_with("//")
                || t.starts_with('*')
                || t.starts_with("/*")
                || (t.starts_with('#') && !t.starts_with("#["));
            if comment || decl_re.is_match(line) {
                continue;
            }
            let line_no = idx as i64 + 1;
            for m in call_re.find_iter(line) {
                let receiver = receiver_type(&conn, file_id, &lines, idx, &line[..m.start()]);
                let site = CallSite {
                    path: shown.clone(),
                    line: line_no,
                    receiver: receiver.clone(),
                    context: truncate(line, 110),
                };
                match receiver {
                    Some(t) if types_lower.contains(&t.to_ascii_lowercase()) => calls.push(site),
                    Some(t) if supers_lower.contains(&t.to_ascii_lowercase()) => {
                        via_supertype.push(site)
                    }
                    Some(_) => excluded.push(site),
                    None => unresolved.push(site),
                }
            }
        }
    }
    let sort = |v: &mut Vec<CallSite>| v.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    sort(&mut calls);
    sort(&mut via_supertype);
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
            "supertypes": via_supertypes,
            "via_supertype": via_supertype,
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
    print_sites(
        &format!(
            "calls on these types: {total_calls} (showing {})",
            calls.len()
        ),
        &calls,
        true,
    );
    if !via_supertype.is_empty() {
        let names: Vec<&str> = via_supertypes.iter().map(|t| short_name(t)).collect();
        print_sites(
            &format!(
                "calls typed as a supertype ({}) — may dispatch here: {}",
                names.join(", "),
                via_supertype.len()
            ),
            &via_supertype,
            true,
        );
    }
    // a common method name (`Update`, `add`) gathers many calls on unrelated objects: a summary by receiver
    // lets them be dismissed at a glance, the full list stays in --format json
    let mut by_receiver: BTreeMap<String, usize> = BTreeMap::new();
    for c in &unresolved {
        *by_receiver.entry(receiver_text(&c.context, &method)).or_default() += 1;
    }
    let mut by_receiver: Vec<(String, usize)> = by_receiver.into_iter().collect();
    by_receiver.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let summary: Vec<String> = by_receiver.iter().take(8).map(|(r, n)| format!("{r} ×{n}")).collect();
    print_sites(
        &format!(
            "receiver not inferred — check by hand: {}{}",
            unresolved.len(),
            if summary.is_empty() { String::new() } else { format!("; by receiver: {}", summary.join(", ")) }
        ),
        &unresolved[..unresolved.len().min(SITES_SHOWN)],
        false,
    );
    print_more(unresolved.len());
    println!("excluded (other types): {}", excluded.len());
    for c in excluded.iter().take(SITES_SHOWN) {
        println!(
            "  {}:{}  [{}]",
            c.path.cyan(),
            c.line,
            c.receiver.as_deref().unwrap_or("")
        );
    }
    print_more(excluded.len());
    Ok(())
}

/// Lines shown per section of calls that need a manual look; `--format json` lists all.
const SITES_SHOWN: usize = 10;

fn print_more(total: usize) {
    if total > SITES_SHOWN {
        println!("  … {} more (--format json lists all)", total - SITES_SHOWN);
    }
}

/// The expression a method is called on in a line of code: `$DB` in `$DB->Update(…)`, `$this->client` in
/// `$this->client->update(…)`, `self::getDataClass()` in `self::getDataClass()::update(…)`.
fn receiver_text(context: &str, method: &str) -> String {
    let lower = context.to_ascii_lowercase();
    let needle = method.to_ascii_lowercase();
    let mut at = None;
    for (i, _) in lower.match_indices(&needle) {
        let before = &context[..i];
        if before.ends_with("->") || before.ends_with("::") || before.ends_with("?->") {
            at = Some(i);
            break;
        }
    }
    let Some(i) = at else {
        return "?".to_string();
    };
    let before = context[..i].trim_end_matches("->").trim_end_matches('?').trim_end_matches("::");
    let bytes = before.as_bytes();
    let mut start = bytes.len();
    let mut depth = 0i32;
    while start > 0 {
        let c = bytes[start - 1];
        match c {
            b')' | b']' => depth += 1,
            b'(' | b'[' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            b'$' | b'_' | b'\\' | b'>' | b'-' | b':' => {}
            _ if c.is_ascii_alphanumeric() => {}
            _ => break,
        }
        start -= 1;
    }
    let text = before[start..].trim_start_matches(['-', '>', ':']);
    if text.is_empty() {
        "?".to_string()
    } else {
        truncate(text, 40)
    }
}

fn print_sites(title: &str, sites: &[CallSite], with_receiver: bool) {
    println!("{title}");
    for c in sites {
        let receiver = if with_receiver {
            format!("  [{}]", short_name(c.receiver.as_deref().unwrap_or("")))
        } else {
            String::new()
        };
        println!("  {}:{}{receiver}  {}", c.path.cyan(), c.line, c.context);
    }
}

fn type_fqns(conn: &Connection, type_part: &str) -> Result<Vec<String>> {
    if type_part.contains('\\') {
        let sql = format!(
            "SELECT DISTINCT qualified_name FROM symbols WHERE qualified_name = ?1 COLLATE NOCASE AND kind IN {CLASS_KINDS}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let defined = stmt
            .query_map(params![type_part], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !defined.is_empty() {
            return Ok(defined);
        }
        // A type outside the index (vendor, framework core, generated ORM classes) still has
        // subtypes and callers in it when code references it by this FQN.
        let referenced: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM refs WHERE fqn = ?1 COLLATE NOCASE)",
            params![type_part],
            |row| row.get(0),
        )?;
        return Ok(if referenced {
            vec![type_part.to_string()]
        } else {
            vec![]
        });
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

/// Names a type goes by in the files that reference it: its short name and import aliases.
fn local_names(conn: &Connection, fqn: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT name FROM refs WHERE fqn = ?1 COLLATE NOCASE")?;
    let mut names: Vec<String> = stmt
        .query_map(params![fqn], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let short = short_name(fqn).to_string();
    if !names.iter().any(|n| n.eq_ignore_ascii_case(&short)) {
        names.push(short);
    }
    Ok(names)
}

/// FQN of an `extends`/`implements` name written on the declaration at `decl_line`. The name may
/// sit a few lines below the `class` line (multi-line lists, attributes).
fn resolve_parent(
    conn: &Connection,
    file_id: i64,
    decl_line: i64,
    parent_name: &str,
) -> Option<String> {
    if let Some(stripped) = parent_name.strip_prefix('\\') {
        return Some(stripped.to_string());
    }
    (decl_line..=decl_line + 10).find_map(|l| fqn_at(conn, file_id, l, short_name(parent_name)))
}

/// The types plus every class/interface that extends or implements one of them, transitively.
/// A child counts only if its `extends`/`implements` name resolves to the parent's FQN, under
/// any alias the parent is imported with.
fn with_subtypes(conn: &Connection, roots: &[String]) -> Result<Vec<String>> {
    let mut all: Vec<String> = roots.to_vec();
    let mut seen: HashSet<String> = roots.iter().map(|r| r.to_ascii_lowercase()).collect();
    let mut queue: Vec<String> = roots.to_vec();
    let sql = format!(
        "SELECT s.qualified_name, s.file_id, s.line, i.parent_name FROM inheritance i
         JOIN symbols s ON s.id = i.child_id
         WHERE (i.parent_name = ?1 COLLATE NOCASE OR i.parent_name LIKE '%\\' || ?1)
           AND s.qualified_name IS NOT NULL AND s.kind IN {CLASS_KINDS}"
    );
    let mut stmt = conn.prepare(&sql)?;
    while let Some(parent) = queue.pop() {
        for name in local_names(conn, &parent)? {
            let children = stmt
                .query_map(params![name], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (child, file_id, line, parent_name) in children {
                let resolves = resolve_parent(conn, file_id, line, &parent_name)
                    .is_some_and(|p| p.eq_ignore_ascii_case(&parent));
                if resolves && seen.insert(child.to_ascii_lowercase()) {
                    all.push(child.clone());
                    queue.push(child);
                }
            }
        }
    }
    Ok(all)
}

/// Every class/interface the types extend or implement, transitively (not the types
/// themselves). Parents outside the index are kept by the name they resolve to.
fn supertypes(conn: &Connection, types: &[String]) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT s.file_id, s.line, i.parent_name FROM symbols s
         JOIN inheritance i ON i.child_id = s.id
         WHERE s.qualified_name = ?1 COLLATE NOCASE AND s.kind IN {CLASS_KINDS}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut seen: HashSet<String> = types.iter().map(|t| t.to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    let mut queue: Vec<String> = types.to_vec();
    while let Some(child) = queue.pop() {
        let parents = stmt
            .query_map(params![child], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (file_id, line, parent_name) in parents {
            if let Some(parent) = resolve_parent(conn, file_id, line, &parent_name) {
                if seen.insert(parent.to_ascii_lowercase()) {
                    out.push(parent.clone());
                    queue.push(parent);
                }
            }
        }
    }
    Ok(out)
}

/// Line of the method in the class declared at `class_line`: the first `function` symbol of
/// that name after the class and before the next class-like declaration in the file.
fn method_in_class(conn: &Connection, file_id: i64, class_line: i64, method: &str) -> Option<i64> {
    let next_class: i64 = conn
        .query_row(
            &format!(
                "SELECT MIN(line) FROM symbols WHERE file_id = ?1 AND line > ?2 AND kind IN {CLASS_KINDS}"
            ),
            params![file_id, class_line],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        .unwrap_or(i64::MAX);
    conn.query_row(
        "SELECT MIN(line) FROM symbols WHERE file_id = ?1 AND name = ?2 COLLATE NOCASE
           AND kind = 'function' AND line >= ?3 AND line < ?4",
        params![file_id, method, class_line, next_class],
        |row| row.get::<_, Option<i64>>(0),
    )
    .ok()
    .flatten()
}

/// Line of `function <method>(` at or after `from_line` (1-based) within the next 400 lines —
/// for anonymous classes, which have no symbol of their own.
fn find_method_decl(content: &str, from_line: usize, method: &str) -> Option<usize> {
    let re = Regex::new(&format!(
        r"\bfunction\s+&?(?i:{})\s*\(",
        regex::escape(method)
    ))
    .ok()?;
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
/// `prefix` is the line up to the `->`/`::` of the call.
fn receiver_type(
    conn: &Connection,
    file_id: i64,
    lines: &[&str],
    idx: usize,
    prefix: &str,
) -> Option<String> {
    let line_no = idx as i64 + 1;
    let rest = &lines[idx][prefix.len()..];
    let prefix = prefix.trim_end();
    if rest.starts_with("::") {
        let token = TRAILING_NAME.captures(prefix)?.get(1)?.as_str();
        return match token.to_ascii_lowercase().as_str() {
            "self" | "static" => enclosing_class(conn, file_id, line_no),
            "parent" => parent_class(conn, file_id, line_no),
            _ => resolve_name(conn, file_id, line_no, token),
        };
    }
    if prefix.ends_with("$this") {
        return enclosing_class(conn, file_id, line_no);
    }
    if let Some(c) = TRAILING_THIS_PROP.captures(prefix) {
        return property_type(conn, file_id, lines, c.get(1)?.as_str());
    }
    if let Some(c) = TRAILING_VAR.captures(prefix) {
        return variable_type(conn, file_id, lines, idx, prefix, c.get(1)?.as_str());
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

/// The class-like declaration a line belongs to: symbol id, line and FQN.
fn class_at(conn: &Connection, file_id: i64, line: i64) -> Option<(i64, i64, Option<String>)> {
    let sql = format!(
        "SELECT id, line, qualified_name FROM symbols WHERE file_id = ?1 AND line <= ?2
           AND kind IN {CLASS_KINDS} ORDER BY line DESC LIMIT 1"
    );
    conn.query_row(&sql, params![file_id, line], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .ok()
}

fn enclosing_class(conn: &Connection, file_id: i64, line: i64) -> Option<String> {
    class_at(conn, file_id, line)?.2
}

/// The class the enclosing class extends.
fn parent_class(conn: &Connection, file_id: i64, line: i64) -> Option<String> {
    let (id, class_line, _) = class_at(conn, file_id, line)?;
    let parent_name: String = conn
        .query_row(
            "SELECT parent_name FROM inheritance WHERE child_id = ?1 AND kind = 'extends' LIMIT 1",
            params![id],
            |row| row.get(0),
        )
        .ok()?;
    resolve_parent(conn, file_id, class_line, &parent_name)
}

const TYPE: &str = r"(\??\\?[A-Za-z_][\w\\|]*)";

static PROPERTY_DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:private|protected|public|readonly|var)\b[^$;=]*?{TYPE}\s+\$(\w+)\b"
    ))
    .unwrap()
});
static DOC_VAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"@var\s+{TYPE}(?:\s+\$(\w+))?")).unwrap());

/// Declared type of `$prop`: typed property / promoted constructor parameter, or `@var`.
fn property_type(conn: &Connection, file_id: i64, lines: &[&str], prop: &str) -> Option<String> {
    for (i, line) in lines.iter().enumerate() {
        let Some(c) = PROPERTY_DECL.captures_iter(line).find(|c| &c[2] == prop) else {
            continue;
        };
        if let Some(t) = first_type(conn, file_id, i as i64 + 1, &c[1]) {
            return Some(t);
        }
        // Untyped declaration: fall back to a `@var` right above it.
        let above = i.checked_sub(1).map(|j| lines[j]).unwrap_or("");
        return DOC_VAR
            .captures_iter(above)
            .find(|c| c.get(2).is_none_or(|v| v.as_str() == prop))
            .and_then(|c| first_type(conn, file_id, i as i64, &c[1]));
    }
    None
}

static NAMED_FUNCTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bfunction\s+&?\w+\s*\(").unwrap());

/// Patterns that type a variable: (regex, variable group, type group).
static VARIABLE_TYPERS: LazyLock<Vec<(Regex, usize, usize)>> =
    LazyLock::new(|| {
        [
        (r"\$(\w+)\s*=\s*\(?\s*new\s+(\\?[A-Za-z_][\w\\]*)".to_string(), 1, 2),
        (r"\$(\w+)\s*=[^;]*?\bget\(\s*(\\?[A-Za-z_][\w\\]*)::class".to_string(), 1, 2),
        (format!(r"@var\s+{TYPE}\s+\$(\w+)\b"), 2, 1),
        (format!(r"[(,]\s*{TYPE}\s+&?(?:\.\.\.)?\$(\w+)\b"), 2, 1),
        // A parameter on its own line of a multi-line signature.
        (
            format!(
                r"^\s*(?:(?:private|protected|public|readonly)\s+)*{TYPE}\s+&?(?:\.\.\.)?\$(\w+)\b"
            ),
            2,
            1,
        ),
    ]
    .into_iter()
    .map(|(re, var, ty)| (Regex::new(&re).unwrap(), var, ty))
    .collect()
    });
static ASSIGNMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$(\w+)\s*(?:=[^=>]|=$)").unwrap());
static FOREACH_BINDING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bas\s+(?:\$\w+\s*=>\s*)?&?\$(\w+)\b").unwrap());

/// Type of `$var` from the nearest earlier typed parameter, `@var`, `new`, or `get(T::class)`,
/// within the enclosing named function. Another assignment or a `foreach … as $var` on the way
/// — the call's own line included — makes the type unknown rather than a guess.
fn variable_type(
    conn: &Connection,
    file_id: i64,
    lines: &[&str],
    idx: usize,
    prefix: &str,
    var: &str,
) -> Option<String> {
    for i in (0..=idx).rev() {
        // On the call's line only the text before the call counts.
        let text = if i == idx { prefix } else { lines[i] };
        let typed = VARIABLE_TYPERS.iter().find_map(|(re, v, t)| {
            re.captures_iter(text)
                .filter(|c| &c[*v] == var)
                .find_map(|c| first_type(conn, file_id, i as i64 + 1, &c[*t]))
        });
        if typed.is_some() {
            return typed;
        }
        // An assignment counts once its statement is over: in `$node = $node->parent()` the
        // receiver is still the old value.
        let assigned = ASSIGNMENT
            .captures_iter(text)
            .any(|c| &c[1] == var && (i < idx || text[c.get(0).unwrap().end()..].contains(';')));
        let bound = FOREACH_BINDING.captures_iter(text).any(|c| &c[1] == var);
        if assigned || bound {
            // `/** @var T */` right above an assignment types it.
            let above = i.checked_sub(1).map(|j| lines[j]).unwrap_or("");
            return DOC_VAR
                .captures_iter(above)
                .find(|c| c.get(2).is_none_or(|v| v.as_str() == var))
                .and_then(|c| first_type(conn, file_id, i as i64, &c[1]));
        }
        if NAMED_FUNCTION.is_match(text) {
            return None;
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
        assert_eq!(
            psr4_path(
                "local/modules/orteka.main/lib/metro/MetroTable.php",
                "Orteka\\Main\\Metro",
                "Orteka\\Main\\Geo\\Metro"
            )
            .as_deref(),
            Some("local/modules/orteka.main/lib/geo/metro/MetroTable.php")
        );
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

        let braces = "<?php\nclass A\n{\n    private string $open = '{';\n    public function a(): string { return \"}{$x}\"; } // }\n}\nclass B {}\n";
        assert_eq!(class_body(braces, 2).last().map(String::as_str), Some("}"));
        assert_eq!(class_body(braces, 2).len(), 5);
    }
}

#[cfg(test)]
mod receiver_tests {
    use super::receiver_text;

    #[test]
    fn receiver_of_a_call_in_a_line() {
        assert_eq!(receiver_text("$r = $DB->Update('t', $f);", "Update"), "$DB");
        assert_eq!(receiver_text("$this->client->update($id);", "Update"), "$this->client");
        assert_eq!(receiver_text("return self::getDataClass()::update(", "update"), "self::getDataClass()");
        assert_eq!(receiver_text("$entity_data_class::update($id, $f);", "update"), "$entity_data_class");
        assert_eq!(receiver_text("$x?->update();", "update"), "$x");
        assert_eq!(receiver_text("update($x);", "update"), "?");
    }
}
