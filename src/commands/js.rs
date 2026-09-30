//! JavaScript/TypeScript/Vue module queries on top of `js_imports`, `js_exports` and `js_uses`.
//!
//! - `usages 'path#name'`, `impact 'path#name'`: every place that takes one export — imports, re-export
//!   chains, uses of the imported name (code, TS types, Vue templates), JSDoc `import()` types, mocks and
//!   dynamic imports of the module, uses inside the defining file;
//! - `impact 'path'`: every reference to the module file;
//! - `move-plan 'path' 'new/path'`: specifiers to rewrite when the file moves, in the style each was written;
//! - `unused_exports`: exports nobody imports and modules nobody references, for `unused-symbols`.
//!
//! Specifiers are resolved here, at query time, the way bundlers and TypeScript do: relative paths, `paths`
//! of the nearest tsconfig.json/jsconfig.json (with `baseUrl`, `extends`, `references`), `js_aliases` from
//! `.ast-index.yaml`, extensions and `index.*`. The index keeps specifiers as written, so a moved or added
//! file never leaves stale targets behind.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use anyhow::{bail, Result};
use colored::Colorize;
use regex::Regex;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::db;
use crate::indexer;

/// Extensions of module files, in the order a specifier without one is tried.
pub const JS_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "vue"];
const COVERAGE: &str = "JS/TS/Vue specifiers resolved by path (relative, tsconfig/jsconfig paths, js_aliases, \
extensions, index files), re-exports, import(), require(), vi.mock/jest.mock, import.meta.glob, JSDoc import() \
types, uses of imported names in code, TS types and Vue templates, uses in the defining file; not covered: \
specifiers built at run time, CommonJS exports, files excluded from the index";
/// Above this many references the text output lists files with counts instead of every line.
const FULL_LIMIT: usize = 60;

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// A JS/TS module argument: `path#export`, or a path with a module extension or a slash (not a PHP FQN).
pub fn is_js_target(arg: &str) -> bool {
    if arg.contains('\\') || arg.contains("::") {
        return false;
    }
    let path = arg.split('#').next().unwrap_or(arg);
    arg.contains('#') || has_js_extension(path) || path.contains('/')
}

fn has_js_extension(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| JS_EXTENSIONS.contains(&e))
}

/// `vite.config.js`, `eslint.config.mjs`, `vitest.config.ts`: loaded by the tool, never imported.
fn is_tool_config(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("");
    has_js_extension(name) && (stem.ends_with(".config") || stem.contains(".config.") || stem.starts_with(".eslintrc"))
}

