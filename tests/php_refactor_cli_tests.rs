//! PHP refactoring commands on a small project: `usages <FQN>`, `impact`, `move-plan`,
//! FQN-aware `unused-symbols`, `duplicates`, and `callers Type::method`.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn run(cwd: &Path, cache: &Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_ast-index"))
        .current_dir(cwd)
        .args(args)
        .env("AST_INDEX_CACHE_DIR", cache)
        .env("AST_INDEX_DISABLE_GC", "1")
        .env_remove("AST_INDEX_DB_PATH")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

const DTO_BODY: &str =
    "{\n    public function __construct(public int $id, public string $name) {}\n}\n";

fn project() -> (TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();

    write(
        root,
        "src/Order/OrderDto.php",
        &format!("<?php\nnamespace App\\Order;\n\nfinal class OrderDto\n{DTO_BODY}"),
    );
    write(
        root,
        "src/Mobile/OrderDto.php",
        &format!("<?php\nnamespace App\\Mobile;\n\nfinal class OrderDto\n{DTO_BODY}"),
    );
    write(
        root,
        "src/Mobile/Unused.php",
        "<?php\nnamespace App\\Mobile;\n\nfinal class Unused {}\n",
    );
    write(root, "src/Order/Helper.php", "<?php\nnamespace App\\Order;\n\nfinal class Helper\n{\n    public static function make(): OrderDto { return new OrderDto(1, 'a'); }\n}\n");
    write(root, "src/Order/Service.php", "<?php\nnamespace App\\Order;\n\nfinal class Service\n{\n    public function one(): OrderDto\n    {\n        return Helper::make();\n    }\n}\n");
    write(root, "src/Api/Controller.php", "<?php\nnamespace App\\Api;\n\nuse App\\Order\\OrderDto;\n\nfinal class Controller\n{\n    /** @var OrderDto[] */\n    private array $items = [];\n\n    public function create(): OrderDto\n    {\n        $class = 'App\\\\Order\\\\OrderDto';\n        return new OrderDto(2, 'b');\n    }\n}\n");
    write(
        root,
        "config/services.yaml",
        "services:\n  App\\Order\\OrderDto: ~\n",
    );

    write(root, "src/Cache/InvalidatorInterface.php", "<?php\nnamespace App\\Cache;\n\ninterface InvalidatorInterface\n{\n    public function invalidate(string $type): void;\n}\n");
    write(root, "src/Cache/OrderInvalidator.php", "<?php\nnamespace App\\Cache;\n\nfinal class OrderInvalidator implements InvalidatorInterface\n{\n    public function invalidate(string $type): void {}\n}\n");
    write(root, "src/Cache/Processor.php", "<?php\nnamespace App\\Cache;\n\nfinal class Processor\n{\n    public function __construct(private InvalidatorInterface $invalidator) {}\n\n    public function run(): void\n    {\n        $this->invalidator->invalidate('order');\n    }\n}\n");
    write(root, "src/Cache/Bridge.php", "<?php\nnamespace App\\Cache;\n\nfinal class Bridge\n{\n    public function handle(): void\n    {\n        $this->invalidate(5);\n    }\n\n    private function invalidate(int $id): void {}\n}\n");
    write(root, "tests/OrderInvalidatorTest.php", "<?php\nnamespace Tests;\n\nuse App\\Cache\\OrderInvalidator;\n\nfinal class OrderInvalidatorTest\n{\n    public function testIt(): void\n    {\n        $invalidator = new OrderInvalidator();\n        $invalidator->invalidate('x');\n    }\n}\n");

    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

fn paths(items: &Value, key: &str) -> Vec<String> {
    let mut out: Vec<String> = items[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| format!("{}:{}", i["path"].as_str().unwrap(), i["line"]))
        .collect();
    out.sort();
    out
}

#[test]
fn usages_by_fqn_ignore_namesakes() {
    let (tmp, cache) = project();
    let v = run(
        tmp.path(),
        cache.path(),
        &["--format", "json", "usages", "App\\Mobile\\OrderDto"],
    );
    assert_eq!(v["pagination"]["total"], 0, "{v}");
    let v = run(
        tmp.path(),
        cache.path(),
        &["--format", "json", "usages", "App\\Order\\OrderDto"],
    );
    let found = paths(&v, "items");
    assert!(
        found.iter().all(|p| !p.starts_with("src/Mobile/")),
        "{found:?}"
    );
    assert!(
        found.contains(&"src/Order/Service.php:6".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&"src/Api/Controller.php:8".to_string()),
        "phpdoc: {found:?}"
    );
    assert!(
        found.contains(&"src/Api/Controller.php:13".to_string()),
        "string: {found:?}"
    );
}

#[test]
fn impact_lists_kinds_and_config_mentions() {
    let (tmp, cache) = project();
    let v = run(
        tmp.path(),
        cache.path(),
        &["--format", "json", "impact", "App\\Order\\OrderDto"],
    );
    let kinds: Vec<&str> = v["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    for kind in ["import", "phpdoc", "new", "string", "type"] {
        assert!(kinds.contains(&kind), "missing {kind}: {kinds:?}");
    }
    assert_eq!(v["definitions"][0]["path"], "src/Order/OrderDto.php");
    assert_eq!(v["config_mentions"][0]["path"], "config/services.yaml");
}

#[test]
fn move_plan_adds_imports_for_same_namespace_users() {
    let (tmp, cache) = project();
    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "move-plan",
            "App\\Order\\OrderDto",
            "App\\Order\\Dto",
        ],
    );
    let plan = serde_json::to_string(&v["files"]).unwrap();
    assert!(
        plan.contains("move file to src/Order/Dto/OrderDto.php"),
        "{plan}"
    );
    assert!(plan.contains("namespace App\\\\Order\\\\Dto;"), "{plan}");
    // Same-namespace users now need an import; the importer only changes its `use`.
    let service = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "src/Order/Service.php");
    assert!(service.is_some(), "{plan}");
    assert!(
        plan.contains("use App\\\\Order\\\\Dto\\\\OrderDto; (was: use App\\\\Order\\\\OrderDto;)"),
        "{plan}"
    );
    assert!(plan.contains("config/services.yaml"), "{plan}");
}

