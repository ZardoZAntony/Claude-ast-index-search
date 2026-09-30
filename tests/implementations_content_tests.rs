use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn run(root: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ast-index"))
        .current_dir(root)
        .env("AST_INDEX_CACHE_DIR", cache)
        .env("AST_INDEX_DISABLE_GC", "1")
        .env("NO_COLOR", "1")
        .env_remove("AST_INDEX_DB_PATH")
        .env_remove("KOTLIN_INDEX_DB_PATH")
        .args(args)
        .output()
        .unwrap()
}

fn fixture() -> (TempDir, TempDir) {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname=\"content-fixture\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        r#"pub trait Processor {
    fn process(&self) -> usize;
}

pub struct ConcreteProcessor;

impl Processor for ConcreteProcessor {
    fn process(&self) -> usize {
        let implementation_secret = 42;
        implementation_secret
    }
}
"#,
    )
    .unwrap();

    let rebuild = run(project.path(), cache.path(), &["rebuild"]);
    assert!(
        rebuild.status.success(),
        "rebuild failed: {}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
    (project, cache)
}

#[test]
fn index_stores_ranges_and_reads_current_file_content() {
    let (project, cache) = fixture();
    let db_path = fs::read_dir(cache.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("index.db"))
        .find(|path| path.exists())
        .unwrap();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let range: (i64, i64) = conn
        .query_row(
            "SELECT line, end_line FROM symbols WHERE name = 'impl Processor for ConcreteProcessor'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(range, (7, 12));
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('symbols')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!columns.iter().any(|column| column == "content"));

    let path = project.path().join("src/lib.rs");
    let original = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        original.replace("implementation_secret", "current_file_value"),
    )
    .unwrap();
    let output = run(
        project.path(),
        cache.path(),
        &["implementations", "Processor", "--with-content"],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("current_file_value"));
    assert!(!stdout.contains("implementation_secret"));
}

#[test]
fn with_content_stops_at_python_dedent() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    fs::write(
        project.path().join("example.py"),
        "def first():\n    value = 42\n    return value\n\ndef second():\n    return 99\n",
    )
    .unwrap();
    assert!(run(project.path(), cache.path(), &["rebuild"])
        .status
        .success());
    let output = run(
        project.path(),
        cache.path(),
        &["symbol", "first", "--with-content"],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("return value"));
    assert!(!stdout.contains("def second"));
}

#[test]
fn with_content_prints_implementation_body_only_when_requested() {
    let (project, cache) = fixture();

    let plain = run(
        project.path(),
        cache.path(),
        &["implementations", "Processor"],
    );
    assert!(plain.status.success());
    let plain_stdout = String::from_utf8_lossy(&plain.stdout);
    assert!(plain_stdout.contains("impl Processor for ConcreteProcessor"));
    assert!(!plain_stdout.contains("implementation_secret"));

    let with_content = run(
        project.path(),
        cache.path(),
        &["implementations", "--with-content", "Processor"],
    );
    assert!(with_content.status.success());
    let stdout = String::from_utf8_lossy(&with_content.stdout);
    assert!(stdout.contains("impl Processor for ConcreteProcessor"));
    assert!(stdout.contains("let implementation_secret = 42;"));
    assert!(stdout.contains("implementation_secret\n"));
}

#[test]
fn with_content_adds_content_to_json_items() {
    let (project, cache) = fixture();
    let plain = run(
        project.path(),
        cache.path(),
        &["--format", "json", "implementations", "Processor"],
    );
    assert!(plain.status.success());
    let plain_value: Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert!(plain_value["items"][0].get("content").is_none());

    let output = run(
        project.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "implementations",
            "Processor",
            "--with-content",
        ],
    );
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 2);
    let items = value["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let content = items[0]["content"].as_str().unwrap();
    assert!(
        content.starts_with("    7\timpl Processor for ConcreteProcessor {"),
        "{content:?}"
    );
    assert!(content.contains("let implementation_secret = 42;"));
    assert!(content.ends_with("   12\t}\n"));
}

#[test]
fn symbol_with_content_prints_matched_method_body() {
    let (project, cache) = fixture();

    let plain = run(project.path(), cache.path(), &["symbol", "process"]);
    assert!(plain.status.success());
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("implementation_secret"));

    let output = run(
        project.path(),
        cache.path(),
        &["symbol", "process", "--with-content"],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn process(&self) -> usize {"));
    assert!(stdout.contains("let implementation_secret = 42;"));
}

#[test]
fn symbol_with_content_adds_content_to_json_items() {
    let (project, cache) = fixture();

    let plain = run(
        project.path(),
        cache.path(),
        &["--format", "json", "symbol", "process"],
    );
    assert!(plain.status.success());
    let plain_value: Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert!(plain_value["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item.get("content").is_none()));

    let output = run(
        project.path(),
        cache.path(),
        &["--format", "json", "symbol", "process", "--with-content"],
    );
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["content"].as_str())
        .any(|content| content.contains("let implementation_secret = 42;")));
}

#[test]
fn search_with_content_prints_matched_symbol_body() {
    let (project, cache) = fixture();

    let plain = run(project.path(), cache.path(), &["search", "process"]);
    assert!(plain.status.success());
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("implementation_secret"));

    let output = run(
        project.path(),
        cache.path(),
        &["search", "process", "--with-content"],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Symbols"));
    assert!(stdout.contains("let implementation_secret = 42;"));
}

#[test]
fn search_with_content_adds_content_to_json_symbols() {
    let (project, cache) = fixture();

    let plain = run(
        project.path(),
        cache.path(),
        &["--format", "json", "search", "process"],
    );
    assert!(plain.status.success());
    let plain_value: Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert!(plain_value["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item.get("content").is_none()));

    let output = run(
        project.path(),
        cache.path(),
        &["--format", "json", "search", "process", "--with-content"],
    );
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["content"].as_str())
        .any(|content| content.contains("let implementation_secret = 42;")));
}

#[test]
fn long_body_reports_truncation_and_indexed_end_line() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let mut source = String::from("pub fn long_body() {\n");
    for n in 0..75 {
        source.push_str(&format!("    let _value_{n} = {n};\n"));
    }
    source.push_str("}\n");
    fs::write(project.path().join("sample.rs"), source).unwrap();
    assert!(run(project.path(), cache.path(), &["rebuild"])
        .status
        .success());

    let output = run(
        project.path(),
        cache.path(),
        &["--format", "json", "symbol", "long_body", "--with-content"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let item = &value["items"][0];
    assert_eq!(item["end_line"], 77);
    assert_eq!(item["truncated"], true);
    let content = item["content"].as_str().unwrap();
    assert!(content.contains("   60\t"));
    assert!(!content.contains("   61\t"));

    let output = run(
        project.path(),
        cache.path(),
        &["symbol", "long_body", "--with-content"],
    );
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("truncated at line 60; symbol ends at line 77"));

    let output = run(
        project.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "search",
            "long_body",
            "--rank",
            "central",
            "--with-content",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let symbol = value["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "long_body")
        .unwrap();
    assert_eq!(symbol["end_line"], 77);
    assert_eq!(symbol["truncated"], true);
}
