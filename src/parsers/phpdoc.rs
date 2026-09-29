//! PHPDoc type references for PHP.
//!
//! The generic reference extractor skips comment lines, so a class that appears only in a
//! docblock (`@var ItemDto[]`, `@param list<Foo>`, `@extends Base<Dto>`) never became a usage.
//! In PHP these annotations are part of the contract: static analysers and runtime mappers read
//! them, and a rename that misses them silently breaks typing. This module reads the type
//! positions of PHPDoc tags and returns them as references. Free-form descriptions are ignored:
//! a type ends at whitespace outside brackets.

use std::collections::HashSet;

use super::{is_noise_ref_name, truncate_context, FileType, ParsedRef, ParsedSymbol};

/// Tags whose first argument is a type (`@var Foo $x`, `@throws FooException`), after dropping
/// a `phpstan-` / `psalm-` prefix.
const TYPE_TAGS: &[&str] = &[
    "var",
    "param",
    "param-out",
    "return",
    "throws",
    "property",
    "property-read",
    "property-write",
    "mixin",
    "extends",
    "implements",
    "use",
    "template-extends",
    "template-implements",
    "template-use",
    "require-extends",
    "require-implements",
    "assert",
    "assert-if-true",
    "assert-if-false",
    "self-out",
    "this-out",
];

/// Extract class references from PHPDoc blocks (`/** … */` and `/* @var … */`).
pub fn extract_phpdoc_refs(
    content: &str,
    defined: &[ParsedSymbol],
    file_type: FileType,
) -> Vec<ParsedRef> {
    let defined_names: HashSet<&str> = defined.iter().map(|s| s.name.as_str()).collect();
    let lines: Vec<&str> = content.lines().collect();
    let mut refs = Vec::new();
    let mut seen: HashSet<(String, usize)> = HashSet::new();

    let mut block: Vec<(usize, String)> = Vec::new();
    let mut in_block = false;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !in_block && trimmed.starts_with("/**/") {
            continue;
        }
        let body = if in_block {
            match trimmed.strip_prefix('*') {
                Some(rest) if !rest.starts_with('/') => rest,
                _ => trimmed,
            }
        } else if let Some(rest) = trimmed.strip_prefix("/**") {
            in_block = true;
            rest
        } else if let Some(rest) = trimmed.strip_prefix("/*") {
            if !rest.trim_start().starts_with('@') {
                continue;
            }
            in_block = true;
            rest
        } else {
            continue;
        };

        let body = match body.find("*/") {
            Some(end) => {
                in_block = false;
                &body[..end]
            }
            None => body,
        };
        block.push((idx + 1, body.to_string()));

        if !in_block {
            for (name, line_no) in scan_block(&block) {
                if defined_names.contains(name.as_str())
                    || is_noise_ref_name(&name, Some(file_type))
                    || !seen.insert((name.clone(), line_no))
                {
                    continue;
                }
                let context = lines.get(line_no - 1).map_or("", |l| l.trim());
                refs.push(ParsedRef {
                    name,
                    line: line_no,
                    context: truncate_context(context),
                });
            }
            block.clear();
        }
    }

    refs
}

/// Docblock text flattened to characters, each tagged with its source line.
struct Doc {
    chars: Vec<(char, usize)>,
}

impl Doc {
    fn at(&self, i: usize) -> Option<char> {
        self.chars.get(i).map(|(c, _)| *c)
    }

    fn line(&self, i: usize) -> usize {
        self.chars
            .get(i)
            .or(self.chars.last())
            .map_or(0, |(_, l)| *l)
    }

    /// Skip spaces and tabs, never crossing a line break.
    fn skip_inline_ws(&self, mut i: usize) -> usize {
        while matches!(self.at(i), Some(' ') | Some('\t')) {
            i += 1;
        }
        i
    }

    /// Read `[A-Za-z0-9_\\-]` starting at `i`.
    fn read_word(&self, mut i: usize) -> (String, usize) {
        let mut word = String::new();
        while let Some(c) = self.at(i) {
            if c.is_ascii_alphanumeric() || c == '_' || c == '\\' || c == '-' {
                word.push(c);
                i += 1;
            } else {
                break;
            }
        }
        (word, i)
    }

