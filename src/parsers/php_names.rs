//! PHP name resolution: fully qualified names and kinds for class references and definitions.
//!
//! The generic extractor keeps only the short name of a reference, so five different `OrderDto`
//! classes looked like one, and a dead class with a live namesake looked used. PHP resolves a
//! class name against the file's `namespace` and `use` imports; this module does the same in one
//! pass over the file, tracking what each character is — code, a string, a heredoc, a comment,
//! a PHPDoc block or inline HTML — so words in SQL strings, comments and templates are not
//! mistaken for classes. Each class reference gets its FQN and the kind of use (`import`,
//! `trait-use`, `new`, `static`, `::class`, `type`, `phpdoc`, `string`, `attribute`,
//! `inheritance`, `check`); class-like definitions get their FQN.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::{ParsedRef, ParsedSymbol};
use crate::db::SymbolKind;

/// Resolution of one PHP file.
pub struct PhpNames {
    /// Definitions, keyed like `ParsedFile::qualified_names`: (kind, line, name) → FQN.
    pub qualified: HashMap<(String, usize, String), String>,
    /// One entry per reference: (FQN, kind), `None` when the reference is not a class name.
    pub refs: Vec<Option<(String, &'static str)>>,
}

pub fn resolve(content: &str, symbols: &[ParsedSymbol], refs: &[ParsedRef]) -> PhpNames {
    let lines: Vec<&str> = content.lines().collect();
    // The parser already knows where each namespace starts, `<?php declare(…); namespace X;` too.
    let namespace_lines: HashMap<usize, String> = symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Package)
        .map(|s| {
            (
                s.line.wrapping_sub(1),
                s.name.trim_start_matches('\\').to_string(),
            )
        })
        .collect();
    let analysis = analyze(&lines, &local_type_names(content), &namespace_lines);

    let ref_names = refs
        .iter()
        .map(|r| {
            analysis
                .names
                .get(r.line.wrapping_sub(1))
                .and_then(|names| names.get(&r.name))
                .cloned()
        })
        .collect();

    let qualified = symbols
        .iter()
        .filter(|s| is_class_like(&s.kind))
        .map(|s| {
            let namespace = analysis
                .namespaces
                .get(s.line.wrapping_sub(1))
                .map(String::as_str)
                .unwrap_or("");
            (
                (s.kind.as_str().to_string(), s.line, s.name.clone()),
                qualify(namespace, &s.name),
            )
        })
        .collect();

    PhpNames {
        qualified,
        refs: ref_names,
    }
}

fn is_class_like(kind: &SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Trait
            | SymbolKind::Enum
            | SymbolKind::Object
    )
}

fn qualify(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        name.to_string()
    } else {
        format!("{namespace}\\{name}")
    }
}

// ---------------------------------------------------------------------------
// One pass over the file
// ---------------------------------------------------------------------------

/// What the lexer is inside of at a given point.
#[derive(Clone, PartialEq, Debug)]
enum Mode {
    /// Text outside `<?php … ?>`.
    Html,
    Code,
    Str(char),
    /// Heredoc/nowdoc body until the closing identifier.
    Heredoc(String),
    Comment {
        doc: bool,
    },
}

/// What the lexer found on a line.
enum Event {
    /// A name in code (`doc == false`) or in a PHPDoc block, at char positions `start..end`.
    Name { start: usize, end: usize, doc: bool },
    /// A fully qualified name inside a string literal or heredoc (escapes already applied).
    StringFqn(String),
    /// A bracket in code: `(`, `)`, `[`, `]`, `{`.
    Bracket { at: usize, c: char },
}

/// A clause in which names have a known kind: an attribute `#[…]`, an `extends`/`implements`
/// list up to the class body, a `catch (…)`. It may span lines.
#[derive(Clone, Copy)]
struct Clause {
    kind: &'static str,
    /// Open `[` (attribute) or `(` (catch) inside the clause.
    depth: i32,
}

/// Update the open clause for a bracket in code; `prev` is the char before it.
fn track_clause(clause: &mut Option<Clause>, c: char, prev: Option<char>) {
    let Some(open) = clause.as_mut() else {
        if c == '[' && prev == Some('#') {
            *clause = Some(Clause {
                kind: "attribute",
                depth: 1,
            });
        }
        return;
    };
    // An `extends`/`implements` list ends at the class body.
    let (opener, closer) = match open.kind {
        "attribute" => ('[', ']'),
        "check" => ('(', ')'),
        _ => ('{', '{'),
    };
    if c == closer {
        open.depth -= 1;
        if open.depth <= 0 {
            *clause = None;
        }
    } else if c == opener {
        open.depth += 1;
    }
}

