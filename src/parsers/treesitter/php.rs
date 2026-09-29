//! Tree-sitter based PHP parser

use anyhow::Result;
use std::sync::LazyLock;
use tree_sitter::{Language, Query, QueryCursor, StreamingIterator};

use super::{line_text, node_line, node_text, parse_tree, LanguageParser};
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
    /// Only class-like definitions hide a name. Methods, functions and constants of the file
    /// stay visible, so `$this->build()` in the class that declares `build()` is a reference —
    /// otherwise a template method called only by its own base class looked unused. The
    /// declaration line itself (`function build(`) is not a reference.
    fn extract_refs_for_lang(
        &self,
        content: &str,
        defined: &[ParsedSymbol],
        file_type: FileType,
    ) -> Result<Vec<ParsedRef>> {
        let definitions: Vec<ParsedSymbol> = defined
            .iter()
            .filter(|s| {
                matches!(
                    s.kind,
                    SymbolKind::Class
                        | SymbolKind::Interface
                        | SymbolKind::Trait
                        | SymbolKind::Enum
                        | SymbolKind::Object
                )
            })
            .cloned()
            .collect();
        let lines: Vec<&str> = content.lines().collect();
        let mut refs = extract_references_for_lang(content, &definitions, Some(file_type))?;
        refs.retain(|r| {
            !lines
                .get(r.line.wrapping_sub(1))
                .is_some_and(|line| declares_function(line, &r.name))
        });
        refs.extend(snake_case_calls(&lines));
        refs.extend(callable_methods(&lines));
        refs.extend(php_only_names(&lines));
        // `#` starts a comment in PHP (`#[` an attribute); the generic extractor knows only `//`.
        refs.retain(|r| {
            !lines.get(r.line.wrapping_sub(1)).is_some_and(|line| {
                let t = line.trim_start();
                t.starts_with('#') && !t.starts_with("#[")
            })
        });
        refs.extend(phpdoc::extract_phpdoc_refs(
            content,
            &definitions,
            file_type,
        ));
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

        let mut matches = cursor.matches(query, tree.root_node(), content.as_bytes());

        while let Some(m) = matches.next() {
            // Namespace
            if let Some(cap) = find_capture(m, idx_namespace_name) {
                let name = node_text(content, &cap.node);
                let line = node_line(&cap.node);
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: SymbolKind::Package,
                    line,
                    signature: line_text(content, line).trim().to_string(),
                    parents: vec![],
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
                    signature: line_text(content, line).trim().to_string(),
                    parents,
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
                    signature: line_text(content, line).trim().to_string(),
                    parents,
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
                    signature: line_text(content, line).trim().to_string(),
                    parents: vec![],
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
                    signature: line_text(content, line).trim().to_string(),
                    parents: vec![],
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
                    signature: line_text(content, line).trim().to_string(),
                    parents: vec![],
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
                    signature: line_text(content, line).trim().to_string(),
                    parents: vec![],
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
                    signature: line_text(content, line).trim().to_string(),
                    parents: vec![],
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
                    signature: line_text(content, line).trim().to_string(),
                    parents: vec![],
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
                    signature: line_text(content, line).trim().to_string(),
                    parents: vec![],
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
                    signature: line_text(content, line).trim().to_string(),
                    parents: vec![],
                });
                continue;
            }
        }

        Ok(symbols)
    }
}

/// Find a capture by index in a match
fn find_capture<'a>(
    m: &'a tree_sitter::QueryMatch<'a, 'a>,
    idx: Option<u32>,
) -> Option<&'a tree_sitter::QueryCapture<'a>> {
    let idx = idx?;
    m.captures.iter().find(|c| c.index == idx)
}

/// `function name(` / `function &name(` on this line.
fn declares_function(line: &str, name: &str) -> bool {
    let Some(pos) = line.find("function") else {
        return false;
    };
    let rest = line[pos + "function".len()..].trim_start();
    let rest = rest.strip_prefix('&').unwrap_or(rest).trim_start();
    rest.strip_prefix(name)
        .is_some_and(|after| after.trim_start().starts_with('('))
}

static SNAKE_CALL_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"([$\w]?)\b([a-z][a-z0-9]*_[a-z0-9_]*)\s*\(").unwrap());

static CALLABLE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"\[\s*(?:\\?[\w\\]+::class|'[\w\\]+'|"[\w\\]+")\s*,\s*['"]([A-Za-z_]\w*)['"]\s*\]|['"]\\?[\w\\]+::([A-Za-z_]\w*)['"]"#,
    )
    .unwrap()
});

/// Methods named in callables: `[Foo::class, 'handle']`, `['App\\Foo', 'handle']`,
/// `'Foo::handle'` — how Bitrix event handlers and agents are registered. Without these the
/// handler methods looked unused.
fn callable_methods(lines: &[&str]) -> Vec<ParsedRef> {
    let mut refs = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.len() > 2000 || trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }
        for caps in CALLABLE_RE.captures_iter(line) {
            if let Some(name) = caps.get(1).or_else(|| caps.get(2)) {
                refs.push(ParsedRef {
                    name: name.as_str().to_string(),
                    line: idx + 1,
                    context: crate::parsers::truncate_context(trimmed),
                });
            }
        }
    }
    refs
}

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

/// Names the generic extractor, built around Java/Kotlin naming, does not see: class names with
/// underscores (Bitrix ORM `EO_Product_Collection`), lower-case class names in class positions
/// (module installers `orteka_core`, legacy `nf_pp::`), and one- or two-letter method calls
/// (`->id()`). ALL-CAPS names with underscores are constants and are skipped.
fn php_only_names(lines: &[&str]) -> Vec<ParsedRef> {
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
        for caps in UNDERSCORE_CLASS_RE.captures_iter(line) {
            let name = &caps[1];
            if name.chars().any(|c| c.is_ascii_lowercase()) {
                push(name);
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

/// `get_items(` — the generic extractor sees only camelCase calls. Magic `__*` names and
/// `$variable(` calls are skipped, and so are comment lines and declarations.
fn snake_case_calls(lines: &[&str]) -> Vec<ParsedRef> {
    let mut refs = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.len() > 2000
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }
        for caps in SNAKE_CALL_RE.captures_iter(line) {
            if caps.get(1).is_some_and(|m| m.as_str() == "$") {
                continue;
            }
            let name = &caps[2];
            if declares_function(line, name) {
                continue;
            }
            refs.push(ParsedRef {
                name: name.to_string(),
                line: idx + 1,
                context: crate::parsers::truncate_context(trimmed),
            });
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
        assert_eq!(at("array_map"), vec![13]);
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
}
