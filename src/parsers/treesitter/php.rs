//! Tree-sitter based PHP parser

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use tree_sitter::{Language, Query, QueryCursor, StreamingIterator};

use super::{node_line, node_text, parse_tree, signature_line, text_end_line, LanguageParser};
use crate::db::SymbolKind;
use crate::parsers::{extract_references_for_lang, phpdoc, FileType, ParsedRef, ParsedSymbol};

static PHP_LANGUAGE: LazyLock<Language> = LazyLock::new(|| tree_sitter_php::LANGUAGE_PHP.into());

static PHP_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(&PHP_LANGUAGE, include_str!("queries/php.scm"))
        .expect("Failed to compile PHP tree-sitter query")
});

pub static PHP_PARSER: PhpParser = PhpParser;

pub struct PhpParser;

impl LanguageParser for PhpParser {
    fn extract_refs(&self, content: &str, defined: &[ParsedSymbol]) -> Result<Vec<ParsedRef>> {
        self.extract_refs_for_lang(content, defined, FileType::Php)
    }

    /// Code references plus class names from PHPDoc type positions, which the generic
    /// extractor skips together with the rest of the comment.
    ///
    /// No definition of the file hides a name: `new Foo()` in Foo's own factory and
    /// `$this->build()` in the class that declares `build()` are references too — a rename needs
    /// them, and a template method called only by its base class is not dead. Only the names
    /// being declared (`class Foo`, `function build(`, at the symbol's own line) are dropped.
    fn extract_refs_for_lang(
        &self,
        content: &str,
        defined: &[ParsedSymbol],
        file_type: FileType,
    ) -> Result<Vec<ParsedRef>> {
        let lines: Vec<&str> = content.lines().collect();
        let mut refs = extract_references_for_lang(content, &[], Some(file_type))?;
        refs.extend(php_extra_refs(&lines));
        // Each declaration hides one occurrence of its name on its line: in
        // `function Logger(): Logger { return new Logger(); }` the type and `new` stay.
        // `use Trait;` is recorded as an import symbol on its line, yet it is a reference.
        let mut declared: HashMap<(usize, String), usize> = HashMap::new();
        for s in defined.iter().filter(|s| s.kind != SymbolKind::Import) {
            *declared.entry((s.line, s.name.clone())).or_default() += 1;
        }
        refs.retain(|r| {
            let Some(line) = lines.get(r.line.wrapping_sub(1)) else {
                return true;
            };
            let t = line.trim_start();
            // `#` starts a comment in PHP (`#[` an attribute); the generic extractor knows only `//`.
            let hash_comment = t.starts_with('#') && !t.starts_with("#[");
            let declaration = match declared.get_mut(&(r.line, r.name.clone())) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    true
                }
                _ => false,
            };
            !hash_comment && !declaration && !calls_builtin_only(line, &r.name)
        });
        refs.extend(phpdoc::extract_phpdoc_refs(content, &[], file_type));
        Ok(refs)
    }

    fn parse_symbols(&self, content: &str) -> Result<Vec<ParsedSymbol>> {
        let tree = parse_tree(content, &PHP_LANGUAGE)?;
        let mut symbols = Vec::new();
        let query = &*PHP_QUERY;
        let mut cursor = QueryCursor::new();

        let capture_names = query.capture_names();
        let idx = |name: &str| -> Option<u32> {
            capture_names
                .iter()
                .position(|n| *n == name)
                .map(|i| i as u32)
        };

        let idx_namespace_name = idx("namespace_name");
        let idx_class_name = idx("class_name");
        let idx_class_parent = idx("class_parent");
        let idx_class_interface = idx("class_interface");
        let idx_interface_name = idx("interface_name");
        let idx_interface_parent = idx("interface_parent");
        let idx_trait_name = idx("trait_name");
        let idx_enum_name = idx("enum_name");
        let idx_func_name = idx("func_name");
        let idx_method_name = idx("method_name");
        let idx_const_name = idx("const_name");
        let idx_prop_name = idx("prop_name");
        let idx_use_name = idx("use_name");
        let idx_use_simple_name = idx("use_simple_name");
        let idx_trait_use_qualified = idx("trait_use_qualified");
        let idx_trait_use_name = idx("trait_use_name");
        let idx_definition = idx("definition");

        let mut matches = cursor.matches(query, tree.root_node(), content.as_bytes());

        while let Some(m) = matches.next() {
            let end_line = find_capture(m, idx_definition).map(|c| text_end_line(content, &c.node));

            // Namespace
            if let Some(cap) = find_capture(m, idx_namespace_name) {
                let name = node_text(content, &cap.node);
                let line = node_line(&cap.node);
                let end_line = cap
                    .node
                    .parent()
                    .filter(|decl| decl.child_by_field_name("body").is_none())
                    .map(|decl| statement_namespace_end_line(content, &decl))
                    .or(end_line);
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Package,
                    line,
                    signature: signature_line(content, line),
                    parents: vec![],
                    end_line,
                });
                continue;
            }

            // Class
            if let Some(name_cap) = find_capture(m, idx_class_name) {
                let name = node_text(content, &name_cap.node);
                let line = node_line(&name_cap.node);
                let mut parents = Vec::new();
                if let Some(parent_cap) = find_capture(m, idx_class_parent) {
                    let parent = node_text(content, &parent_cap.node);
                    parents.push((parent.to_string(), "extends".to_string()));
                }
                if let Some(iface_cap) = find_capture(m, idx_class_interface) {
                    let iface = node_text(content, &iface_cap.node);
                    parents.push((iface.to_string(), "implements".to_string()));
                }
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Class,
                    line,
                    signature: signature_line(content, line),
                    parents,
                    end_line,
                });
                continue;
            }

            // Interface
            if let Some(name_cap) = find_capture(m, idx_interface_name) {
                let name = node_text(content, &name_cap.node);
                let line = node_line(&name_cap.node);
                let parents = find_capture(m, idx_interface_parent)
                    .map(|p| {
                        vec![(
                            node_text(content, &p.node).to_string(),
                            "extends".to_string(),
                        )]
                    })
                    .unwrap_or_default();
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Interface,
                    line,
                    signature: signature_line(content, line),
                    parents,
                    end_line,
                });
                continue;
            }

            // Trait
            if let Some(cap) = find_capture(m, idx_trait_name) {
                let name = node_text(content, &cap.node);
                let line = node_line(&cap.node);
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Trait,
                    line,
                    signature: signature_line(content, line),
                    parents: vec![],
                    end_line,
                });
                continue;
            }

            // Enum
            if let Some(cap) = find_capture(m, idx_enum_name) {
                let name = node_text(content, &cap.node);
                let line = node_line(&cap.node);
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Enum,
                    line,
                    signature: signature_line(content, line),
                    parents: vec![],
                    end_line,
                });
                continue;
            }

            // Function (top-level)
            if let Some(cap) = find_capture(m, idx_func_name) {
                let name = node_text(content, &cap.node);
                let line = node_line(&cap.node);
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Function,
                    line,
                    signature: signature_line(content, line),
                    parents: vec![],
                    end_line,
                });
                continue;
            }

            // Method
            if let Some(cap) = find_capture(m, idx_method_name) {
                let name = node_text(content, &cap.node);
                let line = node_line(&cap.node);
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Function,
                    line,
                    signature: signature_line(content, line),
                    parents: vec![],
                    end_line,
                });
                continue;
            }

            // Constant
            if let Some(cap) = find_capture(m, idx_const_name) {
                let name = node_text(content, &cap.node);
                let line = node_line(&cap.node);
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Constant,
                    line,
                    signature: signature_line(content, line),
                    parents: vec![],
                    end_line,
                });
                continue;
            }

            // Property
            if let Some(cap) = find_capture(m, idx_prop_name) {
                let name = node_text(content, &cap.node);
                let line = node_line(&cap.node);
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Property,
                    line,
                    signature: signature_line(content, line),
                    parents: vec![],
                    end_line,
                });
                continue;
            }

            // Namespace use (import) — qualified or simple name
            if let Some(cap) =
                find_capture(m, idx_use_name).or_else(|| find_capture(m, idx_use_simple_name))
            {
                let name = node_text(content, &cap.node);
                let line = node_line(&cap.node);
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Import,
                    line,
                    signature: signature_line(content, line),
                    parents: vec![],
                    end_line,
                });
                continue;
            }

            // Trait use inside class — qualified or simple name
            if let Some(cap) = find_capture(m, idx_trait_use_qualified)
                .or_else(|| find_capture(m, idx_trait_use_name))
            {
                let name = node_text(content, &cap.node);
                let line = node_line(&cap.node);
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Import,
                    line,
                    signature: signature_line(content, line),
                    parents: vec![],
                    end_line,
                });
                continue;
            }
        }

        Ok(merge_repeated_declarations(symbols))
    }
}