struct Analysis {
    /// Per line (0-based): short name → (FQN, kind), first class-like occurrence wins.
    names: Vec<HashMap<String, (String, &'static str)>>,
    /// Namespace in effect on each line (0-based).
    namespaces: Vec<String>,
}

#[derive(Default, Clone)]
struct Scope {
    namespace: String,
    /// Lower-cased alias → FQN.
    imports: HashMap<String, String>,
}

/// Lines longer than this are generated or minified; the generic extractor skips them too.
const MAX_LINE_LEN: usize = 2000;

fn analyze(
    lines: &[&str],
    local_types: &HashSet<String>,
    namespace_lines: &HashMap<usize, String>,
) -> Analysis {
    let mut names = vec![HashMap::new(); lines.len()];
    let mut namespaces = vec![String::new(); lines.len()];
    let mut scope = Scope::default();
    let mut mode = Mode::Html;
    let mut depth = 0usize;
    // Imports live at the namespace's own brace depth; `use Trait;` sits deeper, in a class.
    let mut import_depth = 0usize;
    // A `use` statement can span lines (group use): its text and line indexes so far.
    let mut pending_use: Option<(String, Vec<usize>)> = None;
    let mut clause: Option<Clause> = None;

    for (idx, line) in lines.iter().enumerate() {
        if let Some(namespace) = namespace_lines.get(&idx) {
            let braced = line
                .split_once("namespace")
                .is_some_and(|(_, rest)| rest.contains('{'))
                || lines
                    .get(idx + 1)
                    .is_some_and(|next| next.trim_start().starts_with('{'));
            import_depth = if braced { depth + 1 } else { depth };
            scope = Scope {
                namespace: namespace.clone(),
                imports: HashMap::new(),
            };
        }
        namespaces[idx] = scope.namespace.clone();

        let trimmed = line.trim_start();
        // A statement can follow the opening tag on the same line: `<?php use App\Foo;`.
        let statement = match (&mode, trimmed.strip_prefix("<?php")) {
            (Mode::Html, Some(rest)) => Some(rest.trim_start()),
            (Mode::Code, _) => Some(trimmed),
            _ => None,
        };
        let use_rest = statement.and_then(|s| keyword_rest(s, "use"));
        let import_line = if let Some((mut text, mut idxs)) = pending_use.take() {
            text.push(' ');
            text.push_str(line);
            idxs.push(idx);
            pending_use = Some((text, idxs));
            true
        } else if let Some(rest) =
            use_rest.filter(|r| depth == import_depth && !r.trim_start().starts_with('('))
        {
            pending_use = Some((rest.to_string(), vec![idx]));
            true
        } else {
            false
        };
        if pending_use
            .as_ref()
            .is_some_and(|(text, _)| text.contains(';'))
        {
            let (text, idxs) = pending_use.take().expect("checked above");
            finish_use(&text, &idxs, &mut scope, &mut names);
        }

        let resolve_names = !import_line && line.len() <= MAX_LINE_LEN;
        let chars: Vec<char> = if resolve_names {
            line.chars().collect()
        } else {
            Vec::new()
        };
        let site_line = SiteLine {
            chars: &chars,
            trait_use: use_rest.is_some() && !import_line,
        };
        let line_names = &mut names[idx];
        lex_line(line, &mut mode, &mut depth, |event| {
            if !resolve_names {
                return;
            }
            match event {
                Event::Bracket { at, c } => {
                    track_clause(&mut clause, c, at.checked_sub(1).map(|p| chars[p]))
                }
                Event::StringFqn(fqn) => {
                    let last = fqn.rsplit('\\').next().unwrap_or(&fqn).to_string();
                    line_names.entry(last).or_insert((fqn, "string"));
                }
                Event::Name { start, end, doc } => {
                    let token: String = chars[start..end].iter().collect();
                    if !doc && clause.is_none() {
                        let opens = match token.to_ascii_lowercase().as_str() {
                            "extends" | "implements" => Some("inheritance"),
                            "catch" => Some("check"),
                            _ => None,
                        };
                        if let Some(kind) = opens {
                            clause = Some(Clause { kind, depth: 0 });
                            return;
                        }
                    }
                    let last = token.rsplit('\\').next().unwrap_or(&token).to_string();
                    if last.is_empty()
                        || line_names.contains_key(&last)
                        || local_types.contains(&token)
                    {
                        return;
                    }
                    let site = Site {
                        start,
                        end,
                        doc,
                        clause: clause.map(|c| c.kind),
                    };
                    if let Some(resolved) = classify(&site_line, &site, &token, &scope) {
                        line_names.insert(last, resolved);
                    }
                }
            }
        });
    }

    Analysis { names, namespaces }
}

/// Record a complete `use` statement: update imports and name every import on its lines.
fn finish_use(
    text: &str,
    idxs: &[usize],
    scope: &mut Scope,
    names: &mut [HashMap<String, (String, &'static str)>],
) {
    let mut statement = HashMap::new();
    parse_use(text, &mut statement, &mut scope.imports);
    for &i in idxs {
        for (name, fqn) in &statement {
            names[i].insert(name.clone(), (fqn.clone(), "import"));
        }
    }
}

/// `rest` after `keyword` followed by whitespace, if the line starts with that keyword.
fn keyword_rest<'a>(trimmed: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = trimmed.strip_prefix(keyword)?;
    rest.starts_with(|c: char| c.is_whitespace() || c == '\\' || c == '{')
        .then_some(rest)
}

/// `use A\B, C as D;`, `use A\{B, C as D};` — function/const imports are skipped. Fills
/// `statement` with every name that appears on the statement's lines (last segments and
/// aliases) and `imports` with alias → FQN.
fn parse_use(
    text: &str,
    statement: &mut HashMap<String, String>,
    imports: &mut HashMap<String, String>,
) {
    let body = text.split(';').next().unwrap_or("").trim();
    if body.starts_with("function ") || body.starts_with("const ") {
        return;
    }
    let mut add = |item: &str, prefix: &str| {
        let item = item.trim();
        if item.is_empty() || item.starts_with("function ") || item.starts_with("const ") {
            return;
        }
        let (name, alias) = match item.split_once(" as ") {
            Some((n, a)) => (n.trim(), Some(a.trim())),
            None => (item, None),
        };
        let fqn = format!("{prefix}{}", name.trim_start_matches('\\'));
        let fqn = fqn.trim_start_matches('\\').to_string();
        let last = fqn.rsplit('\\').next().unwrap_or(&fqn).to_string();
        statement.insert(last.clone(), fqn.clone());
        let alias = alias.map(str::to_string).unwrap_or(last);
        statement.insert(alias.clone(), fqn.clone());
        imports.insert(alias.to_ascii_lowercase(), fqn);
    };

    if let Some((prefix, group)) = body.split_once('{') {
        let prefix = prefix.trim().trim_start_matches('\\');
        let group = group.split('}').next().unwrap_or("");
        for item in group.split(',') {
            add(item, prefix);
        }
    } else {
        for item in body.split(',') {
            add(item, "");
        }
    }
}

static FQN_IN_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\\?[A-Za-z_][A-Za-z0-9_]*(?:\\[A-Za-z_][A-Za-z0-9_]*)+").unwrap()
});