fn is_test_path(path: &str) -> bool {
    path.starts_with("tests/")
        || path.starts_with("test/")
        || path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("/__tests__/")
        || path.contains(".test.")
        || path.contains(".spec.")
        || path.contains("/e2e/")
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `a/b/../c` → `a/c` without touching the file system (symlinks keep their indexed paths).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Relative path from `from_dir` to `to`, with `/` separators.
fn relative(from_dir: &Path, to: &Path) -> String {
    let from: Vec<Component> = from_dir.components().collect();
    let to_parts: Vec<Component> = to.components().collect();
    let common = from.iter().zip(&to_parts).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    parts.extend(to_parts[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    let joined = parts.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

/// Files a specifier base may name: itself, with each module extension, `index.*` inside it, and the
/// TypeScript habit of writing `./x.js` for `x.ts`.
fn candidates(base: &Path) -> Vec<PathBuf> {
    let mut out = vec![base.to_path_buf()];
    let text = base.to_string_lossy();
    for (js, ts) in [("js", "ts"), ("mjs", "mts"), ("cjs", "cts"), ("jsx", "tsx")] {
        if let Some(stem) = text.strip_suffix(&format!(".{js}")) {
            out.push(PathBuf::from(format!("{stem}.{ts}")));
        }
    }
    for ext in JS_EXTENSIONS {
        out.push(PathBuf::from(format!("{text}.{ext}")));
    }
    for ext in JS_EXTENSIONS {
        out.push(base.join(format!("index.{ext}")));
    }
    out
}

/// JSON with comments and trailing commas, as tsconfig.json allows.
fn parse_jsonc(text: &str) -> Option<serde_json::Value> {
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if c == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        } else if c == b',' {
            let rest = text[i + 1..].trim_start();
            if rest.starts_with('}') || rest.starts_with(']') {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    serde_json::from_slice(&out).ok()
}

/// `paths` of a tsconfig/jsconfig: pattern → targets, relative to `base`.
#[derive(Debug, Default)]
struct PathConfig {
    base: PathBuf,
    paths: Vec<(String, Vec<String>)>,
}

#[derive(Debug, Clone, PartialEq)]
enum Target {
    Module(usize),
    Modules(Vec<usize>),
    Asset,
    Package,
    Unresolved,
}

struct Resolver {
    root: PathBuf,
    /// `js_aliases` from `.ast-index.yaml`: prefix → directory (absolute).
    aliases: Vec<(String, PathBuf)>,
    configs: RefCell<HashMap<PathBuf, Option<Rc<PathConfig>>>>,
    package_dirs: RefCell<HashMap<PathBuf, Option<PathBuf>>>,
}

impl Resolver {
    fn new(root: &Path) -> Self {
        let mut aliases: Vec<(String, PathBuf)> = indexer::load_config_quiet(root)
            .and_then(|c| c.js_aliases)
            .unwrap_or_default()
            .into_iter()
            .map(|(prefix, dir)| (prefix, normalize(&root.join(dir))))
            .collect();
        aliases.sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        Resolver {
            root: root.to_path_buf(),
            aliases,
            configs: RefCell::new(HashMap::new()),
            package_dirs: RefCell::new(HashMap::new()),
        }
    }

    /// The tsconfig/jsconfig governing files of `dir`: the nearest one up to the project root.
    fn config_for(&self, dir: &Path) -> Option<Rc<PathConfig>> {
        if let Some(cached) = self.configs.borrow().get(dir) {
            return cached.clone();
        }
        let mut found = None;
        for name in ["tsconfig.json", "jsconfig.json"] {
            let file = dir.join(name);
            if file.is_file() {
                found = Some(Rc::new(load_path_config(&file, &self.root, 0)));
                break;
            }
        }
        if found.is_none() && dir != self.root && dir.starts_with(&self.root) {
            if let Some(parent) = dir.parent() {
                found = self.config_for(parent);
            }
        }
        self.configs.borrow_mut().insert(dir.to_path_buf(), found.clone());
        found
    }

    /// Nearest directory with package.json: the root of `/src/...` specifiers in Vite projects.
    fn package_dir(&self, dir: &Path) -> Option<PathBuf> {
        if let Some(cached) = self.package_dirs.borrow().get(dir) {
            return cached.clone();
        }
        let found = if dir.join("package.json").is_file() {
            Some(dir.to_path_buf())
        } else if dir != self.root && dir.starts_with(&self.root) {
            dir.parent().and_then(|p| self.package_dir(p))
        } else {
            None
        };
        self.package_dirs.borrow_mut().insert(dir.to_path_buf(), found.clone());
        found
    }

    /// Absolute bases a specifier may point to, most specific first; empty for a package.
    fn bases(&self, importer_dir: &Path, spec: &str) -> Vec<PathBuf> {
        if spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../") {
            return vec![normalize(&importer_dir.join(spec))];
        }
        if let Some(rest) = spec.strip_prefix('/') {
            let mut out = Vec::new();
            if let Some(dir) = self.package_dir(importer_dir) {
                out.push(normalize(&dir.join(rest)));
            }
            out.push(normalize(&self.root.join(rest)));
            return out;
        }
        let mut out: Vec<PathBuf> = self
            .aliases
            .iter()
            .filter_map(|(prefix, dir)| spec.strip_prefix(prefix.as_str()).map(|rest| normalize(&dir.join(rest))))
            .collect();
        if let Some(config) = self.config_for(importer_dir) {
            // TypeScript takes the matching pattern with the longest prefix
            let mut matches: Vec<(usize, &Vec<String>, String)> = Vec::new();
            for (pattern, targets) in &config.paths {
                match pattern.split_once('*') {
                    Some((prefix, suffix)) => {
                        if spec.len() >= prefix.len() + suffix.len()
                            && spec.starts_with(prefix)
                            && spec.ends_with(suffix)
                        {
                            let star = spec[prefix.len()..spec.len() - suffix.len()].to_string();
                            matches.push((prefix.len(), targets, star));
                        }
                    }
                    None if pattern == spec => matches.push((usize::MAX, targets, String::new())),
                    None => {}
                }
            }
            matches.sort_by_key(|m| std::cmp::Reverse(m.0));
            for (_, targets, star) in matches {
                for target in targets {
                    out.push(normalize(&config.base.join(target.replace('*', &star))));
                }
            }
        }
        out
    }

    fn resolve(&self, importer_dir: &Path, spec: &str, kind: &str, project: &ModuleSet) -> Target {
        let spec = spec.split('?').next().unwrap_or(spec);
        if kind == "glob" {
            return self.resolve_glob(importer_dir, spec, project);
        }
        let bases = self.bases(importer_dir, spec);
        if bases.is_empty() {
            return Target::Package;
        }
        for base in &bases {
            for candidate in candidates(base) {
                if let Some(&index) = project.by_abs.get(&candidate) {
                    return Target::Module(index);
                }
            }
        }
        if bases.iter().any(|b| b.is_file()) {
            Target::Asset
        } else {
            Target::Unresolved
        }
    }

    /// `import.meta.glob('./blocks/*.vue')`: every indexed module the pattern matches.
    fn resolve_glob(&self, importer_dir: &Path, pattern: &str, project: &ModuleSet) -> Target {
        let wildcard = pattern.find(['*', '?', '{', '[']).unwrap_or(pattern.len());
        let static_end = pattern[..wildcard].rfind('/').map(|i| i + 1).unwrap_or(0);
        let (prefix, rest) = pattern.split_at(static_end);
        let Some(base) = self.bases(importer_dir, if prefix.is_empty() { "./" } else { prefix }).into_iter().next()
        else {
            return Target::Package;
        };
        let Some(regex) = glob_regex(rest) else {
            return Target::Unresolved;
        };
        let mut found: Vec<usize> = project
            .modules
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.abs
                    .strip_prefix(&base)
                    .ok()
                    .and_then(|r| r.to_str())
                    .is_some_and(|r| regex.is_match(r))
            })
            .map(|(i, _)| i)
            .collect();
        found.sort_unstable();
        if found.is_empty() {
            Target::Asset
        } else {
            Target::Modules(found)
        }
    }
}

fn glob_regex(glob: &str) -> Option<Regex> {
    let mut out = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                out.push_str(".*");
                i += 2;
                if chars.get(i) == Some(&'/') {
                    i += 1;
                }
                continue;
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '{' => out.push('('),
            '}' => out.push(')'),
            ',' => out.push('|'),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    out.push('$');
    Regex::new(&out).ok()
}

fn load_path_config(file: &Path, root: &Path, depth: usize) -> PathConfig {
    let dir = file.parent().unwrap_or(root).to_path_buf();
    let Some(json) = fs::read_to_string(file).ok().and_then(|t| parse_jsonc(&t)) else {
        return PathConfig {
            base: dir,
            paths: vec![],
        };
    };
    let mut config = PathConfig {
        base: dir.clone(),
        paths: vec![],
    };
    if depth < 5 {
        // `extends` first: the file's own options override what it extends
        let extends: Vec<String> = match &json["extends"] {
            serde_json::Value::String(s) => vec![s.clone()],
            serde_json::Value::Array(items) => items.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => vec![],
        };
        for parent in extends {
            let path = if parent.starts_with('.') {
                dir.join(&parent)
            } else {
                root.join("node_modules").join(&parent)
            };
            let path = if path.extension().is_none() {
                path.with_extension("json")
            } else {
                path
            };
            let inherited = load_path_config(&normalize(&path), root, depth + 1);
            if !inherited.paths.is_empty() {
                config = inherited;
            }
        }
    }
    let options = &json["compilerOptions"];
    if let Some(base_url) = options["baseUrl"].as_str() {
        config.base = normalize(&dir.join(base_url));
    }
    if let Some(paths) = options["paths"].as_object() {
        if options["baseUrl"].as_str().is_none() {
            config.base = dir.clone();
        }
        config.paths = paths
            .iter()
            .map(|(pattern, targets)| {
                let targets = targets
                    .as_array()
                    .map(|t| t.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                (pattern.clone(), targets)
            })
            .collect();
    }
    // a solution-style tsconfig.json keeps `paths` in the configs it references
    if config.paths.is_empty() && depth < 5 {
        if let Some(references) = json["references"].as_array() {
            for reference in references.iter().filter_map(|r| r["path"].as_str()) {
                let path = dir.join(reference);
                let path = if path.is_dir() { path.join("tsconfig.json") } else { path };
                let referenced = load_path_config(&normalize(&path), root, depth + 1);
                if !referenced.paths.is_empty() {
                    return referenced;
                }
            }
        }
    }
    config
}

// ---------------------------------------------------------------------------
// The project's module graph
// ---------------------------------------------------------------------------

struct Module {
    id: i64,
    shown: String,
    abs: PathBuf,
}

#[derive(Clone)]
struct Import {
    file: usize,
    line: i64,
    name_line: i64,
    kind: String,
    spec: String,
    imported: Option<String>,
    local: Option<String>,
}

#[derive(Clone)]
struct Export {
    file: usize,
    line: i64,
    name: String,
    local: Option<String>,
    decl_line: Option<i64>,
    kind: String,
}

struct ModuleSet {
    modules: Vec<Module>,
    by_abs: HashMap<PathBuf, usize>,
}

struct Project {
    root: PathBuf,
    set: ModuleSet,
    imports: Vec<Import>,
    exports: Vec<Export>,
    /// Resolved target of each import, by index in `imports`.
    targets: Vec<Target>,
    lines: RefCell<HashMap<usize, Rc<Vec<String>>>>,
}

impl Project {
    fn load(conn: &Connection, root: &Path) -> Result<Self> {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, path, root_path FROM files WHERE {} AND external = 0 ORDER BY path",
            db::JS_MODULE_PATH_SQL
        ))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut modules = Vec::with_capacity(rows.len());
        let mut by_abs = HashMap::with_capacity(rows.len());
        let mut by_id = HashMap::with_capacity(rows.len());
        for (id, path, root_path) in rows {
            let base = match root_path.as_deref() {
                Some(r) if !r.is_empty() => PathBuf::from(r),
                _ => root.to_path_buf(),
            };
            let abs = normalize(&base.join(&path));
            let shown = abs
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| abs.to_string_lossy().into_owned());
            by_abs.insert(abs.clone(), modules.len());
            by_id.insert(id, modules.len());
            modules.push(Module { id, shown, abs });
        }

        let mut stmt = conn.prepare(
            "SELECT file_id, line, name_line, kind, spec, imported, local FROM js_imports ORDER BY file_id, line",
        )?;
        let imports: Vec<Import> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    Import {
                        file: 0,
                        line: row.get(1)?,
                        name_line: row.get(2)?,
                        kind: row.get(3)?,
                        spec: row.get(4)?,
                        imported: row.get(5)?,
                        local: row.get(6)?,
                    },
                ))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|(id, mut imp)| {
                imp.file = *by_id.get(&id)?;
                Some(imp)
            })
            .collect();

        let mut stmt = conn.prepare(
            "SELECT file_id, line, name, local, decl_line, kind FROM js_exports ORDER BY file_id, line",
        )?;
        let exports: Vec<Export> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    Export {
                        file: 0,
                        line: row.get(1)?,
                        name: row.get(2)?,
                        local: row.get(3)?,
                        decl_line: row.get(4)?,
                        kind: row.get(5)?,
                    },
                ))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|(id, mut exp)| {
                exp.file = *by_id.get(&id)?;
                Some(exp)
            })
            .collect();

        let set = ModuleSet { modules, by_abs };
        let resolver = Resolver::new(root);
        let mut memo: HashMap<(PathBuf, String, bool), Target> = HashMap::new();
        let targets = imports
            .iter()
            .map(|imp| {
                let dir = set.modules[imp.file].abs.parent().unwrap_or(root).to_path_buf();
                let key = (dir.clone(), imp.spec.clone(), imp.kind == "glob");
                memo.entry(key)
                    .or_insert_with(|| resolver.resolve(&dir, &imp.spec, &imp.kind, &set))
                    .clone()
            })
            .collect();
        if set.modules.is_empty() {
            bail!("The index has no JS/TS/Vue files.");
        }
        let has_facts: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM js_exports)", [], |r| r.get(0))?;
        if !has_facts {
            bail!("The index has no JS/TS module facts yet. Run 'ast-index update' (or 'ast-index rebuild').");
        }

        Ok(Project {
            root: root.to_path_buf(),
            set,
            imports,
            exports,
            targets,
            lines: RefCell::new(HashMap::new()),
        })
    }

    fn points_to(&self, index: usize, module: usize) -> bool {
        match &self.targets[index] {
            Target::Module(m) => *m == module,
            Target::Modules(ms) => ms.binary_search(&module).is_ok(),
            _ => false,
        }
    }

    fn file_lines(&self, file: usize) -> Rc<Vec<String>> {
        if let Some(lines) = self.lines.borrow().get(&file) {
            return lines.clone();
        }
        let lines: Rc<Vec<String>> = Rc::new(
            fs::read(&self.set.modules[file].abs)
                .map(|b| String::from_utf8_lossy(&b).lines().map(String::from).collect())
                .unwrap_or_default(),
        );
        self.lines.borrow_mut().insert(file, lines.clone());
        lines
    }

    fn context(&self, file: usize, line: i64) -> String {
        self.file_lines(file)
            .get((line.max(1) - 1) as usize)
            .map(|l| truncate(l, 110))
            .unwrap_or_default()
    }

    /// The module an argument names: a path relative to the project root or the working directory, with or
    /// without extension, a directory with `index.*`, or a unique file-name suffix.
    fn module_arg(&self, arg: &str) -> Result<usize> {
        let arg = arg.trim().trim_start_matches("./");
        let mut bases = vec![normalize(&self.root.join(arg))];
        if let Ok(cwd) = std::env::current_dir() {
            bases.push(normalize(&cwd.join(arg)));
        }
        for base in &bases {
            for candidate in candidates(base) {
                if let Some(&index) = self.set.by_abs.get(&candidate) {
                    return Ok(index);
                }
            }
        }
        let suffix = format!("/{arg}");
        let matches: Vec<usize> = self
            .set
            .modules
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                let shown = format!("/{}", m.shown);
                candidates(Path::new(&suffix)).iter().any(|c| shown.ends_with(&*c.to_string_lossy()))
            })
            .map(|(i, _)| i)
            .collect();
        match matches.as_slice() {
            [one] => Ok(*one),
            [] => bail!("no JS/TS/Vue module '{arg}' in the index"),
            many => bail!(
                "'{arg}' matches {} modules: {}",
                many.len(),
                many.iter().map(|&i| self.set.modules[i].shown.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    fn exports_of(&self, module: usize) -> impl Iterator<Item = &Export> {
        self.exports.iter().filter(move |e| e.file == module)
    }

    /// Specifiers that did not resolve but end with this module's name — possibly pointing here.
    fn unresolved_near(&self, module: usize) -> Vec<(usize, i64, String)> {
        let abs = &self.set.modules[module].abs;
        let stem = abs.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        let name = if stem == "index" {
            abs.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or_default()
        } else {
            stem
        };
        self.imports
            .iter()
            .enumerate()
            .filter(|(i, imp)| {
                self.targets[*i] == Target::Unresolved && {
                    let spec = imp.spec.split('?').next().unwrap_or(&imp.spec);
                    let last = spec.rsplit('/').next().unwrap_or(spec);
                    last == name || last.split('.').next() == Some(name)
                }
            })
            .map(|(_, imp)| (imp.file, imp.line, imp.spec.clone()))
            .collect()
    }
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

fn open(root: &Path) -> Result<db::LeasedConnection> {
    if !db::db_exists(root) {
        bail!("Index not found. Run 'ast-index rebuild' first.");
    }
    db::open_db_leased(root)
}

// ---------------------------------------------------------------------------
// References of one export
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
struct Reference {
    path: String,
    line: i64,
    kind: String,
    test: bool,
    context: String,
    /// The re-exporting module the reference came through.
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
    #[serde(skip)]
    file: usize,
}

#[derive(Serialize, Clone)]
struct Definition {
    path: String,
    line: i64,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    declared_at: Option<i64>,
}

fn word_lines(lines: &[String], word: &str) -> Vec<i64> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.match_indices(word).any(|(at, _)| {
                let before = line[..at].chars().next_back();
                let after = line[at + word.len()..].chars().next();
                !before.is_some_and(|c| is_ident(c) || c == '.') && !after.is_some_and(is_ident)
            })
        })
        .map(|(i, _)| i as i64 + 1)
        .collect()
}