/// The query matches a class once per implemented interface; keep one symbol per declaration
/// with all its parents.
fn merge_repeated_declarations(symbols: Vec<ParsedSymbol>) -> Vec<ParsedSymbol> {
    let mut merged: Vec<ParsedSymbol> = Vec::with_capacity(symbols.len());
    let mut seen: HashMap<(&'static str, String, usize), usize> = HashMap::new();
    for symbol in symbols {
        let key = (symbol.kind.as_str(), symbol.name.clone(), symbol.line);
        match seen.get(&key) {
            Some(&i) => {
                for parent in symbol.parents {
                    if !merged[i].parents.contains(&parent) {
                        merged[i].parents.push(parent);
                    }
                }
            }
            None => {
                seen.insert(key, merged.len());
                merged.push(symbol);
            }
        }
    }
    merged
}

/// `namespace A\B;` has no body: it scopes everything up to the next namespace
/// statement or the end of the file, while the grammar node stops at the `;`.
fn statement_namespace_end_line(content: &str, decl: &tree_sitter::Node) -> usize {
    let mut sibling = decl.next_sibling();
    while let Some(next) = sibling {
        if next.kind() == "namespace_definition" {
            return node_line(&next).saturating_sub(1).max(node_line(decl));
        }
        sibling = next.next_sibling();
    }
    decl.parent()
        .map(|program| text_end_line(content, &program))
        .unwrap_or_else(|| text_end_line(content, decl))
}

/// Find a capture by index in a match
fn find_capture<'a>(
    m: &'a tree_sitter::QueryMatch<'a, 'a>,
    idx: Option<u32>,
) -> Option<&'a tree_sitter::QueryCapture<'a>> {
    let idx = idx?;
    m.captures.iter().find(|c| c.index == idx)
}

