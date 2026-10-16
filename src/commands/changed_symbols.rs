//! `changed --symbols`: how the API of the PHP classes a branch touches changed — methods,
//! constructors, class headers. Both versions of each file come from git (merge base and
//! head), so no index and no checkout of either revision is needed.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use colored::Colorize;
use serde::Serialize;

use super::changed::{ChangeStatus, ChangedFile};
use crate::parsers::php_api::{
    self, ClassApi, ConstructorChange, HeaderChange, MethodChange,
};

#[derive(Serialize)]
pub(crate) struct SymbolsResult {
    pub schema_version: u32,
    pub base: String,
    pub merge_base: String,
    pub head: String,
    pub scope: Option<String>,
    pub classes: Vec<ClassEntry>,
    /// Files whose old or new version has a syntax error: their classes may be incomplete.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parse_errors: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct ClassEntry {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub class: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub r#abstract: bool,
    /// `added`, `removed`, `changed`, or `moved` — the FQN changed with a renamed file.
    pub change: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<String>,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<HeaderChange>,
    pub methods: Vec<MethodChange>,
    pub constructor: Option<ConstructorChange>,
}

struct Located {
    path: String,
    api: ClassApi,
}

fn is_php(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("php"))
}

pub(crate) fn symbol_changes(
    repo: &Path,
    git: &std::ffi::OsStr,
    merge_base: &str,
    head: &str,
    changes: &[ChangedFile],
) -> Result<(Vec<ClassEntry>, Vec<String>)> {
    let php: Vec<&ChangedFile> = changes
        .iter()
        .filter(|c| is_php(&c.path) || c.old_path.as_deref().is_some_and(is_php))
        .collect();

    let old_side = |c: &ChangedFile| -> Option<String> {
        match c.status {
            ChangeStatus::A => None,
            _ => Some(c.old_path.clone().unwrap_or_else(|| c.path.clone())),
        }
    };
    let new_side = |c: &ChangedFile| -> Option<String> {
        match c.status {
            ChangeStatus::D => None,
            _ => Some(c.path.clone()),
        }
    };
    let mut specs = Vec::new();
    for c in &php {
        if let Some(p) = old_side(c) {
            specs.push(format!("{merge_base}:{p}"));
        }
        if let Some(p) = new_side(c) {
            specs.push(format!("{head}:{p}"));
        }
    }
    let blobs = read_blobs(repo, git, &specs)?;

    let mut parse_errors = Vec::new();
    let mut parse_side = |rev: &str, path: &str, out: &mut Vec<Located>| -> Result<()> {
        let Some(content) = blobs.get(&format!("{rev}:{path}")) else {
            return Ok(());
        };
        let (classes, has_error) = php_api::class_apis(content)
            .with_context(|| format!("failed to parse {rev}:{path}"))?;
        if has_error && !parse_errors.iter().any(|p| p == path) {
            parse_errors.push(path.to_string());
        }
        out.extend(classes.into_iter().map(|api| Located {
            path: path.to_string(),
            api,
        }));
        Ok(())
    };
    let mut before = Vec::new();
    let mut after = Vec::new();
    for c in &php {
        if let Some(p) = old_side(c) {
            parse_side(merge_base, &p, &mut before)?;
        }
        if let Some(p) = new_side(c) {
            parse_side(head, &p, &mut after)?;
        }
    }

    // PHP class names are case-insensitive; a class keeps its identity across files by FQN.
    let mut unmatched_before: HashMap<String, usize> = HashMap::new();
    for (i, located) in before.iter().enumerate() {
        unmatched_before
            .entry(located.api.fqn.to_ascii_lowercase())
            .or_insert(i);
    }
    let mut pairs: Vec<(Option<usize>, Option<usize>)> = Vec::new();
    let mut unmatched_after = Vec::new();
    for (i, located) in after.iter().enumerate() {
        match unmatched_before.remove(&located.api.fqn.to_ascii_lowercase()) {
            Some(b) => pairs.push((Some(b), Some(i))),
            None => unmatched_after.push(i),
        }
    }
    let mut left_before: Vec<usize> = unmatched_before.into_values().collect();
    left_before.sort_unstable();

    // A renamed file whose one class changed its FQN: the class moved to another namespace.
    for c in php.iter().filter(|c| c.status == ChangeStatus::R) {
        let Some(old_path) = c.old_path.as_deref() else {
            continue;
        };
        let olds: Vec<usize> = left_before
            .iter()
            .copied()
            .filter(|&b| before[b].path == old_path)
            .collect();
        let news: Vec<usize> = unmatched_after
            .iter()
            .copied()
            .filter(|&a| after[a].path == c.path)
            .collect();
        if let ([b], [a]) = (olds.as_slice(), news.as_slice()) {
            pairs.push((Some(*b), Some(*a)));
            left_before.retain(|x| x != b);
            unmatched_after.retain(|x| x != a);
        }
    }
    pairs.extend(left_before.into_iter().map(|b| (Some(b), None)));
    pairs.extend(unmatched_after.into_iter().map(|a| (None, Some(a))));

    let mut entries = Vec::new();
    for (b, a) in pairs {
        let old = b.map(|i| &before[i]);
        let new = a.map(|i| &after[i]);
        let diff = php_api::diff_classes(old.map(|l| &l.api), new.map(|l| &l.api));
        let moved = matches!((old, new), (Some(o), Some(n)) if !o.api.fqn.eq_ignore_ascii_case(&n.api.fqn));
        let change = match (old, new) {
            (None, _) => "added",
            (_, None) => "removed",
            _ if moved => "moved",
            _ => "changed",
        };
        if change == "changed" && diff.is_empty() {
            continue;
        }
        let Some(shown) = new.or(old) else {
            continue;
        };
        entries.push(ClassEntry {
            path: shown.path.clone(),
            old_path: match (old, new) {
                (Some(o), Some(n)) if o.path != n.path => Some(o.path.clone()),
                _ => None,
            },
            class: shown.api.fqn.clone(),
            kind: shown.api.kind,
            r#abstract: shown.api.is_abstract,
            change,
            moved_from: moved.then(|| old.map(|o| o.api.fqn.clone())).flatten(),
            line: shown.api.line,
            header: diff.header,
            methods: diff.methods,
            constructor: diff.constructor,
        });
    }
    entries.sort_by(|x, y| (&x.path, x.line).cmp(&(&y.path, y.line)));
    parse_errors.sort();
    Ok((entries, parse_errors))
}