impl Project {
    fn reference(&self, file: usize, line: i64, kind: &str, via: Option<&str>) -> Reference {
        let path = self.set.modules[file].shown.clone();
        Reference {
            test: is_test_path(&path),
            context: self.context(file, line),
            path,
            line,
            kind: kind.to_string(),
            via: via.map(String::from),
            file,
        }
    }

    fn uses(&self, conn: &Connection, file: usize, local: &str, member: Option<&str>) -> Result<Vec<i64>> {
        let id = self.set.modules[file].id;
        let lines = match member {
            None => conn
                .prepare_cached("SELECT line FROM js_uses WHERE file_id = ?1 AND local = ?2 AND member IS NULL")?
                .query_map(params![id, local], |r| r.get(0))?
                .collect::<Result<Vec<i64>, _>>()?,
            Some(m) => conn
                .prepare_cached("SELECT line FROM js_uses WHERE file_id = ?1 AND local = ?2 AND member = ?3")?
                .query_map(params![id, local, m], |r| r.get(0))?
                .collect::<Result<Vec<i64>, _>>()?,
        };
        Ok(lines)
    }

    /// Definitions of `name` in `module`, or an error listing what the module exports.
    fn definitions(&self, module: usize, name: &str) -> Result<Vec<Definition>> {
        let defs: Vec<Definition> = self
            .exports_of(module)
            .filter(|e| e.name == name)
            .map(|e| Definition {
                path: self.set.modules[module].shown.clone(),
                line: e.line,
                kind: e.kind.clone(),
                declared_at: e.decl_line.filter(|d| *d != e.line),
            })
            .collect();
        if defs.is_empty() {
            let mut names: Vec<&str> = self.exports_of(module).map(|e| e.name.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            bail!(
                "{} does not export '{name}'; its exports: {}",
                self.set.modules[module].shown,
                if names.is_empty() { "none".to_string() } else { names.join(", ") }
            );
        }
        Ok(defs)
    }

    /// Every place that takes `name` from `module`, following re-exports.
    fn export_references(&self, conn: &Connection, module: usize, name: &str) -> Result<Vec<Reference>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut queue: Vec<(usize, String, Option<String>)> = vec![(module, name.to_string(), None)];
        while let Some((target, name, via)) = queue.pop() {
            if !seen.insert((target, name.clone())) {
                continue;
            }
            let via_ref = via.as_deref();
            for (i, imp) in self.imports.iter().enumerate() {
                if !self.points_to(i, target) {
                    continue;
                }
                let imported = imp.imported.as_deref();
                let label = if imp.kind == "dynamic" { "dynamic" } else { "import" };
                match imp.kind.as_str() {
                    "import" | "require" | "dynamic" => match (imported, imp.local.as_deref()) {
                        (Some(n), None) if n == name => {
                            out.push(self.reference(imp.file, imp.name_line, label, via_ref));
                        }
                        (None, _) => {
                            // `import()` of the whole module: the export may be used through it
                            out.push(self.reference(imp.file, imp.line, label, via_ref));
                        }
                        (Some(n), Some(local)) if n == name => {
                            out.push(self.reference(imp.file, imp.name_line, label, via_ref));
                            for line in self.uses(conn, imp.file, local, None)? {
                                out.push(self.reference(imp.file, line, "use", via_ref));
                            }
                            // `import { x } …; export { x }` passes the name on
                            for e in self.exports_of(imp.file).filter(|e| e.local.as_deref() == Some(local)) {
                                out.push(self.reference(imp.file, e.line, "reexport", via_ref));
                                let through = format!("{}#{}", self.set.modules[imp.file].shown, e.name);
                                queue.push((imp.file, e.name.clone(), Some(through)));
                            }
                        }
                        (Some("*"), Some(local)) => {
                            let lines = self.uses(conn, imp.file, local, Some(&name))?;
                            if !lines.is_empty() {
                                let kind = if imp.kind == "dynamic" { "dynamic" } else { "namespace" };
                                out.push(self.reference(imp.file, imp.name_line, kind, via_ref));
                            }
                            for line in lines {
                                out.push(self.reference(imp.file, line, "use", via_ref));
                            }
                        }
                        _ => {}
                    },
                    "reexport" => match (imported, imp.local.as_deref()) {
                        (Some(n), alias) if n == name => {
                            out.push(self.reference(imp.file, imp.name_line, "reexport", via_ref));
                            let exported = alias.unwrap_or(n).to_string();
                            let through = format!("{}#{}", self.set.modules[imp.file].shown, exported);
                            queue.push((imp.file, exported, Some(through)));
                        }
                        (Some("*"), None) => {
                            let through = format!("{}#{}", self.set.modules[imp.file].shown, name);
                            queue.push((imp.file, name.clone(), Some(through)));
                        }
                        (Some("*"), Some(_)) => {
                            out.push(self.reference(imp.file, imp.line, "reexport", via_ref));
                        }
                        _ => {}
                    },
                    "jsdoc" if imported == Some(name.as_str()) => {
                        out.push(self.reference(imp.file, imp.line, "jsdoc", via_ref));
                    }
                    // take the whole module: the export may be used through them
                    "glob" | "mock" => {
                        out.push(self.reference(imp.file, imp.line, &imp.kind, via_ref));
                    }
                    _ => {}
                }
            }
        }

        // uses inside the defining file: a rename changes them too
        let local_names: HashSet<String> = self
            .exports_of(module)
            .filter(|e| e.name == name)
            .map(|e| e.local.clone().unwrap_or_else(|| e.name.clone()))
            .filter(|n| n != "default")
            .collect();
        let export_lines: HashSet<i64> = self
            .exports_of(module)
            .filter(|e| e.name == name)
            .flat_map(|e| [Some(e.line), e.decl_line])
            .flatten()
            .collect();
        let lines = self.file_lines(module);
        for local in &local_names {
            for line in word_lines(&lines, local) {
                if !export_lines.contains(&line) {
                    out.push(self.reference(module, line, "local", None));
                }
            }
        }

        out.sort_by(|a, b| (a.test, &a.path, a.line, &a.kind).cmp(&(b.test, &b.path, b.line, &b.kind)));
        out.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.kind == b.kind);
        Ok(out)
    }

    /// Every reference to the module file itself.
    fn module_references(&self, module: usize) -> Vec<Reference> {
        let mut out: Vec<Reference> = Vec::new();
        let mut seen = HashSet::new();
        for (i, imp) in self.imports.iter().enumerate() {
            if self.points_to(i, module) && seen.insert((imp.file, imp.line, imp.kind.clone())) {
                out.push(self.reference(imp.file, imp.line, &imp.kind, None));
            }
        }
        out.sort_by(|a, b| (a.test, &a.path, a.line).cmp(&(b.test, &b.path, b.line)));
        out
    }
}