/// Built-in PHP functions (`scripts/php-builtin-functions.php`): calls to them are noise in the
/// index, while project functions such as `is_spec()` or `plural_form()` are references.
static BUILTIN_FUNCTIONS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    include_str!("../php_builtin_functions.txt")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect()
});

/// `name` is a built-in function and every occurrence on the line is a global call, not a method
/// (`count($a)` is noise, `$list->count()` is not).
fn calls_builtin_only(line: &str, name: &str) -> bool {
    BUILTIN_FUNCTIONS.contains(name)
        && !line.match_indices(name).any(|(pos, _)| {
            let before = line[..pos].trim_end();
            before.ends_with("->") || before.ends_with("::")
        })
}

static SNAKE_CALL_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b([a-z][a-z0-9]*_[a-z0-9_]*)\s*\(").unwrap());
static CALLABLE_ARRAY_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?:\[|\barray\s*\()\s*(\\?[\w\\]+::class|\$\w+|'[^'\s]+'|"[^"\s]+")\s*,\s*['"]([A-Za-z_]\w*)['"]\s*[\])]"#,
    )
    .unwrap()
});
static CALLABLE_STRING_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"['"]\\?[A-Z][\w\\]*::([A-Za-z_]\w*)['"]"#).unwrap());
static UNDERSCORE_CLASS_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b([A-Z][A-Za-z0-9]*(?:_[A-Za-z0-9]+)+)\b").unwrap());
static LOWERCASE_CLASS_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?:\b(?:new|extends|implements|instanceof)\s+\\?(?:\w+\\)*([a-z_]\w*)|(?:^|[^\w$>:\\])\\?(?:\w+\\)*([a-z_]\w*)::)",
    )
    .unwrap()
});
static SHORT_METHOD_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?:->|::)\s*([A-Za-z_]\w?)\s*\(").unwrap());

/// References the generic extractor, built around Java/Kotlin naming, does not see:
/// - snake_case calls (`$this->get_items()`, project functions);
/// - methods named in callables — `[Foo::class, 'handle']`, `[$this, 'onEvent']`,
///   `array('App\Foo', 'handle')`, `'Foo::handle'` — how Bitrix event handlers and agents are
///   registered;
/// - class names with underscores (Bitrix ORM `EO_Product_Collection`) and lower-case class
///   names in class positions (module installers `orteka_core`, legacy `nf_pp::`);
/// - one- and two-letter method calls (`->id()`).
fn php_extra_refs(lines: &[&str]) -> Vec<ParsedRef> {
    let has_lower = |s: &str| s.chars().any(|c| c.is_ascii_lowercase());
    let mut refs = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.len() > 2000
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }
        let mut push = |name: &str| {
            if !crate::parsers::php_names::is_reserved_word(name) {
                refs.push(ParsedRef {
                    name: name.to_string(),
                    line: idx + 1,
                    context: crate::parsers::truncate_context(trimmed),
                });
            }
        };

        // Built-in functions are dropped later, together with the generic extractor's calls.
        for caps in SNAKE_CALL_RE.captures_iter(line) {
            let m = caps.get(1).expect("group 1 always matches");
            if !line[..m.start()].ends_with('$') {
                push(m.as_str());
            }
        }
        for caps in CALLABLE_ARRAY_RE.captures_iter(line) {
            let target = &caps[1];
            // A quoted first element must look like a class (`['ID', 'NAME']` is a field list).
            let quoted_class = target.starts_with(['\'', '"'])
                && (target.contains('\\')
                    || (target[1..].starts_with(|c: char| c.is_ascii_uppercase())
                        && has_lower(target)));
            if (quoted_class || !target.starts_with(['\'', '"'])) && has_lower(&caps[2]) {
                push(&caps[2]);
            }
        }
        for caps in CALLABLE_STRING_RE.captures_iter(line) {
            push(&caps[1]);
        }
        for caps in UNDERSCORE_CLASS_RE.captures_iter(line) {
            if has_lower(&caps[1]) {
                push(&caps[1]);
            }
        }
        for caps in LOWERCASE_CLASS_RE.captures_iter(line) {
            if let Some(m) = caps.get(1).or_else(|| caps.get(2)) {
                push(m.as_str());
            }
        }
        for caps in SHORT_METHOD_RE.captures_iter(line) {
            push(&caps[1]);
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_namespace() {
        let content = "<?php\nnamespace App\\Models;\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "App\\Models" && s.kind == SymbolKind::Package));
    }

    #[test]
    fn test_parse_class() {
        let content = "<?php\nclass User {\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "User" && s.kind == SymbolKind::Class));
    }

    #[test]
    fn test_parse_class_extends() {
        let content = "<?php\nclass User extends Model {\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        let cls = symbols
            .iter()
            .find(|s| s.name == "User" && s.kind == SymbolKind::Class);
        assert!(cls.is_some());
        assert!(cls
            .unwrap()
            .parents
            .iter()
            .any(|(p, k)| p == "Model" && k == "extends"));
    }

    #[test]
    fn test_parse_class_implements() {
        let content = "<?php\nclass User extends Model implements Authenticatable {\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        let cls = symbols
            .iter()
            .find(|s| s.name == "User" && s.kind == SymbolKind::Class);
        assert!(cls.is_some());
        assert!(cls
            .unwrap()
            .parents
            .iter()
            .any(|(p, k)| p == "Model" && k == "extends"));
        assert!(cls.unwrap().parents.iter().any(|(_, k)| k == "implements"));
    }

    #[test]
    fn test_parse_interface() {
        let content =
            "<?php\ninterface Authenticatable {\n    public function getAuthIdentifier();\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "Authenticatable" && s.kind == SymbolKind::Interface));
    }

    #[test]
    fn test_parse_interface_extends() {
        let content = "<?php\ninterface AdminAuth extends Authenticatable {\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        let iface = symbols
            .iter()
            .find(|s| s.name == "AdminAuth" && s.kind == SymbolKind::Interface);
        assert!(iface.is_some());
        assert!(iface
            .unwrap()
            .parents
            .iter()
            .any(|(p, k)| p == "Authenticatable" && k == "extends"));
    }

    #[test]
    fn test_parse_trait() {
        let content =
            "<?php\ntrait HasFactory {\n    public function factory() { return new static; }\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "HasFactory" && s.kind == SymbolKind::Trait));
    }

    #[test]
    fn test_parse_enum() {
        let content = "<?php\nenum Status {\n    case Active;\n    case Inactive;\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "Status" && s.kind == SymbolKind::Enum));
    }

    #[test]
    fn test_parse_function() {
        let content = "<?php\nfunction helper(): string {\n    return 'hello';\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "helper" && s.kind == SymbolKind::Function));
    }

    #[test]
    fn test_parse_method() {
        let content = "<?php\nclass User {\n    public function getName(): string {\n        return $this->name;\n    }\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "getName" && s.kind == SymbolKind::Function));
    }

    #[test]
    fn test_parse_constant() {
        let content =
            "<?php\nclass Config {\n    const MAX_RETRIES = 3;\n    const VERSION = '1.0';\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "MAX_RETRIES" && s.kind == SymbolKind::Constant));
        assert!(symbols
            .iter()
            .any(|s| s.name == "VERSION" && s.kind == SymbolKind::Constant));
    }

    #[test]
    fn test_parse_property() {
        let content = "<?php\nclass User {\n    public string $name;\n    protected int $age;\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "$name" && s.kind == SymbolKind::Property));
        assert!(symbols
            .iter()
            .any(|s| s.name == "$age" && s.kind == SymbolKind::Property));
    }

    #[test]
    fn test_parse_use_import() {
        let content = "<?php\nuse App\\Models\\User;\nuse Illuminate\\Support\\Facades\\DB;\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "App\\Models\\User" && s.kind == SymbolKind::Import));
        assert!(symbols
            .iter()
            .any(|s| s.name == "Illuminate\\Support\\Facades\\DB" && s.kind == SymbolKind::Import));
    }

    #[test]
    fn test_parse_trait_use() {
        let content = "<?php\nclass User {\n    use HasFactory;\n    use Notifiable;\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols
            .iter()
            .any(|s| s.name == "HasFactory" && s.kind == SymbolKind::Import));
        assert!(symbols
            .iter()
            .any(|s| s.name == "Notifiable" && s.kind == SymbolKind::Import));
    }

    #[test]
    fn test_comments_ignored() {
        let content =
            "<?php\n// class FakeClass {}\n/* class AnotherFake {} */\nclass RealClass {\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        assert!(symbols.iter().any(|s| s.name == "RealClass"));
        assert!(!symbols.iter().any(|s| s.name == "FakeClass"));
        assert!(!symbols.iter().any(|s| s.name == "AnotherFake"));
    }

    #[test]
    fn test_parse_full_laravel_model() {
        let content = r#"<?php

namespace App\Models;

use Illuminate\Database\Eloquent\Model;
use Illuminate\Contracts\Auth\Authenticatable;

class User extends Model implements Authenticatable {
    use HasFactory;
    use Notifiable;

    const TABLE = 'users';

    public string $name;
    protected string $email;

    public function getName(): string {
        return $this->name;
    }

    public static function findByEmail(string $email): ?self {
        return static::where('email', $email)->first();
    }
}
"#;
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();

        // Namespace
        assert!(symbols
            .iter()
            .any(|s| s.name == "App\\Models" && s.kind == SymbolKind::Package));

        // Imports
        assert!(symbols
            .iter()
            .any(|s| s.name == "Illuminate\\Database\\Eloquent\\Model"
                && s.kind == SymbolKind::Import));
        assert!(symbols
            .iter()
            .any(|s| s.name == "Illuminate\\Contracts\\Auth\\Authenticatable"
                && s.kind == SymbolKind::Import));

        // Class
        let cls = symbols
            .iter()
            .find(|s| s.name == "User" && s.kind == SymbolKind::Class);
        assert!(cls.is_some());

        // Trait use
        assert!(symbols
            .iter()
            .any(|s| s.name == "HasFactory" && s.kind == SymbolKind::Import));
        assert!(symbols
            .iter()
            .any(|s| s.name == "Notifiable" && s.kind == SymbolKind::Import));

        // Constant
        assert!(symbols
            .iter()
            .any(|s| s.name == "TABLE" && s.kind == SymbolKind::Constant));

        // Properties
        assert!(symbols
            .iter()
            .any(|s| s.name == "$name" && s.kind == SymbolKind::Property));
        assert!(symbols
            .iter()
            .any(|s| s.name == "$email" && s.kind == SymbolKind::Property));

        // Methods
        assert!(symbols
            .iter()
            .any(|s| s.name == "getName" && s.kind == SymbolKind::Function));
        assert!(symbols
            .iter()
            .any(|s| s.name == "findByEmail" && s.kind == SymbolKind::Function));
    }

    #[test]
    fn same_file_method_calls_are_references_but_declarations_are_not() {
        let content = "<?php\nabstract class Base\n{\n    abstract protected function builderClass(): string;\n\n    public function run(): void\n    {\n        $c = $this->builderClass();\n        $items = $this->get_items();\n        $f = $callback_fn($c);\n    }\n\n    private function get_items(): array { return array_map(null, []); }\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        let refs = PHP_PARSER.extract_refs(content, &symbols).unwrap();
        let at = |name: &str| -> Vec<usize> {
            refs.iter()
                .filter(|r| r.name == name)
                .map(|r| r.line)
                .collect()
        };
        assert_eq!(at("builderClass"), vec![8]);
        assert_eq!(at("get_items"), vec![9]);
        assert!(at("array_map").is_empty(), "built-in functions are noise");
        assert!(at("callback_fn").is_empty());
        assert!(at("Base").is_empty());
    }

    #[test]
    fn php_specific_names_are_references() {
        let content = "<?php\nclass Entity extends EO_Product {}\n/** @var EO_Product_Collection $c */\n$c = new EO_Product_Collection();\nif (SITE_ID === 's1') {}\n$i = new orteka_core();\nnf_pp::run();\n$x = self::build(); $y = $this->id();\n# EO_Old_Thing in a comment\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        let refs = PHP_PARSER.extract_refs(content, &symbols).unwrap();
        let at = |name: &str| -> Vec<usize> {
            let mut lines: Vec<usize> = refs
                .iter()
                .filter(|r| r.name == name)
                .map(|r| r.line)
                .collect();
            lines.sort();
            lines.dedup();
            lines
        };
        assert_eq!(at("EO_Product"), vec![2]);
        assert_eq!(at("EO_Product_Collection"), vec![3, 4]);
        assert!(at("SITE_ID").is_empty());
        assert_eq!(at("orteka_core"), vec![6]);
        assert_eq!(at("nf_pp"), vec![7]);
        assert!(at("self").is_empty());
        assert_eq!(at("id"), vec![8]);
        assert!(at("EO_Old_Thing").is_empty());
    }

    #[test]
    fn methods_named_in_callables_are_references() {
        let content = "<?php\n$em->addEventHandler('main', 'OnEndBufferContent', [Bridge::class, 'onEndBufferContent']);\n$em->addEventHandler('main', 'OnAdmin', ['App\\\\Admin\\\\Handler', 'onAdminListDisplay']);\n\\CAgent::AddAgent('\\\\App\\\\Agent::run_daily();');\n$cb = 'App\\\\Agent::cleanup';\n";
        let refs = PHP_PARSER.extract_refs(content, &[]).unwrap();
        let at = |name: &str| -> Vec<usize> {
            refs.iter()
                .filter(|r| r.name == name)
                .map(|r| r.line)
                .collect()
        };
        assert_eq!(at("onEndBufferContent"), vec![2]);
        assert_eq!(at("onAdminListDisplay"), vec![3]);
        assert_eq!(at("run_daily"), vec![4]);
        assert_eq!(at("cleanup"), vec![5]);
    }

    #[test]
    fn callables_name_methods_but_field_lists_do_not() {
        let content = "<?php\n$em->addEventHandler('main', 'OnProlog', [$this, 'onEvent']);\nAddEventHandler('main', 'OnEnd', array('CLegacyHandler', 'OnEndHandler'));\n$select = ['ID', 'NAME']; $order = ['asc', 'desc'];\n$x = $this->array_thing(); $y = array_map(null, []);\n";
        let refs = PHP_PARSER.extract_refs(content, &[]).unwrap();
        let has = |name: &str, line: usize| refs.iter().any(|r| r.name == name && r.line == line);
        assert!(has("onEvent", 2));
        assert!(has("OnEndHandler", 3));
        // `desc` could come only from reading `['asc', 'desc']` as a callable.
        assert!(!has("desc", 4));
        assert!(has("array_thing", 5));
        assert!(!has("array_map", 5));
    }

    #[test]
    fn a_class_references_itself_but_its_declaration_is_not_a_reference() {
        let content = "<?php\nfinal class Runner\n{\n    public static function make(): self { return new Runner(); }\n    /** @return Runner */\n    public function copy(): Runner { return Runner::make(); }\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        let refs = PHP_PARSER.extract_refs(content, &symbols).unwrap();
        let mut lines: Vec<usize> = refs
            .iter()
            .filter(|r| r.name == "Runner")
            .map(|r| r.line)
            .collect();
        lines.sort();
        lines.dedup();
        assert_eq!(lines, vec![4, 5, 6]);
    }

    #[test]
    fn a_class_with_several_interfaces_is_one_symbol() {
        let content = "<?php\nfinal class Adapter implements First, Second, Third {}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        let classes: Vec<&ParsedSymbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        let parents: Vec<&str> = classes[0].parents.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(parents, vec!["First", "Second", "Third"]);
    }

    #[test]
    fn project_functions_that_look_built_in_are_references() {
        let content = "<?php\n$ok = is_valid_item($x) && is_array($x);\n$t = mb_ucfirst($s) . mb_strtolower($s);\n";
        let refs = PHP_PARSER.extract_refs(content, &[]).unwrap();
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(
            names.contains(&"is_valid_item") && names.contains(&"mb_ucfirst"),
            "{names:?}"
        );
        assert!(
            !names.contains(&"is_array") && !names.contains(&"mb_strtolower"),
            "{names:?}"
        );
    }

    #[test]
    fn a_keyword_later_on_the_line_does_not_hide_a_reference() {
        let content = "<?php\nfinal class Maker\n{\n    public function make(): AbstractItem { return $this->item; } // was function AbstractItem\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        let refs = PHP_PARSER.extract_refs(content, &symbols).unwrap();
        assert!(refs.iter().any(|r| r.name == "AbstractItem" && r.line == 4));
        assert!(!refs.iter().any(|r| r.name == "make" && r.line == 4));
        assert!(!refs.iter().any(|r| r.name == "Maker"));
    }

    #[test]
    fn a_method_named_like_its_return_class_keeps_the_class_references() {
        let content = "<?php\nfinal class Factory\n{\n    public function Logger(): Logger { return new Logger(); }\n}\n";
        let symbols = PHP_PARSER.parse_symbols(content).unwrap();
        let refs = PHP_PARSER.extract_refs(content, &symbols).unwrap();
        let n = refs
            .iter()
            .filter(|r| r.name == "Logger" && r.line == 4)
            .count();
        assert_eq!(n, 2, "the declaration hides one of three occurrences");
    }

    #[test]
    fn global_built_in_calls_are_dropped_but_methods_named_alike_are_kept() {
        let content =
            "<?php\n$n = count($a) + strlen($s) + bcadd('1', '2');\n$m = $list->count();\n";
        let refs = PHP_PARSER.extract_refs(content, &[]).unwrap();
        let has = |name: &str, line: usize| refs.iter().any(|r| r.name == name && r.line == line);
        assert!(!has("count", 2) && !has("strlen", 2) && !has("bcadd", 2));
        assert!(has("count", 3));
    }
}
