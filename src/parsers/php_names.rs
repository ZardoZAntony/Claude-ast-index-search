//! PHP name resolution: fully qualified names for class references and definitions.
//!
//! The generic extractor keeps only the short name of a reference, so five different `OrderDto`
//! classes looked like one, and a dead class with a live namesake looked used. PHP resolves a
//! class name against the file's `namespace` and `use` imports; this module does the same for
//! every line (code, strings with FQNs, PHPDoc types) and gives class-like definitions their FQN.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::{ParsedRef, ParsedSymbol};
use crate::db::SymbolKind;

/// FQNs for one PHP file.
pub struct PhpNames {
    /// Definitions, keyed like `ParsedFile::qualified_names`: (kind, line, name) → FQN.
    pub qualified: HashMap<(String, usize, String), String>,
    /// One entry per reference, `None` when the reference is not a class name.
    pub refs: Vec<Option<String>>,
}

pub fn resolve(content: &str, symbols: &[ParsedSymbol], refs: &[ParsedRef]) -> PhpNames {
    let lines: Vec<&str> = content.lines().collect();
    let scopes = Scopes::build(&lines);
    let local_types = local_type_names(content);

    let mut per_line: HashMap<usize, HashMap<String, String>> = HashMap::new();
    for r in refs {
        per_line.entry(r.line).or_insert_with(|| {
            let text = lines.get(r.line.wrapping_sub(1)).copied().unwrap_or("");
            resolve_line(
                text,
                scopes.at(r.line),
                scopes.is_import_line(r.line),
                &local_types,
            )
        });
    }
    let ref_fqns = refs
        .iter()
        .map(|r| {
            per_line
                .get(&r.line)
                .and_then(|names| names.get(&r.name))
                .cloned()
        })
        .collect();

    let qualified = symbols
        .iter()
        .filter(|s| is_class_like(&s.kind))
        .map(|s| {
            let fqn = qualify(&scopes.at(s.line).namespace, &s.name);
            ((s.kind.as_str().to_string(), s.line, s.name.clone()), fqn)
        })
        .collect();

    PhpNames {
        qualified,
        refs: ref_fqns,
    }
}

fn is_class_like(kind: &SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum | SymbolKind::Object
    )
}

fn qualify(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        name.to_string()
    } else {
        format!("{namespace}\\{name}")
    }
}

/// Namespace and imports in effect from a given line on.
#[derive(Default, Clone)]
struct Scope {
    namespace: String,
    /// Lower-cased alias → FQN.
    imports: HashMap<String, String>,
}

struct Scopes {
    /// (first line, scope), ordered by line.
    ranges: Vec<(usize, Scope)>,
    import_lines: std::collections::HashSet<usize>,
}

impl Scopes {
    fn build(lines: &[&str]) -> Self {
        let depths = brace_depths(lines);
        let mut ranges = vec![(1, Scope::default())];
        let mut import_lines = std::collections::HashSet::new();
        // Imports live at the namespace's own brace depth; `use Trait;` sits deeper, in a class.
        let mut import_depth = 0usize;
        let mut idx = 0;

        while idx < lines.len() {
            let line_no = idx + 1;
            let trimmed = lines[idx].trim_start();

            if let Some(rest) = keyword_rest(trimmed, "namespace") {
                let rest = rest.trim_start();
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '\\')
                    .collect();
                if !rest.starts_with('\\') {
                    import_depth = if rest.contains('{') {
                        depths[idx] + 1
                    } else {
                        depths[idx]
                    };
                    let scope = Scope {
                        namespace: name.trim_start_matches('\\').to_string(),
                        imports: HashMap::new(),
                    };
                    ranges.push((line_no, scope));
                }
            } else if let Some(rest) = keyword_rest(trimmed, "use") {
                if depths[idx] == import_depth && !rest.trim_start().starts_with('(') {
                    // A statement may span lines (group use); collect up to the semicolon.
                    let mut statement = rest.to_string();
                    let mut end = idx;
                    while !statement.contains(';') && end + 1 < lines.len() {
                        end += 1;
                        statement.push(' ');
                        statement.push_str(lines[end]);
                    }
                    let mut scope = ranges.last().map(|(_, s)| s.clone()).unwrap_or_default();
                    parse_use(&statement, &mut scope.imports);
                    ranges.push((line_no, scope));
                    for l in idx..=end {
                        import_lines.insert(l + 1);
                    }
                    idx = end + 1;
                    continue;
                }
            }
            idx += 1;
        }