/// FQNs in string text; `\\` (one escaped backslash) counts as a separator.
fn string_fqns(text: &str, on_event: &mut impl FnMut(Event)) {
    let unescaped = text.replace("\\\\", "\\");
    for m in FQN_IN_TEXT.find_iter(&unescaped) {
        on_event(Event::StringFqn(
            m.as_str().trim_start_matches('\\').to_string(),
        ));
    }
}

/// Walk one line: track the lexer mode and brace depth, report names in code and PHPDoc blocks
/// and FQNs in strings/heredocs. Plain comments and inline HTML report nothing; `$variables` are
/// skipped.
fn lex_line(line: &str, mode: &mut Mode, depth: &mut usize, mut on_event: impl FnMut(Event)) {
    let chars: Vec<char> = line.chars().collect();
    let at = |i: usize| chars.get(i).copied();
    let starts = |i: usize, s: &str| s.chars().enumerate().all(|(k, c)| at(i + k) == Some(c));
    let mut i = 0;

    if let Mode::Heredoc(id) = mode.clone() {
        let indent = chars.iter().take_while(|c| c.is_whitespace()).count();
        let closes = starts(indent, &id)
            && !at(indent + id.len()).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if !closes {
            string_fqns(line, &mut on_event);
            return;
        }
        *mode = Mode::Code;
        i = indent + id.len();
    }

    while i < chars.len() {
        let c = chars[i];
        match mode.clone() {
            Mode::Html => {
                let php_tag = chars
                    .get(i..i + 5)
                    .is_some_and(|t| t.iter().collect::<String>().eq_ignore_ascii_case("<?php"));
                let xml_tag = chars
                    .get(i..i + 5)
                    .is_some_and(|t| t.iter().collect::<String>().eq_ignore_ascii_case("<?xml"));
                if php_tag {
                    *mode = Mode::Code;
                    i += 5;
                } else if starts(i, "<?=") {
                    *mode = Mode::Code;
                    i += 3;
                } else if starts(i, "<?") && !xml_tag {
                    // Short open tag, with or without a space: `<?if (…):`, `<?$APPLICATION->…`.
                    *mode = Mode::Code;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Mode::Comment { doc } => {
                if starts(i, "*/") {
                    *mode = Mode::Code;
                    i += 2;
                } else if doc && is_token_start(&chars, i) {
                    let end = token_end(&chars, i);
                    on_event(Event::Name {
                        start: i,
                        end,
                        doc: true,
                    });
                    i = end;
                } else {
                    i += 1;
                }
            }
            Mode::Str(q) => {
                // Scan to the closing quote on this line, honouring escapes.
                let from = i;
                let mut j = i;
                let mut closed = false;
                while j < chars.len() {
                    if chars[j] == '\\' {
                        j += 2;
                        continue;
                    }
                    if chars[j] == q {
                        closed = true;
                        break;
                    }
                    j += 1;
                }
                let end = j.min(chars.len());
                let text: String = chars[from..end].iter().collect();
                string_fqns(&text, &mut on_event);
                if closed {
                    *mode = Mode::Code;
                    i = end + 1;
                } else {
                    return;
                }
            }
            Mode::Heredoc(_) => return,
            Mode::Code => {
                if starts(i, "?>") {
                    *mode = Mode::Html;
                    i += 2;
                    continue;
                }
                // A one-line comment ends at the line end or at `?>`, which closes PHP even there.
                if starts(i, "//") || (c == '#' && at(i + 1) != Some('[')) {
                    match (i..chars.len()).find(|&k| starts(k, "?>")) {
                        Some(k) => {
                            *mode = Mode::Html;
                            i = k + 2;
                            continue;
                        }
                        None => return,
                    }
                }
                if starts(i, "/*") {
                    let doc = at(i + 2) == Some('*') && at(i + 3) != Some('/');
                    *mode = Mode::Comment { doc };
                    i += 2;
                    continue;
                }
                if starts(i, "<<<") {
                    let mut j = i + 3;
                    while at(j).is_some_and(|c| c == ' ' || c == '\'' || c == '"') {
                        j += 1;
                    }
                    let id: String = chars[j..]
                        .iter()
                        .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
                        .collect();
                    if !id.is_empty() {
                        *mode = Mode::Heredoc(id);
                        return;
                    }
                }
                match c {
                    '\'' | '"' => {
                        *mode = Mode::Str(c);
                        i += 1;
                    }
                    '{' => {
                        *depth += 1;
                        on_event(Event::Bracket { at: i, c });
                        i += 1;
                    }
                    '(' | ')' | '[' | ']' => {
                        on_event(Event::Bracket { at: i, c });
                        i += 1;
                    }
                    '}' => {
                        *depth = depth.saturating_sub(1);
                        i += 1;
                    }
                    '$' => {
                        i += 1;
                        while at(i).is_some_and(is_ident_char) {
                            i += 1;
                        }
                    }
                    _ if is_token_start(&chars, i) => {
                        let end = token_end(&chars, i);
                        on_event(Event::Name {
                            start: i,
                            end,
                            doc: false,
                        });
                        i = end;
                    }
                    _ => i += 1,
                }
            }
        }
    }
}

/// A name starts at `\Name` or `Name`, not inside another identifier, a `$var`, or right after
/// a `\` that belongs to an earlier token.
fn is_token_start(chars: &[char], i: usize) -> bool {
    let c = chars[i];
    let begins =
        (c == '\\' && chars.get(i + 1).is_some_and(|n| is_ident_start(*n))) || is_ident_start(c);
    begins
        && !(i > 0 && (is_ident_char(chars[i - 1]) || chars[i - 1] == '\\' || chars[i - 1] == '$'))
}

/// End of a (possibly qualified) name.
fn token_end(chars: &[char], i: usize) -> usize {
    let mut j = i;
    if chars.get(j) == Some(&'\\') {
        j += 1;
    }
    loop {
        while j < chars.len() && is_ident_char(chars[j]) {
            j += 1;
        }
        if chars.get(j) == Some(&'\\') && chars.get(j + 1).is_some_and(|c| is_ident_start(*c)) {
            j += 1;
        } else {
            return j;
        }
    }
}

// ---------------------------------------------------------------------------
// Deciding what a name in code is
// ---------------------------------------------------------------------------

/// What `classify` needs to know about the line a name stands on.
struct SiteLine<'a> {
    chars: &'a [char],
    /// `use Trait;` inside a class.
    trait_use: bool,
}

/// A name on the line: char positions, whether it is in a PHPDoc block, and the clause it is in.
struct Site {
    start: usize,
    end: usize,
    doc: bool,
    clause: Option<&'static str>,
}

fn classify(
    line: &SiteLine,
    site: &Site,
    token: &str,
    scope: &Scope,
) -> Option<(String, &'static str)> {
    let chars = line.chars;
    let (start, end) = (site.start, site.end);
    let mut before_end = start;
    while before_end > 0 && chars[before_end - 1].is_whitespace() {
        before_end -= 1;
    }
    let ends_with = |s: &str| {
        let n = s.chars().count();
        before_end >= n
            && chars[before_end - n..before_end]
                .iter()
                .copied()
                .eq(s.chars())
    };
    if ends_with("->") || ends_with("::") {
        return None;
    }
    let prev_word = previous_word(chars, start);
    let after = |k: usize| chars.get(end + k).copied();
    let after_is_scope = after(0) == Some(':') && after(1) == Some(':');
    // Declared names (functions, constants, enum cases, the class itself) are not references;
    // `case Status::ACTIVE:` in a switch is.
    let declares = matches!(
        prev_word.as_str(),
        "function"
            | "fn"
            | "const"
            | "goto"
            | "namespace"
            | "class"
            | "interface"
            | "trait"
            | "enum"
    ) || (prev_word == "case" && !after_is_scope);
    // `name: value` — a named argument or a label, not a type.
    if declares || (after(0) == Some(':') && !after_is_scope) {
        return None;
    }
    let in_attribute = site.clause == Some("attribute");
    let next = chars[end..].iter().find(|c| !c.is_whitespace()).copied();
    if next == Some('(') && prev_word != "new" && !after_is_scope && !in_attribute {
        return None;
    }
    let lower = token.to_ascii_lowercase();
    // Keywords are case-insensitive: `Throw New Exception()` is not a class `Throw`.
    if RESERVED.contains(lower.as_str()) {
        return None;
    }

    let class_constant = after_is_scope
        && chars
            .get(end + 2..end + 7)
            .is_some_and(|t| t.iter().copied().eq("class".chars()))
        && after(7).is_none_or(|c| !is_ident_char(c));
    let kind = if site.doc {
        "phpdoc"
    } else if line.trait_use {
        "trait-use"
    } else if class_constant {
        "::class"
    } else if in_attribute && !after_is_scope {
        "attribute"
    } else if after_is_scope {
        "static"
    } else if prev_word == "new" {
        "new"
    } else if site.clause == Some("inheritance") {
        "inheritance"
    } else if prev_word == "instanceof" || site.clause == Some("check") {
        "check"
    } else {
        "type"
    };

    // Where only a class can stand, any spelling is a class. Elsewhere lower-case names are
    // functions and keywords, ALL-CAPS names (and a lone capital) constants, template parameters
    // or prose (`mode Y`) — unless the file imports that name.
    let certain = matches!(
        kind,
        "trait-use" | "::class" | "static" | "new" | "inheritance" | "check"
    );
    let last = token.rsplit('\\').next().unwrap_or(token);
    let unqualified = !token.contains('\\');
    let imported = unqualified && scope.imports.contains_key(&lower);
    let all_caps = last
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    let lowercase = last.starts_with(|c: char| c.is_ascii_lowercase());
    if !certain && !imported && (lowercase || (unqualified && all_caps)) {
        return None;
    }

    let fqn = if let Some(stripped) = token.strip_prefix('\\') {
        stripped.to_string()
    } else if let Some((first, rest)) = token.split_once('\\') {
        if first.eq_ignore_ascii_case("namespace") {
            qualify(&scope.namespace, rest)
        } else {
            match scope.imports.get(&first.to_ascii_lowercase()) {
                Some(prefix) => format!("{prefix}\\{rest}"),
                None => qualify(&scope.namespace, token),
            }
        }
    } else {
        scope
            .imports
            .get(&lower)
            .cloned()
            .unwrap_or_else(|| qualify(&scope.namespace, token))
    };
    Some((fqn, kind))
}

/// Reserved words, lower-cased: PHP forbids them as class names, so no reference to a class or
/// method can be spelled like one (in any letter case).
const PHP_KEYWORDS: &[&str] = &[
    "abstract",
    "and",
    "array",
    "as",
    "break",
    "callable",
    "case",
    "catch",
    "class",
    "clone",
    "const",
    "continue",
    "declare",
    "default",
    "die",
    "do",
    "echo",
    "else",
    "elseif",
    "empty",
    "enddeclare",
    "endfor",
    "endforeach",
    "endif",
    "endswitch",
    "endwhile",
    "enum",
    "eval",
    "exit",
    "extends",
    "false",
    "final",
    "finally",
    "fn",
    "for",
    "foreach",
    "function",
    "global",
    "goto",
    "if",
    "implements",
    "include",
    "include_once",
    "instanceof",
    "insteadof",
    "interface",
    "isset",
    "list",
    "match",
    "namespace",
    "new",
    "null",
    "or",
    "parent",
    "print",
    "private",
    "protected",
    "public",
    "readonly",
    "require",
    "require_once",
    "return",
    "self",
    "static",
    "switch",
    "throw",
    "trait",
    "true",
    "try",
    "unset",
    "use",
    "var",
    "while",
    "xor",
    "yield",
];

static RESERVED: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| PHP_KEYWORDS.iter().copied().collect());