#[test]
fn unused_symbols_see_through_namesakes() {
    let (tmp, cache) = project();
    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "unused-symbols",
            "--module",
            "src/",
            "--limit",
            "500",
        ],
    );
    let names: Vec<String> = v
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["kind"] == "class")
        .map(|s| {
            format!(
                "{}:{}",
                s["path"].as_str().unwrap(),
                s["name"].as_str().unwrap()
            )
        })
        .collect();
    assert!(
        names.contains(&"src/Mobile/OrderDto.php:OrderDto".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"src/Mobile/Unused.php:Unused".to_string()),
        "{names:?}"
    );
    assert!(
        !names.contains(&"src/Order/OrderDto.php:OrderDto".to_string()),
        "{names:?}"
    );
}

#[test]
fn duplicates_report_copy_usage() {
    let (tmp, cache) = project();
    let v = run(
        tmp.path(),
        cache.path(),
        &["--format", "json", "duplicates"],
    );
    let pair = v["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "OrderDto")
        .expect("OrderDto pair");
    assert_eq!(pair["similarity"], 1.0);
    let refs: Vec<i64> = [&pair["a"], &pair["b"]]
        .iter()
        .map(|c| c["references"].as_i64().unwrap())
        .collect();
    assert!(refs.contains(&0) && refs.iter().any(|r| *r > 0), "{pair}");
}

#[test]
fn typed_callers_follow_receiver_types() {
    let (tmp, cache) = project();
    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "callers",
            "InvalidatorInterface::invalidate",
        ],
    );
    assert_eq!(
        paths(&v, "calls"),
        vec![
            "src/Cache/Processor.php:10".to_string(),
            "tests/OrderInvalidatorTest.php:11".to_string()
        ]
    );
    assert_eq!(
        paths(&v, "excluded"),
        vec!["src/Cache/Bridge.php:8".to_string()]
    );
    assert_eq!(v["declarations"].as_array().unwrap().len(), 2);
}

#[test]
fn unused_symbols_export_only_applies_to_module_scan() {
    let (tmp, cache) = project();
    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "unused-symbols",
            "--module",
            "src/",
            "--export-only",
            "--limit",
            "500",
        ],
    );
    let lowercase: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .filter(|n| n.starts_with(|c: char| c.is_ascii_lowercase()))
        .collect();
    assert!(lowercase.is_empty(), "{lowercase:?}");
}