    /// Whether `i` is the first non-blank character of its line.
    fn at_line_start(&self, i: usize) -> bool {
        let mut k = i;
        while k > 0 {
            k -= 1;
            match self.at(k) {
                Some('\n') => return true,
                Some(' ') | Some('\t') => continue,
                _ => return false,
            }
        }
        true
    }
}

fn scan_block(block: &[(usize, String)]) -> Vec<(String, usize)> {
    let mut chars = Vec::new();
    for (line, text) in block {
        chars.extend(text.chars().map(|c| (c, *line)));
        chars.push(('\n', *line));
    }
    let doc = Doc { chars };
    let mut out = Vec::new();
    let mut i = 0;

    while let Some(c) = doc.at(i) {
        if c == '@' && doc.at_line_start(i) {
            let (tag, next) = doc.read_word(i + 1);
            i = read_tag(&doc, &tag, next, &mut out);
        } else if c == '{' && doc.at(i + 1) == Some('@') {
            let (tag, next) = doc.read_word(i + 2);
            i = if tag == "see" || tag == "link" {
                read_reference(&doc, next, &mut out)
            } else {
                next
            };
        } else {
            i += 1;
        }
    }

    out
}

/// Dispatch on a tag name; returns the position after whatever was consumed.
fn read_tag(doc: &Doc, tag: &str, i: usize, out: &mut Vec<(String, usize)>) -> usize {
    let base = tag
        .strip_prefix("phpstan-")
        .or_else(|| tag.strip_prefix("psalm-"))
        .unwrap_or(tag);

    match base {
        tag if TYPE_TAGS.contains(&tag) => {
            let start = doc.skip_inline_ws(i);
            // `@param $x Description` without a type: the next words are prose, not a type.
            if doc.at(start) == Some('$') {
                return start;
            }
            read_type(doc, start, out)
        }
        "type" => {
            // `@phpstan-type Alias = Type`, `@psalm-type Alias Type`
            let (_, after_alias) = doc.read_word(doc.skip_inline_ws(i));
            let mut j = doc.skip_inline_ws(after_alias);
            if doc.at(j) == Some('=') {
                j = doc.skip_inline_ws(j + 1);
            }
            read_type(doc, j, out)
        }
        "import-type" => {
            // `@phpstan-import-type Alias from Foo`
            let (_, after_alias) = doc.read_word(doc.skip_inline_ws(i));
            let (keyword, after_keyword) = doc.read_word(doc.skip_inline_ws(after_alias));
            if keyword != "from" {
                return after_keyword;
            }
            read_type(doc, doc.skip_inline_ws(after_keyword), out)
        }
        "template" | "template-covariant" | "template-contravariant" => {
            // `@template T of Bound = Default`, psalm: `@template T as Bound`
            let (_, after_name) = doc.read_word(doc.skip_inline_ws(i));
            let mut j = doc.skip_inline_ws(after_name);
            let (keyword, after_keyword) = doc.read_word(j);
            if keyword == "of" || keyword == "as" {
                j = read_type(doc, doc.skip_inline_ws(after_keyword), out);
                j = doc.skip_inline_ws(j);
            }
            if doc.at(j) == Some('=') {
                j = read_type(doc, doc.skip_inline_ws(j + 1), out);
            }
            j
        }
        "method" => read_method(doc, i, out),
        "see" | "uses" | "used-by" | "covers" | "coversDefaultClass" => read_reference(doc, i, out),
        _ => i,
    }
}

