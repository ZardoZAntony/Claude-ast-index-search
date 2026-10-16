use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git must run");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(repo: &Path, path: &str, content: &str) {
    let file = repo.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, content).unwrap();
}

fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", message]);
}

fn new_repo() -> TempDir {
    let temp = TempDir::new().unwrap();
    git(temp.path(), &["init", "-q", "-b", "main"]);
    git(temp.path(), &["config", "user.email", "test@example.invalid"]);
    git(temp.path(), &["config", "user.name", "Test"]);
    temp
}

fn changed_symbols(repo: &Path, args: &[&str]) -> Value {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_ast-index"))
        .current_dir(repo)
        .args(["changed", "--symbols", "--format", "json"])
        .args(args)
        .output()
        .expect("ast-index changed must run");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

fn class<'v>(result: &'v Value, fqn: &str) -> Option<&'v Value> {
    result["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["class"] == fqn)
}

const SERVICE_BEFORE: &str = r#"<?php
namespace App\File;

interface FileServiceInterface
{
    public function get(int $id): array;
}
"#;

const SERVICE_AFTER: &str = r#"<?php
namespace App\File;

/**
 * Files.
 */
interface FileServiceInterface
{
    /** By id. */
    public function get(
        int $id,
    ): array;

    public function getPath(int $fileId): string;
}
"#;

const CITY_BEFORE: &str = r#"<?php
namespace App\Geo;

final readonly class City
{
    public function __construct(
        private int $id,
        private string $name = '',
    ) {}

    public function getId(): int
    {
        return $this->id;
    }
}
"#;

const CITY_AFTER: &str = r#"<?php
namespace App\Geo;

final readonly class City
{
    public function __construct(
        private int $id,
        /** Presence city. */
        private bool $isPresence,
        private string $name = '',
    ) {}

    public function getId(): int
    {
        return $this->id + 0;
    }
}
"#;

#[test]
fn reports_api_changes_of_a_branch_against_its_merge_base() {
    let repo = new_repo();
    let root = repo.path();
    write(root, "src/File/FileServiceInterface.php", SERVICE_BEFORE);
    write(root, "src/Geo/City.php", CITY_BEFORE);
    write(root, "src/Old/Gone.php", "<?php\nnamespace App\\Old;\nclass Gone { public function run(): void {} }\n");
    commit_all(root, "base");
    git(root, &["checkout", "-qb", "feature"]);
    write(root, "src/File/FileServiceInterface.php", SERVICE_AFTER);
    write(root, "src/Geo/City.php", CITY_AFTER);
    fs::remove_file(root.join("src/Old/Gone.php")).unwrap();
    write(root, "src/New/Fresh.php", "<?php\nnamespace App\\New;\nclass Fresh { public function make(#[Attr([1, 2])] int $a, Foo $foo = new Foo()): void {} }\n");
    commit_all(root, "feature");

    let result = changed_symbols(root, &["--base", "main"]);

    let service = class(&result, "App\\File\\FileServiceInterface").expect("interface listed");
    assert_eq!(service["kind"], "interface");
    assert_eq!(service["change"], "changed");
    let methods = service["methods"].as_array().unwrap();
    assert_eq!(methods.len(), 1, "phpDoc and layout are no change: {methods:?}");
    assert_eq!(methods[0]["name"], "getPath");
    assert_eq!(methods[0]["change"], "added");
    assert_eq!(methods[0]["after"], "getPath(int $fileId): string");

    let city = class(&result, "App\\Geo\\City").expect("class listed");
    let ctor = &city["constructor"];
    assert_eq!(ctor["change"], "changed");
    assert_eq!(ctor["params"]["added"][0]["param"], "private bool $isPresence");
    assert_eq!(ctor["params"]["added"][0]["required"], true);
    let get_id = &city["methods"][0];
    assert_eq!(get_id["name"], "getId");
    assert_eq!(get_id["change"], "body");

    let gone = class(&result, "App\\Old\\Gone").expect("removed class listed");
    assert_eq!(gone["change"], "removed");
    assert_eq!(gone["methods"][0]["change"], "removed");

    let fresh = class(&result, "App\\New\\Fresh").expect("added class listed");
    assert_eq!(fresh["change"], "added");
    assert_eq!(fresh["methods"][0]["after"], "make(int $a, Foo $foo = new Foo()): void");
}

#[test]
fn head_flag_compares_another_revision_without_checking_it_out() {
    let repo = new_repo();
    let root = repo.path();
    write(root, "src/File/FileServiceInterface.php", SERVICE_BEFORE);
    commit_all(root, "base");
    git(root, &["checkout", "-qb", "feature"]);
    write(root, "src/File/FileServiceInterface.php", SERVICE_AFTER);
    commit_all(root, "feature");
    git(root, &["checkout", "-q", "main"]);

    let result = changed_symbols(root, &["--base", "main", "--head", "feature"]);

    assert_eq!(result["head"], "feature");
    let service = class(&result, "App\\File\\FileServiceInterface").expect("interface listed");
    assert_eq!(service["methods"][0]["name"], "getPath");
}

#[test]
fn renamed_file_with_new_namespace_is_a_move() {
    let repo = new_repo();
    let root = repo.path();
    let body = "class Adapter implements Port\n{\n    public function resize(array $ids): array\n    {\n        return $ids;\n    }\n\n    public function other(): void\n    {\n    }\n}\n";
    write(root, "src/Legacy/Adapter.php", &format!("<?php\nnamespace App\\Legacy;\n\n{body}"));
    commit_all(root, "base");
    git(root, &["checkout", "-qb", "feature"]);
    fs::remove_file(root.join("src/Legacy/Adapter.php")).unwrap();
    write(root, "src/Infra/Adapter.php", &format!("<?php\nnamespace App\\Infra;\n\n{body}"));
    commit_all(root, "feature");

    let result = changed_symbols(root, &["--base", "main"]);

    let moved = class(&result, "App\\Infra\\Adapter").expect("moved class listed");
    assert_eq!(moved["change"], "moved");
    assert_eq!(moved["moved_from"], "App\\Legacy\\Adapter");
    assert_eq!(moved["old_path"], "src/Legacy/Adapter.php");
    assert!(class(&result, "App\\Legacy\\Adapter").is_none());
}

#[test]
fn constants_and_enum_cases_are_listed() {
    let repo = new_repo();
    let root = repo.path();
    write(root, "src/Status.php", "<?php\nnamespace App;\n\nenum Status: string\n{\n    case Active = 'active';\n    case Old = 'old';\n}\n");
    commit_all(root, "base");
    git(root, &["checkout", "-qb", "feature"]);
    write(root, "src/Status.php", "<?php\nnamespace App;\n\nenum Status: string\n{\n    case Active = 'on';\n    case Paused = 'paused';\n}\n");
    commit_all(root, "feature");

    let result = changed_symbols(root, &["--base", "main"]);
    let status = class(&result, "App\\Status").expect("enum listed");
    let changes: Vec<(String, String)> = status["constants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["name"].as_str().unwrap().to_string(), c["change"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(
        changes,
        vec![
            ("Active".into(), "changed".into()),
            ("Paused".into(), "added".into()),
            ("Old".into(), "removed".into()),
        ]
    );
}