        Self {
            ranges,
            import_lines,
        }
    }

    fn at(&self, line: usize) -> &Scope {
        let pos = self.ranges.partition_point(|(from, _)| *from <= line);
        &self.ranges[pos.saturating_sub(1)].1
    }

    fn is_import_line(&self, line: usize) -> bool {
        self.import_lines.contains(&line)
    }
}

/// `rest` after `keyword` followed by whitespace, if the line starts with that keyword.
fn keyword_rest<'a>(trimmed: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = trimmed.strip_prefix(keyword)?;
    rest.starts_with(|c: char| c.is_whitespace() || c == '\\' || c == '{')
        .then_some(rest)
}

/// `use A\B, C as D;`, `use A\{B, C as D};` — function/const imports are skipped.
fn parse_use(statement: &str, imports: &mut HashMap<String, String>) {
    let body = statement.split(';').next().unwrap_or("").trim();
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
        let alias = alias
            .map(str::to_string)
            .unwrap_or_else(|| fqn.rsplit('\\').next().unwrap_or(&fqn).to_string());
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

/// Brace depth at the start of each line, ignoring braces in strings and comments.
fn brace_depths(lines: &[&str]) -> Vec<usize> {
    let mut depths = Vec::with_capacity(lines.len());
    let mut depth = 0usize;
    let mut in_block_comment = false;

    for line in lines {
        depths.push(depth);
        let chars: Vec<char> = line.chars().collect();
        let mut quote: Option<char> = None;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if in_block_comment {
                if c == '*' && chars.get(i + 1) == Some(&'/') {
                    in_block_comment = false;
                    i += 1;
                }
            } else if let Some(q) = quote {
                if c == '\\' {
                    i += 1;
                } else if c == q {
                    quote = None;
                }
            } else {
                match c {
                    '\'' | '"' => quote = Some(c),
                    '#' => break,
                    '/' if chars.get(i + 1) == Some(&'/') => break,
                    '/' if chars.get(i + 1) == Some(&'*') => {
                        in_block_comment = true;
                        i += 1;
                    }
                    '{' => depth += 1,
                    '}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
            i += 1;
        }
    }
    depths
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

/// Resolve every class-name token on one line: last segment → FQN (first occurrence wins).
fn resolve_line(
    line: &str,
    scope: &Scope,
    import_line: bool,
    local_types: &HashSet<String>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let trimmed = line.trim_start();
    if keyword_rest(trimmed, "namespace").is_some() {
        return out;
    }
    // `#[CoversClass(Foo::class)]`: inside an attribute `Name(` is a class, not a call.
    let attribute_line = trimmed.starts_with("#[");
    // In PHP string literals `\\` is one backslash: `'Orteka\\Foo'` names `Orteka\Foo`.
    let normalized = line.replace("\\\\", "\\");
    let chars: Vec<char> = normalized.chars().collect();
    let mut quote: Option<char> = None;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
                i += 1;
                continue;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            i += 1;
            continue;
        }
        if c == '$' {
            i += 1;
            while i < chars.len() && is_ident_char(chars[i]) {
                i += 1;
            }
            continue;
        }
        let starts_token = (c == '\\' && chars.get(i + 1).is_some_and(|n| is_ident_start(*n)))
            || is_ident_start(c);
        if !starts_token || (i > 0 && (is_ident_char(chars[i - 1]) || chars[i - 1] == '\\')) {
            i += 1;
            continue;
        }

        let start = i;
        let mut j = i;
        loop {
            if chars.get(j) == Some(&'\\') {
                j += 1;
            }
            let seg_start = j;
            while j < chars.len() && is_ident_char(chars[j]) {
                j += 1;
            }
            if j == seg_start {
                break;
            }
            if !(chars.get(j) == Some(&'\\')
                && chars.get(j + 1).is_some_and(|n| is_ident_start(*n)))
            {
                break;
            }
        }
        let token: String = chars[start..j].iter().collect();
        i = j;

        let last = token.rsplit('\\').next().unwrap_or(&token).to_string();
        if last.is_empty() || out.contains_key(&last) || local_types.contains(&token) {
            continue;
        }
        let context = TokenContext {
            in_string: quote.is_some(),
            import_line,
            attribute_line,
        };
        if let Some(fqn) = classify(&chars, start, j, &token, scope, context) {
            out.insert(last, fqn);
        }
    }
    out
}

#[derive(Clone, Copy)]
struct TokenContext {
    in_string: bool,
    import_line: bool,
    attribute_line: bool,
}

fn classify(
    chars: &[char],
    start: usize,
    end: usize,
    token: &str,
    scope: &Scope,
    context: TokenContext,
) -> Option<String> {
    if context.in_string {
        // A string names a class only as a full FQN (`'App\\Handler'`); plain words are prose.
        return token
            .contains('\\')
            .then(|| token.trim_start_matches('\\').to_string());
    }
    if context.import_line {
        return Some(token.trim_start_matches('\\').to_string());
    }

    // `$obj->name`, `$obj?->name`, `Foo::NAME` — members, not classes.
    let before: String = chars[..start].iter().rev().take(3).collect::<String>();
    let before = before.chars().rev().collect::<String>();
    if before.ends_with("->") || before.ends_with("::") {
        return None;
    }
    let prev_word = previous_word(chars, start);
    if matches!(
        prev_word.as_str(),
        "function" | "fn" | "const" | "goto" | "namespace"
    ) {
        return None;
    }
    // `name(` is a function call unless it is `new Name(`; `Name::` is always a class.
    let next = chars[end..].iter().find(|c| !c.is_whitespace()).copied();
    let after_is_scope = chars.get(end) == Some(&':') && chars.get(end + 1) == Some(&':');
    if next == Some('(') && prev_word != "new" && !after_is_scope && !context.attribute_line {
        return None;
    }

    let lower = token.to_ascii_lowercase();
    if matches!(lower.as_str(), "self" | "static" | "parent") {
        return None;
    }
    if let Some(stripped) = token.strip_prefix('\\') {
        return Some(stripped.to_string());
    }
    if let Some((first, rest)) = token.split_once('\\') {
        if first.eq_ignore_ascii_case("namespace") {
            return Some(qualify(&scope.namespace, rest));
        }
        return Some(match scope.imports.get(&first.to_ascii_lowercase()) {
            Some(prefix) => format!("{prefix}\\{rest}"),
            None => qualify(&scope.namespace, token),
        });
    }
    Some(
        scope
            .imports
            .get(&lower)
            .cloned()
            .unwrap_or_else(|| qualify(&scope.namespace, token)),
    )
}

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

    /// (ref name, line) → FQN for every reference the PHP parser records.
    fn fqns(code: &str) -> Vec<(String, usize, Option<String>)> {
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
        fqns(code)
            .into_iter()
            .find(|(n, l, _)| n == name && *l == line)
            .and_then(|(_, _, f)| f)
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
            fqn_of(FILE, "LoggerTrait", 11).as_deref(),
            Some("App\\Order\\LoggerTrait")
        );
        assert_eq!(
            fqn_of(FILE, "LineDto", 13).as_deref(),
            Some("App\\Order\\LineDto")
        );
        assert_eq!(
            fqn_of(FILE, "Status", 23).as_deref(),
            Some("App\\Order\\Status")
        );
    }

    #[test]
    fn fully_qualified_relative_and_string() {
        assert_eq!(fqn_of(FILE, "Thing", 20).as_deref(), Some("Other\\Thing"));
        assert_eq!(
            fqn_of(FILE, "Helper", 21).as_deref(),
            Some("App\\Order\\Sub\\Helper")
        );
        assert_eq!(
            fqn_of(FILE, "OrderHandler", 22).as_deref(),
            Some("App\\Handlers\\OrderHandler")
        );
    }

    #[test]
    fn import_lines_name_the_imported_class() {
        assert_eq!(
            fqn_of(FILE, "Money", 5).as_deref(),
            Some("App\\Shared\\Money")
        );
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
            fqn_of(code, "CoversClass", 4).as_deref(),
            Some("PHPUnit\\Framework\\Attributes\\CoversClass")
        );
        assert_eq!(fqn_of(code, "Policy", 4).as_deref(), Some("T\\Policy"));
    }
}