/// Read one type expression starting at `i`, pushing class names; returns the end position.
///
/// The expression ends at whitespace outside brackets, except around `|` / `&` and after a
/// callable return colon. Array-shape keys, variables and quoted literals are skipped.
fn read_type(doc: &Doc, i: usize, out: &mut Vec<(String, usize)>) -> usize {
    let mut depth = 0usize;
    let mut j = i;
    let mut last = ' ';

    while let Some(c) = doc.at(j) {
        if c.is_whitespace() {
            if depth > 0 {
                j += 1;
                continue;
            }
            let k = doc.skip_inline_ws(j);
            match doc.at(k) {
                Some('|') | Some('&') => {
                    j = k;
                    continue;
                }
                Some(next) if next != '\n' && matches!(last, '|' | '&' | ':') => {
                    j = k;
                    continue;
                }
                _ => break,
            }
        }
        match c {
            '<' | '(' | '{' | '[' => depth += 1,
            '>' | ')' | '}' | ']' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            '\'' | '"' => {
                j += 1;
                while let Some(q) = doc.at(j) {
                    if q == c {
                        break;
                    }
                    j += 1;
                }
            }
            '$' => {
                let (_, end) = doc.read_word(j + 1);
                last = '$';
                j = end;
                continue;
            }
            c if c.is_ascii_alphabetic() || c == '_' || c == '\\' => {
                let (word, end) = doc.read_word(j);
                // `Foo::CONST` / `Foo::*` still references Foo; skip the member part.
                let mut after = end;
                if doc.at(after) == Some(':') && doc.at(after + 1) == Some(':') {
                    after += 2;
                    while matches!(doc.at(after), Some(ch) if ch.is_ascii_alphanumeric() || ch == '_' || ch == '*')
                    {
                        after += 1;
                    }
                } else if is_shape_key(doc, end) {
                    last = ':';
                    j = end;
                    continue;
                }
                push_names(&word, doc.line(j), out);
                last = 'a';
                j = after;
                continue;
            }
            _ => {}
        }
        if !c.is_whitespace() {
            last = c;
        }
        j += 1;
    }

    j
}

/// `key: type` / `key?: type` inside an array shape — but not `Foo::BAR` or `? A : B`.
fn is_shape_key(doc: &Doc, end: usize) -> bool {
    match doc.at(end) {
        Some(':') => doc.at(end + 1) != Some(':'),
        Some('?') => doc.at(end + 1) == Some(':'),
        _ => false,
    }
}

/// `@method [static] [ReturnType] name(ParamType $a, …)` — types only, never the method name.
fn read_method(doc: &Doc, i: usize, out: &mut Vec<(String, usize)>) -> usize {
    let mut start = doc.skip_inline_ws(i);
    let (word, after_word) = doc.read_word(start);
    if word == "static" && matches!(doc.at(after_word), Some(' ') | Some('\t')) {
        start = doc.skip_inline_ws(after_word);
    }

    // First `(` at bracket depth 0 on this line opens the parameter list.
    let mut depth = 0usize;
    let mut paren = None;
    let mut j = start;
    while let Some(c) = doc.at(j) {
        match c {
            '\n' => break,
            '<' | '{' | '[' => depth += 1,
            '>' | '}' | ']' => depth = depth.saturating_sub(1),
            '(' if depth == 0 => {
                paren = Some(j);
                break;
            }
            _ => {}
        }
        j += 1;
    }
    let Some(paren) = paren else {
        return read_type(doc, start, out);
    };

    let mut name_start = paren;
    while name_start > start
        && matches!(doc.at(name_start - 1), Some(c) if c.is_ascii_alphanumeric() || c == '_')
    {
        name_start -= 1;
    }
    let mut k = start;
    while k < name_start {
        k = doc.skip_inline_ws(read_type(doc, k, out)).max(k + 1);
    }

    // Parameter list: every type before a `$name`; defaults and names are skipped by read_type.
    let mut p = paren + 1;
    let mut level = 0usize;
    let mut segment_start = p;
    while let Some(c) = doc.at(p) {
        match c {
            '(' | '<' | '{' | '[' => level += 1,
            ')' if level == 0 => break,
            ')' | '>' | '}' | ']' => level = level.saturating_sub(1),
            '\n' => break,
            ',' if level == 0 => {
                read_param(doc, segment_start, p, out);
                segment_start = p + 1;
            }
            _ => {}
        }
        p += 1;
    }
    read_param(doc, segment_start, p, out);

    // Optional return type after the parameter list: `name(): Foo`.
    let mut r = doc.skip_inline_ws(p + 1);
    if doc.at(r) == Some(':') {
        r = read_type(doc, doc.skip_inline_ws(r + 1), out);
    }
    r
}