/// `true` for PHP reserved words in any letter case (`Throw`, `NULL`).
pub(crate) fn is_reserved_word(name: &str) -> bool {
    RESERVED.contains(name.to_ascii_lowercase().as_str())
}

/// The identifier right before `start`, skipping spaces (not across `(`, `|`, `?`, `,`).
fn previous_word(chars: &[char], start: usize) -> String {
    let mut k = start;
    while k > 0 && chars[k - 1].is_whitespace() {
        k -= 1;
    }
    let end = k;
    while k > 0 && is_ident_char(chars[k - 1]) {
        k -= 1;
    }
    chars[k..end]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase()
}

static LOCAL_TYPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"@(?:phpstan-|psalm-)?(?:type|import-type|template(?:-covariant|-contravariant)?)\s+([A-Za-z_]\w*)(?:.*?\bas\s+([A-Za-z_]\w*))?",
    )
    .unwrap()
});

/// Names a PHPDoc block declares for this file only — `@phpstan-type Alias`, imported type
/// aliases and `@template T` — which look like classes but are not.
fn local_type_names(content: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    for caps in LOCAL_TYPE_RE.captures_iter(content) {
        for i in 1..=2 {
            if let Some(m) = caps.get(i) {
                names.insert(m.as_str().to_string());
            }
        }
    }
    names
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::treesitter::php::PHP_PARSER;
    use crate::parsers::treesitter::LanguageParser;
    use crate::parsers::FileType;

    /// (ref name, line) → (FQN, kind) for every reference the PHP parser records.
    fn resolved(code: &str) -> Vec<(String, usize, Option<(String, &'static str)>)> {
        let symbols = PHP_PARSER.parse_symbols(code).unwrap();
        let refs = PHP_PARSER
            .extract_refs_for_lang(code, &symbols, FileType::Php)
            .unwrap();
        let names = resolve(code, &symbols, &refs);
        refs.into_iter()
            .zip(names.refs)
            .map(|(r, f)| (r.name, r.line, f))
            .collect()
    }

    fn fqn_of(code: &str, name: &str, line: usize) -> Option<String> {
        kind_of(code, name, line).map(|(fqn, _)| fqn)
    }

    fn kind_of(code: &str, name: &str, line: usize) -> Option<(String, &'static str)> {
        resolved(code)
            .into_iter()
            .find(|(n, l, _)| n == name && *l == line)
            .and_then(|(_, _, f)| f)
    }

    /// Every resolved FQN in the file.
    fn all_fqns(code: &str) -> Vec<String> {
        resolved(code)
            .into_iter()
            .filter_map(|(_, _, f)| f.map(|(fqn, _)| fqn))
            .collect()
    }

    const FILE: &str = r#"<?php

namespace App\Order;

use App\Shared\Money;
use App\Catalog\{Offer, Product as CatalogProduct};
use Vendor\Lib\Client as HttpClient;

final class OrderService
{
    use LoggerTrait;

    /** @var list<LineDto> */
    private array $lines = [];

    public function __construct(private Money $money, private HttpClient $client) {}

    public function build(CatalogProduct $product): Offer
    {
        $x = new \Other\Thing();
        $y = Sub\Helper::make();
        $h = 'App\\Handlers\\OrderHandler';
        return Offer::fromProduct($product, Status::ACTIVE);
    }
}
"#;

    #[test]
    fn imports_aliases_and_group_use() {
        assert_eq!(
            fqn_of(FILE, "Money", 16).as_deref(),
            Some("App\\Shared\\Money")
        );
        assert_eq!(
            fqn_of(FILE, "HttpClient", 16).as_deref(),
            Some("Vendor\\Lib\\Client")
        );
        assert_eq!(
            fqn_of(FILE, "CatalogProduct", 18).as_deref(),
            Some("App\\Catalog\\Product")
        );
        assert_eq!(
            fqn_of(FILE, "Offer", 18).as_deref(),
            Some("App\\Catalog\\Offer")
        );
    }

    #[test]
    fn same_namespace_trait_and_phpdoc() {
        assert_eq!(
            kind_of(FILE, "LoggerTrait", 11),
            Some(("App\\Order\\LoggerTrait".to_string(), "trait-use"))
        );
        assert_eq!(
            kind_of(FILE, "LineDto", 13),
            Some(("App\\Order\\LineDto".to_string(), "phpdoc"))
        );
        assert_eq!(
            kind_of(FILE, "Status", 23),
            Some(("App\\Order\\Status".to_string(), "static"))
        );
    }

    #[test]
    fn fully_qualified_relative_and_string() {
        assert_eq!(
            kind_of(FILE, "Thing", 20),
            Some(("Other\\Thing".to_string(), "new"))
        );
        assert_eq!(
            fqn_of(FILE, "Helper", 21).as_deref(),
            Some("App\\Order\\Sub\\Helper")
        );
        assert_eq!(
            kind_of(FILE, "OrderHandler", 22),
            Some(("App\\Handlers\\OrderHandler".to_string(), "string"))
        );
    }

    #[test]
    fn import_lines_name_the_imported_class() {
        assert_eq!(
            kind_of(FILE, "Money", 5),
            Some(("App\\Shared\\Money".to_string(), "import"))
        );
    }

    #[test]
    fn alias_on_import_line_resolves_to_the_imported_class() {
        let code = "<?php\nnamespace M;\nuse Orteka\\Share\\OrtekaVersion as Version;\nfinal class V1 extends Version {}\n";
        assert_eq!(
            kind_of(code, "Version", 3),
            Some(("Orteka\\Share\\OrtekaVersion".to_string(), "import"))
        );
        assert_eq!(
            kind_of(code, "OrtekaVersion", 3),
            Some(("Orteka\\Share\\OrtekaVersion".to_string(), "import"))
        );
        assert_eq!(
            kind_of(code, "Version", 4),
            Some(("Orteka\\Share\\OrtekaVersion".to_string(), "inheritance"))
        );
    }

    #[test]
    fn multi_line_group_use() {
        let code = "<?php\nnamespace App;\nuse Lib\\{\n    Alpha,\n    Beta as B,\n};\nfinal class X { public function a(Alpha $a, B $b): void {} }\n";
        assert_eq!(
            kind_of(code, "Alpha", 4),
            Some(("Lib\\Alpha".to_string(), "import"))
        );
        assert_eq!(fqn_of(code, "B", 7).as_deref(), Some("Lib\\Beta"));
        assert_eq!(fqn_of(code, "Alpha", 7).as_deref(), Some("Lib\\Alpha"));
    }

    #[test]
    fn members_and_namespace_segments_are_not_classes() {
        assert_eq!(fqn_of(FILE, "ACTIVE", 23), None);
        assert_eq!(fqn_of(FILE, "App", 3), None);
        assert_eq!(fqn_of(FILE, "Other", 20), None);
    }

    #[test]
    fn definitions_get_their_fqn() {
        let symbols = PHP_PARSER.parse_symbols(FILE).unwrap();
        let names = resolve(FILE, &symbols, &[]);
        assert!(names
            .qualified
            .values()
            .any(|fqn| fqn == "App\\Order\\OrderService"));
    }

    #[test]
    fn file_without_namespace_and_case_insensitive_alias() {
        let code =
            "<?php\nuse Bitrix\\Main\\Loader;\nLOADER::includeModule('x');\n$a = new Legacy();\n";
        assert_eq!(
            fqn_of(code, "LOADER", 3).as_deref(),
            Some("Bitrix\\Main\\Loader")
        );
        assert_eq!(fqn_of(code, "Legacy", 4).as_deref(), Some("Legacy"));
    }

    #[test]
    fn phpdoc_type_aliases_and_templates_are_not_classes() {
        let code = "<?php\nnamespace App;\n/**\n * @phpstan-type FilterArray array{a: int}\n * @template T of Item\n */\nfinal class Builder\n{\n    /** @return FilterArray */\n    public function a(): array { return []; }\n    /** @return T */\n    public function b(): mixed { return null; }\n}\n";
        assert_eq!(fqn_of(code, "FilterArray", 9), None);
        assert_eq!(fqn_of(code, "T", 11), None);
        assert_eq!(fqn_of(code, "Item", 5).as_deref(), Some("App\\Item"));
    }

    #[test]
    fn attribute_class_is_resolved() {
        let code = "<?php\nnamespace T;\nuse PHPUnit\\Framework\\Attributes\\CoversClass;\n#[CoversClass(Policy::class)]\nfinal class PolicyTest {}\n";
        assert_eq!(
            kind_of(code, "CoversClass", 4),
            Some((
                "PHPUnit\\Framework\\Attributes\\CoversClass".to_string(),
                "attribute"
            ))
        );
        assert_eq!(
            kind_of(code, "Policy", 4),
            Some(("T\\Policy".to_string(), "::class"))
        );
    }

    #[test]
    fn sql_in_strings_and_heredocs_is_not_code() {
        let code = "<?php\nnamespace App;\n$sql = \"\n    SELECT ID, NAME\n    FROM Orders\n    WHERE Active = 'Y'\n\";\n$q = <<<SQL\n    SELECT Product FROM Catalog\n    SQL;\n$r = new Real();\n";
        let fqns = all_fqns(code);
        assert_eq!(fqns, vec!["App\\Real".to_string()], "{fqns:?}");
    }

    #[test]
    fn enum_cases_named_arguments_and_constants_are_not_classes() {
        let code = "<?php\nnamespace App;\nenum Status: string\n{\n    case Active = 'a';\n    case DRAFT = 'd';\n}\n$c->remember(Key: 'k', ttl: 60, Count: 1);\n$x = MAX_ITEMS + DEFAULT_TTL;\nswitch ($s) { case Other::Active: break; }\n";
        let fqns = all_fqns(code);
        assert_eq!(fqns, vec!["App\\Other".to_string()], "{fqns:?}");
    }

    #[test]
    fn comments_and_inline_html_are_not_code() {
        let code = "<?php\nnamespace App;\n$a = new Real(); // Uses Legacy here\nif ($x) Throw New Real();\n# Old Thing\n?>\n<div class=\"Wrapper\">Hello World</div>\n<?= Render::block() ?>\n";
        let mut fqns = all_fqns(code);
        fqns.sort();
        fqns.dedup();
        assert_eq!(
            fqns,
            vec!["App\\Real".to_string(), "App\\Render".to_string()],
            "{fqns:?}"
        );
    }

    #[test]
    fn closing_tag_in_a_line_comment_ends_php() {
        let code = "<?php $a = new Real(); ?>\n<div><?//= Loc::getMessage('X') ?></div>\n<p>Don't Panic Ghost</p>\n<?php $b = new Second(); ?>\n<p>It's Phantom</p>\n<?php $c = new Third(); # done ?>\n<p>Wraith</p>\n";
        let mut fqns = all_fqns(code);
        fqns.sort();
        assert_eq!(fqns, vec!["Real", "Second", "Third"], "{fqns:?}");
    }

    #[test]
    fn xml_declaration_is_markup() {
        let code = "<?xml version=\"1.0\"?>\n<root>Item Value</root>\n<?php $a = new Real();\n";
        assert_eq!(all_fqns(code), vec!["Real".to_string()]);
    }

    #[test]
    fn namespace_after_declare_on_the_opening_line() {
        let code = "<?php declare(strict_types=1); namespace Fx\\One;\nfinal class One { public function a(): Two { return new Two(); } }\n";
        assert_eq!(fqn_of(code, "Two", 2).as_deref(), Some("Fx\\One\\Two"));
        let symbols = PHP_PARSER.parse_symbols(code).unwrap();
        let names = resolve(code, &symbols, &[]);
        assert!(names.qualified.values().any(|fqn| fqn == "Fx\\One\\One"));
    }

    #[test]
    fn short_open_tags_without_a_space_are_code() {
        let code = "<div>\n<?if (\\App\\Settings::I()->on()):?>\n<?$APPLICATION->IncludeComponent(Widget::NAME);?>\n<?endif?>\n</div>\n";
        assert_eq!(
            fqn_of(code, "Settings", 2).as_deref(),
            Some("App\\Settings")
        );
        assert_eq!(fqn_of(code, "Widget", 3).as_deref(), Some("Widget"));
    }

    #[test]
    fn inheritance_and_catch_kinds_end_with_their_clause() {
        let code = "<?php\nnamespace T;\n$x = new class extends Base { public function p(): Palette { return Palette::make(MAX_ITEMS); } };\ntry { run(); } catch (Failure $e) { log(Level::ERROR, LIMIT_X); }\n";
        assert_eq!(
            kind_of(code, "Base", 3),
            Some(("T\\Base".to_string(), "inheritance"))
        );
        assert_eq!(
            kind_of(code, "Palette", 3),
            Some(("T\\Palette".to_string(), "type"))
        );
        assert_eq!(fqn_of(code, "MAX_ITEMS", 3), None);
        assert_eq!(
            kind_of(code, "Failure", 4),
            Some(("T\\Failure".to_string(), "check"))
        );
        assert_eq!(fqn_of(code, "LIMIT_X", 4), None);
    }

    #[test]
    fn clauses_span_lines_and_end_where_they_close() {
        let code = "<?php\nnamespace T;\nfinal class A extends Base implements\n    First,\n    Second\n{\n    #[Route('/a')] public function route(Omicron $o): Pi { return new Pi(); }\n    #[Assert\\Choice(\n        choices: Status::ALL,\n    )]\n    public function b(): void\n    {\n        try { run(); } catch (\n            Upsilon | Phi $e\n        ) { log(LIMIT_X); }\n    }\n}\n";
        assert_eq!(
            kind_of(code, "First", 4),
            Some(("T\\First".to_string(), "inheritance"))
        );
        assert_eq!(
            kind_of(code, "Second", 5),
            Some(("T\\Second".to_string(), "inheritance"))
        );
        assert_eq!(
            kind_of(code, "Route", 7),
            Some(("T\\Route".to_string(), "attribute"))
        );
        assert_eq!(
            kind_of(code, "Omicron", 7),
            Some(("T\\Omicron".to_string(), "type"))
        );
        assert_eq!(
            kind_of(code, "Status", 9),
            Some(("T\\Status".to_string(), "static"))
        );
        assert_eq!(
            kind_of(code, "Upsilon", 14),
            Some(("T\\Upsilon".to_string(), "check"))
        );
        assert_eq!(fqn_of(code, "LIMIT_X", 15), None);
    }

    #[test]
    fn a_keyword_in_a_comment_opens_no_clause() {
        let code = "<?php\nnamespace T;\n/**\n * This class extends the idea of a catch\n */\nfinal class A\n{\n    public function b(): Omega { return foo(MAX_X); }\n}\n";
        assert_eq!(
            kind_of(code, "Omega", 8),
            Some(("T\\Omega".to_string(), "type"))
        );
        assert_eq!(fqn_of(code, "MAX_X", 8), None);
    }
}
