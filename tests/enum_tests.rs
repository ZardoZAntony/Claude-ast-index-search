//! `enum '<FQN>' --method …` and `usages '<FQN>::<CASE>'` on two enums sharing a short name.

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

const CATALOG_ENUM: &str = r#"<?php
namespace Catalog\Enum;

enum ProductCacheEnum: string
{
    private const HOUR = 60 * 60;

    case LIST = 'products_list';
    case HITS = 'hit_ids';
    case CARD = 'card';

    public function ttl(): int
    {
        return match ($this) {
            self::LIST,
            self::HITS => self::HOUR,
            default => 24 * self::HOUR,
        };
    }

    public function cacheDir(): string
    {
        return '/catalog/' . $this->value;
    }

    public function tagFor(string $code): string
    {
        return match ($this) {
            self::LIST => 'list_' . $code,
            default => 'other',
        };
    }
}
"#;

const MOBILE_ENUM: &str = r#"<?php
namespace Mobile\Enum;

enum ProductCacheEnum: string
{
    case LIST = 'mobile_list';
}
"#;

const USE_CASE: &str = r#"<?php
namespace Catalog\UseCase;

use Catalog\Enum\ProductCacheEnum;
use Mobile\Enum\ProductCacheEnum as MobileCache;

/**
 * Caches the list under {@see ProductCacheEnum::LIST}.
 */
final class ListUseCase
{
    public function run(): array
    {
        $key = ProductCacheEnum::LIST->value;
        $ttl = ProductCacheEnum::LIST
            ->ttl();
        $mobile = MobileCache::LIST;
        return [$key, $ttl, $mobile];
    }
}
"#;

fn project() -> (TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "src/Catalog/Enum/ProductCacheEnum.php", CATALOG_ENUM);
    write(root, "src/Mobile/Enum/ProductCacheEnum.php", MOBILE_ENUM);
    write(root, "src/Catalog/UseCase/ListUseCase.php", USE_CASE);
    write(
        root,
        "tests/ListUseCaseTest.php",
        "<?php\nnamespace Tests;\n\nuse Catalog\\Enum\\ProductCacheEnum;\n\nfinal class ListUseCaseTest\n{\n    public function testKey(): void\n    {\n        $case = ProductCacheEnum::LIST;\n    }\n}\n",
    );
    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

#[test]
fn enum_reports_cases_and_per_case_method_values() {
    let (tmp, cache) = project();
    let result = run(
        tmp.path(),
        cache.path(),
        &[
            "enum",
            "Catalog\\Enum\\ProductCacheEnum",
            "--method",
            "ttl",
            "--method",
            "cacheDir",
            "--method",
            "tagFor",
            "--format",
            "json",
        ],
    );
    assert_eq!(result["backed"], "string");
    let cases = result["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    let case = |name: &str| cases.iter().find(|c| c["name"] == name).unwrap();
    assert_eq!(case("LIST")["value"], "products_list");
    assert_eq!(case("LIST")["line"], 8);
    assert_eq!(case("LIST")["methods"]["ttl"]["value"], 3600);
    assert_eq!(case("LIST")["methods"]["ttl"]["line"], 16, "the line of the arm's value");
    assert_eq!(case("HITS")["methods"]["ttl"]["value"], 3600);
    assert_eq!(case("CARD")["methods"]["ttl"]["value"], 86400, "default arm");
    assert_eq!(case("CARD")["methods"]["cacheDir"]["value"], "/catalog/card");
    let tag = &case("LIST")["methods"]["tagFor"];
    assert!(tag["value"].is_null());
    assert_eq!(tag["expr"], "'list_' . $code");
}

#[test]
fn case_usages_follow_the_class_not_its_namesake() {
    let (tmp, cache) = project();
    let result = run(
        tmp.path(),
        cache.path(),
        &["usages", "Catalog\\Enum\\ProductCacheEnum::LIST", "--format", "json"],
    );
    assert_eq!(result["kind"], "case");
    assert_eq!(result["declared_at"], "src/Catalog/Enum/ProductCacheEnum.php:8");
    let places: Vec<(String, u64, bool, bool)> = result["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["path"].as_str().unwrap().to_string(),
                i["line"].as_u64().unwrap(),
                i["test"].as_bool().unwrap_or(false),
                i["self"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert_eq!(
        places,
        vec![
            ("src/Catalog/Enum/ProductCacheEnum.php".into(), 15, false, true),
            ("src/Catalog/Enum/ProductCacheEnum.php".into(), 29, false, true),
            ("src/Catalog/UseCase/ListUseCase.php".into(), 14, false, false),
            ("src/Catalog/UseCase/ListUseCase.php".into(), 15, false, false),
            ("tests/ListUseCaseTest.php".into(), 10, true, false),
        ]
    );
    assert_eq!(result["phpdoc"][0]["line"], 8);

    let no_tests = run(
        tmp.path(),
        cache.path(),
        &["usages", "Catalog\\Enum\\ProductCacheEnum::LIST", "--no-tests", "--format", "json"],
    );
    assert_eq!(no_tests["items"].as_array().unwrap().len(), 4);

    let mobile = run(
        tmp.path(),
        cache.path(),
        &["usages", "Mobile\\Enum\\ProductCacheEnum::LIST", "--format", "json"],
    );
    let mobile_items = mobile["items"].as_array().unwrap();
    assert_eq!(mobile_items.len(), 1);
    assert_eq!(mobile_items[0]["line"], 17);
}