/// One `@method` parameter: `Type $name = default` → the type part only.
fn read_param(doc: &Doc, from: usize, to: usize, out: &mut Vec<(String, usize)>) {
    let mut j = doc.skip_inline_ws(from);
    while j < to {
        match doc.at(j) {
            Some('$') | Some('=') => return,
            Some(c) if c.is_whitespace() => j += 1,
            Some(_) => {
                let end = read_type(doc, j, out);
                j = end.max(j + 1);
            }
            None => return,
        }
    }
}

/// `@see Foo`, `@see \Foo\Bar::baz()`, `{@see Foo}` — the class part of a reference.
fn read_reference(doc: &Doc, i: usize, out: &mut Vec<(String, usize)>) -> usize {
    let start = doc.skip_inline_ws(i);
    let mut j = start;
    let mut token = String::new();
    while let Some(c) = doc.at(j) {
        if c.is_whitespace() || c == '}' {
            break;
        }
        token.push(c);
        j += 1;
    }
    if token.contains("://") {
        return j;
    }
    let class_part = token.split("::").next().unwrap_or("");
    let class_part = class_part.split('(').next().unwrap_or("");
    push_names(class_part, doc.line(start), out);
    j
}

/// Emit each CamelCase segment of a (possibly qualified) name, as the generic extractor does
/// for code lines: `\Orteka\Foo\BarDto` → `Orteka`, `Foo`, `BarDto`.
fn push_names(word: &str, line: usize, out: &mut Vec<(String, usize)>) {
    for segment in word.split('\\') {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        if first.is_ascii_uppercase() && chars.all(|c| c.is_ascii_alphanumeric()) {
            out.push((segment.to_string(), line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(content: &str) -> Vec<(String, usize)> {
        let mut refs: Vec<(String, usize)> = extract_phpdoc_refs(content, &[], FileType::Php)
            .into_iter()
            .map(|r| (r.name, r.line))
            .collect();
        refs.sort();
        refs
    }

    fn only_names(content: &str) -> Vec<String> {
        let mut n: Vec<String> = names(content).into_iter().map(|(n, _)| n).collect();
        n.sort();
        n.dedup();
        n
    }

    #[test]
    fn var_array_of_dto_above_property() {
        let code = "<?php\nfinal class A {\n    public function __construct(\n        /** @var ItemDto[] */\n        public array $items,\n    ) {}\n}\n";
        assert_eq!(names(code), vec![("ItemDto".to_string(), 4)]);
    }

    #[test]
    fn generics_list_and_array_with_key() {
        let code = "/**\n * @param list<FooDto> $a\n * @param array<int, BarDto> $b\n * @return array<string, list<BazDto>>\n */";
        assert_eq!(only_names(code), vec!["BarDto", "BazDto", "FooDto"]);
    }

    #[test]
    fn description_words_are_not_types() {
        let code = "/**\n * @param Foo $x Some Description With Capitals\n * @throws BarException When Something Fails\n * Plain Prose Line Mentions Nothing\n */";
        assert_eq!(only_names(code), vec!["BarException", "Foo"]);
    }

    #[test]
    fn nullable_union_intersection_and_spaced_union() {
        let code = "/**\n * @var ?Foo\n * @var Bar|null|Baz\n * @var Qux&Quux\n * @return Alpha | Beta\n */";
        assert_eq!(
            only_names(code),
            vec!["Alpha", "Bar", "Baz", "Beta", "Foo", "Quux", "Qux"]
        );
    }

    #[test]
    fn qualified_names_emit_every_segment() {
        let code = "/** @var \\Orteka\\Catalog\\ItemDto $x */";
        assert_eq!(only_names(code), vec!["Catalog", "ItemDto", "Orteka"]);
    }

    #[test]
    fn multiline_array_shape_skips_keys() {
        let code = "/** @var array<int, array{\n *     id: int,\n *     ID: int,\n *     item: ItemDto,\n *     opt?: OptDto,\n * }> */";
        assert_eq!(
            names(code),
            vec![("ItemDto".to_string(), 4), ("OptDto".to_string(), 5)]
        );
    }

    #[test]
    fn extends_and_implements_generics() {
        let code = "/**\n * @extends AbstractDtoComponent<ParamsDto, ResultDto, ViewModel>\n * @implements IteratorAggregate<int, ItemDto>\n */";
        assert_eq!(
            only_names(code),
            vec![
                "AbstractDtoComponent",
                "ItemDto",
                "IteratorAggregate",
                "ParamsDto",
                "ResultDto",
                "ViewModel"
            ]
        );
    }

    #[test]
    fn method_tag_takes_types_not_method_name() {
        let code = "/**\n * @method static EntityCollection GetList(array $filter, Query $q = null)\n * @method Foo make()\n * @method bar(Baz $b): Qux\n */";
        assert_eq!(
            only_names(code),
            vec!["Baz", "EntityCollection", "Foo", "Query", "Qux"]
        );
    }

    #[test]
    fn see_covers_and_inline_see() {
        let code = "/**\n * Delegates to {@see KeyedLocker} and {@link Other::run()}.\n * @see \\Orteka\\Foo::bar()\n * @see https://example.com/Docs\n * @covers \\Tests\\SubjectClass::method\n */";
        assert_eq!(
            only_names(code),
            vec![
                "Foo",
                "KeyedLocker",
                "Orteka",
                "Other",
                "SubjectClass",
                "Tests"
            ]
        );
    }

    #[test]
    fn template_bound_and_import_type() {
        let code = "/**\n * @template T of BaseDto\n * @template-covariant U as Item = DefaultItem\n * @phpstan-import-type Shape from ShapeHolder\n * @phpstan-type Alias = array{a: AliasTarget}\n */";
        assert_eq!(
            only_names(code),
            vec![
                "AliasTarget",
                "BaseDto",
                "DefaultItem",
                "Item",
                "ShapeHolder"
            ]
        );
    }

    #[test]
    fn inline_var_comment_and_single_star_comment() {
        let code = "<?php\n/** @var FooService $svc */\n$svc = $x;\n/* @var BarService $b */\n/* not a doc: CamelWord */\n";
        assert_eq!(
            names(code),
            vec![("BarService".to_string(), 4), ("FooService".to_string(), 2)]
        );
    }

    #[test]
    fn param_without_type_and_mid_line_at_sign() {
        let code =
            "/**\n * @param $x Prose Words Here\n * Contact admin@Example.com About Things\n */";
        assert!(names(code).is_empty());
    }

    #[test]
    fn class_constant_types_keep_class() {
        let code = "/** @param Status::ACTIVE|Status::* $s */";
        assert_eq!(only_names(code), vec!["Status"]);
    }

    #[test]
    fn defined_and_noise_names_are_filtered() {
        let symbols = vec![ParsedSymbol {
            name: "SelfDto".to_string(),
            kind: crate::db::SymbolKind::Class,
            line: 1,
            signature: String::new(),
            parents: vec![],
        }];
        let code = "/**\n * @var SelfDto[]\n * @throws Exception\n * @return OtherDto\n */";
        let refs: Vec<String> = extract_phpdoc_refs(code, &symbols, FileType::Php)
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(refs, vec!["OtherDto"]);
    }

    #[test]
    fn context_is_the_annotation_line() {
        let code = "<?php\n    /**\n     * @return ABTestSettingsDto[]\n     */\n";
        let refs = extract_phpdoc_refs(code, &[], FileType::Php);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].line, 3);
        assert_eq!(refs[0].context, "* @return ABTestSettingsDto[]");
    }

    #[test]
    fn code_lines_are_not_scanned() {
        let code = "<?php\n$a = '/* @var NotDoc */';\nfinal class Foo {}\n";
        assert!(names(code).is_empty());
    }
}