/// Contents of `<rev>:<path>` specs through one `git cat-file --batch`; missing ones are absent.
fn read_blobs(
    repo: &Path,
    git: &std::ffi::OsStr,
    specs: &[String],
) -> Result<HashMap<String, String>> {
    // The batch protocol is line based: a path with a newline cannot be asked for.
    let specs: Vec<&String> = specs.iter().filter(|s| !s.contains('\n')).collect();
    let mut blobs = HashMap::new();
    if specs.is_empty() {
        return Ok(blobs);
    }
    let mut child = Command::new(git)
        .args(["cat-file", "--batch"])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start git cat-file")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("git cat-file has no stdin"))?;
    let requests: String = specs.iter().map(|s| format!("{s}\n")).collect();
    // Written from another thread: git answers while it reads, and both pipes have a limit.
    let writer = std::thread::spawn(move || stdin.write_all(requests.as_bytes()));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("git cat-file has no stdout"))?;
    let mut reader = BufReader::new(stdout);
    for spec in specs {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.ends_with(" missing") || header.ends_with(" ambiguous") {
            continue;
        }
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("unexpected git cat-file header '{header}'"))?;
        let mut content = vec![0u8; size + 1];
        reader.read_exact(&mut content)?;
        content.pop();
        blobs.insert(spec.clone(), String::from_utf8_lossy(&content).into_owned());
    }
    writer
        .join()
        .map_err(|_| anyhow!("git cat-file writer panicked"))?
        .context("failed to write to git cat-file")?;
    child.wait().context("git cat-file did not exit")?;
    Ok(blobs)
}

pub(crate) fn render_symbols(result: &SymbolsResult, format: &str) -> Result<String> {
    if format == "json" {
        let mut rendered = serde_json::to_string_pretty(result)?;
        rendered.push('\n');
        return Ok(rendered);
    }
    let mut out = format!(
        "{}\n",
        format!(
            "PHP API changes against {} (merge base {}) -> {}: {} class(es)",
            result.base,
            short_sha(&result.merge_base),
            result.head,
            result.classes.len()
        )
        .bold()
    );
    let mut last_path = "";
    for entry in &result.classes {
        if entry.path != last_path {
            out.push_str(&format!("{}\n", entry.path.cyan()));
            last_path = &entry.path;
        }
        let mark = match entry.change {
            "added" => "+",
            "removed" => "-",
            "moved" => ">",
            _ => "~",
        };
        let kind = if entry.r#abstract {
            format!("abstract {}", entry.kind)
        } else {
            entry.kind.to_string()
        };
        out.push_str(&format!("  {mark} {kind} {}", entry.class));
        if let Some(from) = &entry.moved_from {
            out.push_str(&format!(" (from {from})"));
        }
        out.push_str(&format!("  :{}\n", entry.line));
        if let Some(h) = &entry.header {
            out.push_str(&format!("      header: {} -> {}\n", h.before, h.after));
        }
        if let Some(c) = &entry.constructor {
            let shown = match (&c.before, &c.after) {
                (Some(b), Some(a)) => format!("{b} -> {a}"),
                (Some(b), None) => format!("removed {b}"),
                (None, Some(a)) => format!("added {a}"),
                (None, None) => String::new(),
            };
            out.push_str(&format!("      constructor: {shown}\n"));
            for p in &c.params.added {
                let required = if p.required { " (required)" } else { "" };
                out.push_str(&format!("        + {}{required}\n", p.param));
            }
            for p in &c.params.removed {
                out.push_str(&format!("        - {p}\n"));
            }
            for p in &c.params.changed {
                out.push_str(&format!("        ~ {} -> {}\n", p.before, p.after));
            }
            if c.params.reordered {
                out.push_str("        parameters reordered\n");
            }
        }
        for m in &entry.methods {
            let visibility = match m.visibility_before {
                Some(before) => format!("{before} -> {}", m.visibility),
                None => m.visibility.to_string(),
            };
            let line = match (m.change, &m.before, &m.after) {
                ("added", _, Some(a)) => format!("+ {a}"),
                ("removed", Some(b), _) => format!("- {b}"),
                ("changed", Some(b), Some(a)) => format!("~ {b} -> {a}"),
                _ => format!("* {} (body)", m.name),
            };
            out.push_str(&format!("      {line}  [{visibility}] :{}\n", m.line));
        }
    }
    if !result.parse_errors.is_empty() {
        out.push_str(&format!(
            "{} syntax errors in: {}\n",
            "note:".yellow(),
            result.parse_errors.join(", ")
        ));
    }
    Ok(out)
}

fn short_sha(sha: &str) -> &str {
    sha.get(..10).unwrap_or(sha)
}