fn split_target(arg: &str) -> (&str, Option<&str>) {
    match arg.rsplit_once('#') {
        Some((path, name)) if !name.is_empty() => (path, Some(name)),
        _ => (arg.trim_end_matches('#'), None),
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_references(refs: &[Reference], full: bool) {
    if full || refs.len() <= FULL_LIMIT {
        let mut current = "";
        for r in refs {
            if r.path != current {
                current = &r.path;
                println!("{}{}", r.path.cyan(), if r.test { "  (test)" } else { "" });
            }
            let via = r.via.as_deref().map(|v| format!("  via {v}")).unwrap_or_default();
            println!("    {:>5} {:<9} {}{}", r.line, r.kind, r.context, via.dimmed());
        }
    } else {
        let mut by_file: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
        for r in refs {
            *by_file.entry(&r.path).or_default().entry(&r.kind).or_default() += 1;
        }
        for (path, kinds) in by_file {
            let counts: Vec<String> = kinds.iter().map(|(k, n)| format!("{k} {n}")).collect();
            println!("  {}  {}", path.cyan(), counts.join(", "));
        }
        println!("  (per-file summary; --full lists every line)");
    }
}

fn summary(refs: &[Reference]) -> (BTreeMap<&str, usize>, usize, usize) {
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for r in refs {
        *by_kind.entry(&r.kind).or_default() += 1;
    }
    let files: HashSet<&str> = refs.iter().map(|r| r.path.as_str()).collect();
    let tests = refs.iter().filter(|r| r.test).count();
    (by_kind, files.len(), tests)
}

// ---------------------------------------------------------------------------
// usages / impact
// ---------------------------------------------------------------------------

pub fn cmd_js_usages(root: &Path, target: &str, limit: usize, format: &str) -> Result<()> {
    let _lease = db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let project = Project::load(&conn, root)?;
    let (path, name) = split_target(target);
    let module = project.module_arg(path)?;
    let shown = project.set.modules[module].shown.clone();
    let refs = match name {
        Some(name) => {
            project.definitions(module, name)?;
            project
                .export_references(&conn, module, name)?
                .into_iter()
                .filter(|r| r.kind != "local")
                .collect()
        }
        None => project.module_references(module),
    };
    let label = name.map(|n| format!("{shown}#{n}")).unwrap_or(shown);
    let total = refs.len();
    let shown_refs: Vec<&Reference> = refs.iter().take(limit).collect();
    if format == "json" {
        return print_json(&serde_json::json!({
            "target": label,
            "items": shown_refs,
            "pagination": {"total": total, "returned": shown_refs.len(), "truncated": total > shown_refs.len(), "limit": limit},
        }));
    }
    println!("{}", format!("Usages of {label} (showing {} of {total}):", shown_refs.len()).bold());
    for r in shown_refs {
        println!("  {}:{}  [{}] {}", r.path.cyan(), r.line, r.kind, r.context);
    }
    if total > limit {
        println!("  … {} more; rerun with a larger --limit", total - limit);
    }
    Ok(())
}

pub fn cmd_js_impact(root: &Path, target: &str, full: bool, format: &str) -> Result<()> {
    let _lease = db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let project = Project::load(&conn, root)?;
    let (path, name) = split_target(target);
    let module = project.module_arg(path)?;
    let shown = project.set.modules[module].shown.clone();
    let unresolved: Vec<serde_json::Value> = project
        .unresolved_near(module)
        .into_iter()
        .map(|(file, line, spec)| {
            serde_json::json!({"path": project.set.modules[file].shown, "line": line, "spec": spec})
        })
        .collect();

    let Some(name) = name else {
        return module_impact(&project, &conn, module, &unresolved, full, format);
    };
    let definitions = project.definitions(module, name)?;
    let refs = project.export_references(&conn, module, name)?;
    let (by_kind, files, tests) = summary(&refs);
    let label = format!("{shown}#{name}");
    if format == "json" {
        return print_json(&serde_json::json!({
            "target": label,
            "definitions": definitions,
            "references": refs,
            "summary": {"references": refs.len(), "files": files, "tests": tests, "by_kind": by_kind},
            "unresolved": unresolved,
            "coverage": COVERAGE,
        }));
    }
    println!("{}", format!("Impact of {label}").bold());
    for d in &definitions {
        let declared = d.declared_at.map(|l| format!(", declared at :{l}")).unwrap_or_default();
        println!("  definition: {}:{} ({}{declared})", d.path.cyan(), d.line, d.kind);
    }
    let kinds: Vec<String> = by_kind.iter().map(|(k, n)| format!("{k} {n}")).collect();
    println!("  {} references in {files} files: {} (tests: {tests})", refs.len(), kinds.join(", "));
    print_references(&refs, full);
    print_unresolved(&unresolved);
    println!("{} {COVERAGE}", "coverage:".dimmed());
    Ok(())
}

fn print_unresolved(unresolved: &[serde_json::Value]) {
    if unresolved.is_empty() {
        return;
    }
    println!(
        "  {} {} specifiers did not resolve and end with this module's name — check them:",
        "unresolved:".yellow(),
        unresolved.len()
    );
    for u in unresolved {
        println!("    {}:{}  '{}'", u["path"].as_str().unwrap_or(""), u["line"], u["spec"].as_str().unwrap_or(""));
    }
}

fn module_impact(
    project: &Project,
    conn: &Connection,
    module: usize,
    unresolved: &[serde_json::Value],
    full: bool,
    format: &str,
) -> Result<()> {
    let shown = &project.set.modules[module].shown;
    let refs = project.module_references(module);
    let mut names: Vec<String> = project.exports_of(module).map(|e| e.name.clone()).collect();
    names.sort_unstable();
    names.dedup();
    let mut exports = Vec::new();
    for name in &names {
        let used: Vec<Reference> = project
            .export_references(conn, module, name)?
            .into_iter()
            // a mock replaces the module without using its exports
            .filter(|r| r.file != module && r.kind != "mock")
            .collect();
        let files: HashSet<&str> = used.iter().map(|r| r.path.as_str()).collect();
        exports.push((name.clone(), used.len(), files.len()));
    }
    let (by_kind, files, tests) = summary(&refs);
    if format == "json" {
        let exports: Vec<serde_json::Value> = exports
            .iter()
            .map(|(n, r, f)| serde_json::json!({"name": n, "references": r, "files": f}))
            .collect();
        return print_json(&serde_json::json!({
            "target": shown,
            "exports": exports,
            "references": refs,
            "summary": {"references": refs.len(), "files": files, "tests": tests, "by_kind": by_kind},
            "unresolved": unresolved,
            "coverage": COVERAGE,
        }));
    }
    println!("{}", format!("Impact of module {shown}").bold());
    let listed: Vec<String> = exports
        .iter()
        .map(|(n, r, f)| if *r == 0 { format!("{n} (unused)") } else { format!("{n} ({r} in {f} files)") })
        .collect();
    println!("  exports: {}", if listed.is_empty() { "none".to_string() } else { listed.join(", ") });
    let kinds: Vec<String> = by_kind.iter().map(|(k, n)| format!("{k} {n}")).collect();
    println!("  {} references in {files} files: {} (tests: {tests})", refs.len(), kinds.join(", "));
    print_references(&refs, full);
    print_unresolved(unresolved);
    println!("{} {COVERAGE}", "coverage:".dimmed());
    Ok(())
}

/// `impact <name>` with a short name: answered for the JS/TS export when no PHP class has that name and one
/// module exports it; several exporters are listed. `None` — not a JS export, let PHP answer.
pub fn short_name_impact(root: &Path, name: &str, full: bool, format: &str) -> Option<Result<()>> {
    if name.contains('\\') || is_js_target(name) {
        return None;
    }
    let php_class: bool = open(root)
        .ok()?
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM symbols WHERE name = ?1 COLLATE NOCASE AND qualified_name IS NOT NULL)",
            params![name],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if php_class {
        return None;
    }
    match exporters(root, name).as_slice() {
        [] => None,
        [one] => Some(cmd_js_impact(root, one, full, format)),
        many => {
            if format == "json" {
                return Some(print_json(&serde_json::json!({"did_you_mean": many})));
            }
            println!("{} JS/TS modules export {name}; pass one:", many.len());
            for m in many {
                println!("  ast-index impact '{m}'");
            }
            Some(Ok(()))
        }
    }
}

/// After short-name `usages`: the JS/TS modules that export the name, for a module-precise answer.
pub fn print_exporters_notice(root: &Path, name: &str) {
    let found = exporters(root, name);
    if !found.is_empty() {
        let hints: Vec<String> = found.iter().take(5).map(|m| format!("usages '{m}'")).collect();
        println!(
            "{} these are matches by short name; through imports of one module: {}",
            "note:".yellow(),
            hints.join(", ")
        );
    }
}

/// JS/TS modules exporting `name`, as `path#name` — for a short name given to `impact` or `usages`.
pub fn exporters(root: &Path, name: &str) -> Vec<String> {
    let Ok(conn) = open(root) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT f.path FROM js_exports e JOIN files f ON f.id = e.file_id
         WHERE e.name = ?1 AND e.kind <> 'reexport' ORDER BY f.path",
    ) else {
        return Vec::new();
    };
    stmt.query_map(params![name], |r| r.get::<_, String>(0))
        .map(|rows| rows.filter_map(|r| r.ok()).map(|p| format!("{p}#{name}")).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// move-plan
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Edit {
    path: String,
    line: i64,
    kind: String,
    from: String,
    to: String,
}

impl Project {
    /// `spec` rewritten to reach `target` from `importer_dir`, in the style it was written: relative stays
    /// relative, an alias stays an alias when one covers the target, extension and `index` kept as they were.
    fn respell(&self, resolver: &Resolver, importer_dir: &Path, spec: &str, old_target: &Path, target: &Path) -> String {
        let (bare, query) = match spec.split_once('?') {
            Some((b, q)) => (b, format!("?{q}")),
            None => (spec, String::new()),
        };
        let last = bare.rsplit('/').next().unwrap_or(bare);
        let had_extension = Path::new(last).extension().is_some();
        let old_is_index = old_target.file_stem().is_some_and(|s| s == "index") && !last.starts_with("index");
        let new_is_index = target.file_stem().is_some_and(|s| s == "index");
        let shape = |path: &Path| -> PathBuf {
            if old_is_index && new_is_index {
                path.parent().unwrap_or(path).to_path_buf()
            } else if !had_extension && has_js_extension(&path.to_string_lossy()) {
                path.with_extension("")
            } else {
                path.to_path_buf()
            }
        };
        let relative_spell = || format!("{}{query}", relative(importer_dir, &shape(target)));
        if bare.starts_with('.') {
            return relative_spell();
        }
        // alias: prefer the one the specifier used, then any covering the target
        let mut options: Vec<(String, PathBuf)> = resolver.aliases.clone();
        if let Some(config) = resolver.config_for(importer_dir) {
            for (pattern, targets) in &config.paths {
                if let Some((prefix, "")) = pattern.split_once('*') {
                    for t in targets {
                        if let Some((dir, "")) = t.split_once('*') {
                            options.push((prefix.to_string(), normalize(&config.base.join(dir))));
                        }
                    }
                }
            }
        }
        options.sort_by_key(|(prefix, _)| !bare.starts_with(prefix.as_str()));
        for (prefix, dir) in options {
            if let Ok(rest) = shape(target).strip_prefix(&dir) {
                return format!("{prefix}{}{query}", rest.to_string_lossy());
            }
        }
        relative_spell()
    }
}

pub fn cmd_js_move_plan(root: &Path, from: &str, to: &str, format: &str) -> Result<()> {
    let _lease = db::acquire_project_lease(root)?;
    let conn = open(root)?;
    let project = Project::load(&conn, root)?;
    let module = project.module_arg(from)?;
    let old_abs = project.set.modules[module].abs.clone();
    let mut new_abs = normalize(&root.join(to));
    if to.ends_with('/') || new_abs.is_dir() {
        new_abs = new_abs.join(old_abs.file_name().unwrap_or_default());
    }
    let new_shown = new_abs.strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| to.to_string());
    let resolver = Resolver::new(root);
    let mut warnings = Vec::new();
    if new_abs.exists() {
        warnings.push(format!("{new_shown} already exists"));
    }
    let old_dir = old_abs.parent().unwrap_or(root).to_path_buf();
    let new_dir = new_abs.parent().unwrap_or(root).to_path_buf();
    if resolver.config_for(&old_dir).map(|c| c.base.clone()) != resolver.config_for(&new_dir).map(|c| c.base.clone()) {
        warnings.push("the file moves under another tsconfig/jsconfig: check its alias imports".to_string());
    }

    let mut edits = Vec::new();
    let mut glob_checks = Vec::new();
    for (i, imp) in project.imports.iter().enumerate() {
        if !project.points_to(i, module) || imp.file == module {
            continue;
        }
        let importer = &project.set.modules[imp.file];
        if imp.kind == "glob" {
            glob_checks.push(Edit {
                path: importer.shown.clone(),
                line: imp.line,
                kind: imp.kind.clone(),
                from: imp.spec.clone(),
                to: String::new(),
            });
            continue;
        }
        let dir = importer.abs.parent().unwrap_or(root);
        let spelled = project.respell(&resolver, dir, &imp.spec, &old_abs, &new_abs);
        if spelled != imp.spec {
            edits.push(Edit {
                path: importer.shown.clone(),
                line: imp.line,
                kind: imp.kind.clone(),
                from: imp.spec.clone(),
                to: spelled,
            });
        }
    }
    edits.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    edits.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.from == b.from);

    // the moved file's own specifiers
    let mut own = Vec::new();
    for (i, imp) in project.imports.iter().enumerate() {
        if imp.file != module || imp.kind == "glob" {
            continue;
        }
        let target_abs = match &project.targets[i] {
            Target::Module(m) if *m == module => new_abs.clone(),
            Target::Module(m) => project.set.modules[*m].abs.clone(),
            Target::Asset => normalize(&old_dir.join(imp.spec.split('?').next().unwrap_or(&imp.spec))),
            _ => continue,
        };
        if !imp.spec.starts_with('.') && !matches!(project.targets[i], Target::Module(_)) {
            continue;
        }
        let spelled = if imp.spec.starts_with('.') {
            let query = imp.spec.split_once('?').map(|(_, q)| format!("?{q}")).unwrap_or_default();
            let last = imp.spec.split('?').next().unwrap_or(&imp.spec).rsplit('/').next().unwrap_or("");
            let target_shape = if matches!(project.targets[i], Target::Asset) || Path::new(last).extension().is_some() {
                target_abs.clone()
            } else if target_abs.file_stem().is_some_and(|s| s == "index") && !last.starts_with("index") {
                target_abs.parent().unwrap_or(&target_abs).to_path_buf()
            } else {
                target_abs.with_extension("")
            };
            format!("{}{query}", relative(&new_dir, &target_shape))
        } else {
            project.respell(&resolver, &new_dir, &imp.spec, &target_abs, &target_abs)
        };
        if spelled != imp.spec {
            own.push(Edit {
                path: new_shown.clone(),
                line: imp.line,
                kind: imp.kind.clone(),
                from: imp.spec.clone(),
                to: spelled,
            });
        }
    }
    let unresolved = project.unresolved_near(module);

    if format == "json" {
        let unresolved: Vec<serde_json::Value> = unresolved
            .iter()
            .map(|(f, l, s)| serde_json::json!({"path": project.set.modules[*f].shown, "line": l, "spec": s}))
            .collect();
        return print_json(&serde_json::json!({
            "from": project.set.modules[module].shown,
            "to": new_shown,
            "edits": edits,
            "own_imports": own,
            "globs_to_check": glob_checks,
            "unresolved": unresolved,
            "warnings": warnings,
        }));
    }
    println!("{}", format!("Move {} → {new_shown}", project.set.modules[module].shown).bold());
    for w in &warnings {
        println!("  {} {w}", "warning:".yellow());
    }
    let files: HashSet<&str> = edits.iter().map(|e| e.path.as_str()).collect();
    println!("  {} specifiers to rewrite in {} files; own imports to rewrite: {}", edits.len(), files.len(), own.len());
    let mut current = "";
    for e in &edits {
        if e.path != current {
            current = &e.path;
            println!("{}", e.path.cyan());
        }
        println!("    {:>5} {:<8} '{}' → '{}'", e.line, e.kind, e.from, e.to);
    }
    if !own.is_empty() {
        println!("{} {}", "own imports of".bold(), new_shown.cyan());
        for e in &own {
            println!("    {:>5} {:<8} '{}' → '{}'", e.line, e.kind, e.from, e.to);
        }
    }
    if !glob_checks.is_empty() {
        println!("{}", "glob patterns that match the file now — check they still do:".bold());
        for e in &glob_checks {
            println!("    {}:{} '{}'", e.path, e.line, e.from);
        }
    }
    let unresolved: Vec<serde_json::Value> = unresolved
        .iter()
        .map(|(f, l, s)| serde_json::json!({"path": project.set.modules[*f].shown, "line": l, "spec": s}))
        .collect();
    print_unresolved(&unresolved);
    Ok(())
}

// ---------------------------------------------------------------------------
// unused exports
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
pub struct UnusedExport {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: i64,
    /// Used inside its own file: drop `export` rather than the code.
    pub used_locally: bool,
    /// Imported by tests only.
    pub tests_only: bool,
}

/// What `unused_exports` counts as a use — printed with its answer, so it is not re-checked by hand.
pub const UNUSED_COVERAGE: &str = "an export is used when a module imports it — static imports (tests \
included), re-exports, `ns.name` of namespace imports, import()/require() by the names taken from them, JSDoc \
import() types; a module object passed on whole, import() without names, export * and import.meta.glob take the \
whole module; mocks do not count; resolution as in `impact`";

/// Exports under `prefix` that no module imports, and modules under it nothing references (entry points or
/// dead files). `skip` filters files and names the framework uses by convention.
pub fn unused_exports(
    conn: &Connection,
    root: &Path,
    prefix: Option<&str>,
    skip: &dyn Fn(&str, &str) -> bool,
) -> Result<(Vec<UnusedExport>, Vec<String>)> {
    let has_tables: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'js_exports')", [], |r| r.get(0))
        .unwrap_or(false);
    if !has_tables {
        return Ok((vec![], vec![]));
    }
    let project = match Project::load(conn, root) {
        Ok(p) => p,
        Err(_) => return Ok((vec![], vec![])),
    };
    let mut used: HashSet<(usize, String)> = HashSet::new();
    let mut used_outside_tests: HashSet<(usize, String)> = HashSet::new();
    let mut members_stmt = conn.prepare("SELECT member FROM js_uses WHERE file_id = ?1 AND local = ?2")?;
    let mut referenced: HashSet<usize> = HashSet::new();
    let mut whole: HashSet<usize> = HashSet::new();
    for (i, imp) in project.imports.iter().enumerate() {
        let targets: Vec<usize> = match &project.targets[i] {
            Target::Module(m) => vec![*m],
            Target::Modules(ms) => ms.clone(),
            _ => continue,
        };
        for t in targets {
            if t == imp.file {
                continue;
            }
            if imp.kind != "mock" {
                referenced.insert(t);
            }
            let names: Vec<String> = match (imp.kind.as_str(), imp.imported.as_deref(), imp.local.as_deref()) {
                ("mock" | "side", _, _) => continue,
                ("glob", _, _) | ("dynamic" | "require", None, _) => {
                    whole.insert(t);
                    continue;
                }
                // a module object bound to a name: the members read from it, unless it is passed on whole
                ("import" | "require" | "dynamic", Some("*"), Some(local)) => {
                    let members: Vec<Option<String>> = members_stmt
                        .query_map(params![project.set.modules[imp.file].id, local], |r| r.get(0))?
                        .collect::<Result<_, _>>()?;
                    if members.iter().any(Option::is_none) {
                        whole.insert(t);
                        continue;
                    }
                    members.into_iter().flatten().collect()
                }
                (_, Some("*"), _) => {
                    whole.insert(t);
                    continue;
                }
                (_, Some(name), _) => vec![name.to_string()],
                _ => continue,
            };
            let from_tests = is_test_path(&project.set.modules[imp.file].shown);
            for name in names {
                if !from_tests {
                    used_outside_tests.insert((t, name.clone()));
                }
                used.insert((t, name));
            }
        }
    }
    let mut unused = Vec::new();
    let mut unreferenced = Vec::new();
    for (index, module) in project.set.modules.iter().enumerate() {
        if let Some(prefix) = prefix {
            if !module.shown.starts_with(prefix) {
                continue;
            }
        }
        // test files and tool configs are started by their runners, not imported
        if skip(&module.shown, "") || whole.contains(&index) || is_test_path(&module.shown) || is_tool_config(&module.shown)
        {
            continue;
        }
        if !referenced.contains(&index) {
            unreferenced.push(module.shown.clone());
            continue;
        }
        let mut seen = HashSet::new();
        for e in project.exports_of(index) {
            let key = (index, e.name.clone());
            let tests_only = used.contains(&key) && !used_outside_tests.contains(&key);
            if (used.contains(&key) && !tests_only) || !seen.insert(e.name.clone()) || skip(&module.shown, &e.name) {
                continue;
            }
            let local = e.local.clone().unwrap_or_else(|| e.name.clone());
            let own_lines: HashSet<i64> = [Some(e.line), e.decl_line].into_iter().flatten().collect();
            let used_locally = local != "default"
                && word_lines(&project.file_lines(index), &local).iter().any(|l| !own_lines.contains(l));
            unused.push(UnusedExport {
                name: e.name.clone(),
                kind: e.kind.clone(),
                path: module.shown.clone(),
                line: e.line,
                used_locally,
                tests_only,
            });
        }
    }
    Ok((unused, unreferenced))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_between_directories() {
        assert_eq!(relative(Path::new("/p/src/a"), Path::new("/p/src/a/b.js")), "./b.js");
        assert_eq!(relative(Path::new("/p/src/a"), Path::new("/p/src/c/d")), "../c/d");
        assert_eq!(relative(Path::new("/p/tests"), Path::new("/p/src/x.vue")), "../src/x.vue");
    }

    #[test]
    fn jsonc_with_comments_and_trailing_commas() {
        let json = parse_jsonc("{\n // c\n \"a\": \"x//y\", /* b */\n \"p\": {\"@/*\": [\"src/*\",],},\n \"ru\": \"строка\",\n}").unwrap();
        assert_eq!(json["a"], "x//y");
        assert_eq!(json["p"]["@/*"][0], "src/*");
        assert_eq!(json["ru"], "строка");
    }

    #[test]
    fn glob_patterns() {
        let re = glob_regex("blocks/*.{vue,js}").unwrap();
        assert!(re.is_match("blocks/A.vue"));
        assert!(re.is_match("blocks/b.js"));
        assert!(!re.is_match("blocks/deep/A.vue"));
        assert!(glob_regex("**/*.vue").unwrap().is_match("a/b/C.vue"));
    }

    #[test]
    fn js_targets_are_told_from_php_names() {
        assert!(is_js_target("src/a.js#foo"));
        assert!(is_js_target("src/ui/Popup.vue"));
        assert!(is_js_target("@/ui/x"));
        assert!(!is_js_target("App\\Order\\OrderDto"));
        assert!(!is_js_target("OrderRepository::find"));
        assert!(!is_js_target("OrderDto"));
    }
}
