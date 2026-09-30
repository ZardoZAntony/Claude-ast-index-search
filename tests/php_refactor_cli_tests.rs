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

#[test]
fn implementations_accept_fqn() {
    let (tmp, cache) = project();
    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "implementations",
            "App\\Cache\\InvalidatorInterface",
        ],
    );
    let fqns: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["fqn"].as_str().unwrap())
        .collect();
    assert_eq!(fqns, vec!["App\\Cache\\OrderInvalidator"]);
}

fn run_text(cwd: &Path, cache: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_ast-index"))
        .current_dir(cwd)
        .args(args)
        .env("AST_INDEX_CACHE_DIR", cache)
        .env("AST_INDEX_DISABLE_GC", "1")
        .env("NO_COLOR", "1")
        .env_remove("AST_INDEX_DB_PATH")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Aliased imports, traits, supertypes, long classes and DI wiring.
fn project2() -> (TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();

    write(
        root,
        ".ast-index.yaml",
        "unused_ignore:\n  - \"**/install/index.php\"\nunused_ignore_names:\n  - \"*Action\"\n",
    );
    write(root, "src/Base/Version.php", "<?php\nnamespace App\\Base;\n\nabstract class OrtekaVersion\n{\n    abstract public function up(): void;\n}\n");
    write(root, "migrations/V1.php", "<?php\nnamespace Migrations;\n\nuse App\\Base\\OrtekaVersion as Version;\n\nfinal class V1 extends Version\n{\n    public function up(): void {}\n}\n");
    write(root, "migrations/V2.php", "<?php\nnamespace Migrations;\n\nuse App\\Base\\OrtekaVersion as Version;\n\nfinal class V2 extends Version\n{\n    public function up(): void {}\n}\n");

    write(root, "src/Model/BaseTrait.php", "<?php\nnamespace App\\Model;\n\ntrait BaseTrait\n{\n    public function id(): int { return 1; }\n}\n");
    write(
        root,
        "src/Model/Entity.php",
        "<?php\nnamespace App\\Model;\n\nfinal class Entity\n{\n    use BaseTrait;\n}\n",
    );
    write(
        root,
        "src/Model/Sub/Entity.php",
        "<?php\nnamespace App\\Model\\Sub;\n\nfinal class Entity {}\n",
    );

    // A long class: the method is far below the class line.
    let filler: String = (0..450).map(|i| format!("    // filler {i}\n")).collect();
    write(root, "src/Cache/CacheManagerInterface.php", "<?php\nnamespace App\\Cache;\n\ninterface CacheManagerInterface\n{\n    public function remember(string $key): mixed;\n}\n");
    write(root, "src/Cache/CacheManager.php", &format!("<?php\nnamespace App\\Cache;\n\nclass CacheManager implements CacheManagerInterface\n{{\n{filler}    public function remember(string $key): mixed {{ return null; }}\n}}\n"));
    write(root, "src/Cache/TaggedCacheManager.php", "<?php\nnamespace App\\Cache;\n\nfinal class TaggedCacheManager extends CacheManager\n{\n    public function remember(string $key): mixed\n    {\n        return parent::remember($key);\n    }\n}\n");
    write(root, "src/Cache/Consumer.php", "<?php\nnamespace App\\Cache;\n\nfinal class Consumer\n{\n    public function __construct(\n        private CacheManagerInterface $cache,\n        private CacheManager $manager,\n    ) {}\n\n    public function a(): void\n    {\n        $this->cache->remember('a');\n        // $this->manager->remember('in a comment');\n        $this->manager->remember('b'); $this->manager->remember('c');\n        usort($list, function ($x) { return $this->manager->remember('d'); });\n    }\n\n    public function b(CacheManager $m): void\n    {\n        $m = $this->other();\n        $m->remember('e');\n    }\n}\n");

    write(root, "src/Di/Registered.php", "<?php\nnamespace App\\Di;\n\nfinal class Registered\n{\n    public function __construct() {}\n}\n");
    write(
        root,
        "src/Di/Requested.php",
        "<?php\nnamespace App\\Di;\n\nfinal class Requested\n{\n    public function listAction(): array { return []; }\n}\n",
    );
    write(root, "src/di/services.php", "<?php\nuse App\\Di\\Registered;\nuse App\\Di\\Requested;\n\nreturn [\n    Registered::class => ['className' => Registered::class],\n    Requested::class => [\n        'constructor' => static fn() => new Requested(),\n    ],\n];\n");
    write(
        root,
        "src/install/index.php",
        "<?php\nfinal class ModuleInstaller\n{\n    public function DoInstall(): void {}\n}\n",
    );

    write(root, "src/Dup/A/Same.php", "<?php\nnamespace App\\Dup;\n\nfinal class Same\n{\n    public function one(): int { return 1; }\n    public function two(): int { return 2; }\n}\n");
    write(root, "src/Dup/B/Same.php", "<?php\nnamespace App\\Dup;\n\nfinal class Same\n{\n    public function one(): int { return 1; }\n    public function two(): int { return 2; }\n}\n");

    let many: String = (0..70)
        .map(|i| format!("        $a{i} = new Target();\n"))
        .collect();
    write(
        root,
        "src/Many/Target.php",
        "<?php\nnamespace App\\Many;\n\nfinal class Target {}\n",
    );
    write(root, "src/Many/User.php", &format!("<?php\nnamespace App\\Many;\n\nfinal class User\n{{\n    public function run(): void\n    {{\n{many}    }}\n}}\n"));

    write(
        root,
        "src/Orm/ThingCollection.php",
        "<?php\nnamespace App\\Orm;\n\nuse App\\Orm\\Table\\EO_Thing_Collection;\n\nfinal class ThingCollection extends EO_Thing_Collection {}\n",
    );

    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

#[test]
fn implementations_of_a_type_outside_the_index() {
    let (tmp, cache) = project2();
    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "implementations",
            "App\\Orm\\Table\\EO_Thing_Collection",
        ],
    );
    let fqns: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["fqn"].as_str().unwrap())
        .collect();
    assert_eq!(fqns, vec!["App\\Orm\\ThingCollection"]);
}

#[test]
fn implementations_follow_import_aliases() {
    let (tmp, cache) = project2();
    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "implementations",
            "App\\Base\\OrtekaVersion",
        ],
    );
    let mut fqns: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["fqn"].as_str().unwrap())
        .collect();
    fqns.sort();
    assert_eq!(fqns, vec!["Migrations\\V1", "Migrations\\V2"]);
}

#[test]
fn alias_import_line_resolves_to_the_class() {
    let (tmp, cache) = project2();
    let v = run(
        tmp.path(),
        cache.path(),
        &["--format", "json", "impact", "App\\Base\\OrtekaVersion"],
    );
    let lines: Vec<String> = v["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{}:{}:{}",
                r["path"].as_str().unwrap(),
                r["line"],
                r["kind"].as_str().unwrap()
            )
        })
        .collect();
    assert!(
        lines.contains(&"migrations/V1.php:4:import".to_string()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"migrations/V1.php:6:inheritance".to_string()),
        "{lines:?}"
    );
}

#[test]
fn move_plan_imports_a_same_namespace_trait_and_warns_on_collision() {
    let (tmp, cache) = project2();
    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "move-plan",
            "App\\Model\\Entity",
            "App\\Model\\Sub",
        ],
    );
    let plan = serde_json::to_string(&v["files"]).unwrap();
    assert!(
        plan.contains("add `use App\\\\Model\\\\BaseTrait;`"),
        "{plan}"
    );
    let warnings = serde_json::to_string(&v["warnings"]).unwrap();
    assert!(
        warnings.contains("already exists at src/Model/Sub/Entity.php"),
        "{warnings}"
    );
}

#[test]
fn callers_find_far_declarations_supertype_calls_and_rebinding() {
    let (tmp, cache) = project2();
    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "callers",
            "App\\Cache\\CacheManager::remember",
        ],
    );
    let decls: Vec<String> = v["declarations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| format!("{}:{}", d["path"].as_str().unwrap(), d["line"]))
        .collect();
    assert!(
        decls.contains(&"src/Cache/CacheManager.php:456".to_string()),
        "{decls:?}"
    );
    assert!(
        decls.contains(&"src/Cache/TaggedCacheManager.php:6".to_string()),
        "{decls:?}"
    );
    assert_eq!(
        paths(&v, "calls"),
        vec![
            "src/Cache/Consumer.php:15".to_string(),
            "src/Cache/Consumer.php:15".to_string(),
            "src/Cache/Consumer.php:16".to_string(),
            "src/Cache/TaggedCacheManager.php:8".to_string(),
        ]
    );
    assert_eq!(
        paths(&v, "via_supertype"),
        vec!["src/Cache/Consumer.php:13".to_string()]
    );
    assert_eq!(
        paths(&v, "unresolved"),
        vec!["src/Cache/Consumer.php:22".to_string()]
    );
}

#[test]
fn impact_summarizes_many_references_per_file() {
    let (tmp, cache) = project2();
    let text = run_text(tmp.path(), cache.path(), &["impact", "App\\Many\\Target"]);
    assert!(text.contains("per file"), "{text}");
    assert!(text.contains("src/Many/User.php  new×70"), "{text}");
    assert!(!text.contains("$a42 = new Target"), "{text}");
    let full = run_text(
        tmp.path(),
        cache.path(),
        &["impact", "App\\Many\\Target", "--full"],
    );
    assert!(full.contains("$a42 = new Target"), "{full}");
}

#[test]
fn duplicates_flag_copies_with_the_same_fqn() {
    let (tmp, cache) = project2();
    let v = run(
        tmp.path(),
        cache.path(),
        &["--format", "json", "duplicates"],
    );
    let pair = v["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Same")
        .expect("Same pair");
    assert_eq!(pair["same_fqn"], true);
}

#[test]
fn unused_symbols_skip_magic_ignored_files_and_report_di_only() {
    let (tmp, cache) = project2();
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
    let rows = v.as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(!names.contains(&"__construct"), "{names:?}");
    assert!(!names.contains(&"DoInstall"), "{names:?}");
    assert!(!names.contains(&"ModuleInstaller"), "{names:?}");
    assert!(!names.contains(&"Requested"), "{names:?}");
    assert!(!names.contains(&"listAction"), "{names:?}");
    let registered = rows
        .iter()
        .find(|s| s["name"] == "Registered")
        .expect("Registered reported");
    assert_eq!(registered["reason"], "registered in DI only");
}

#[test]
fn short_name_usages_warn_about_namesakes() {
    let (tmp, cache) = project();
    let text = run_text(tmp.path(), cache.path(), &["usages", "OrderDto"]);
    assert!(text.contains("2 classes are named OrderDto"), "{text}");
}

/// The second review's cases: a class referencing itself, calls on one line with a `foreach`,
/// method names in another letter case, a root `di/`, callables with `$this`, a template with
/// `?>` in a one-line comment.
fn project3() -> (TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();

    write(root, "src/RunnerInterface.php", "<?php\nnamespace Fx\\Calls;\ninterface RunnerInterface { public function run(int $n): void; }\n");
    write(root, "src/Helper.php", "<?php\nnamespace Fx\\Calls;\nfinal class Helper { public function run(int $n): void {} }\n");
    write(root, "src/AbstractRunner.php", "<?php\nnamespace Fx\\Calls;\nabstract class AbstractRunner implements RunnerInterface\n{\n    public function run(int $n): void {}\n    public function GetList(): array { return []; }\n}\n");
    write(root, "src/Runner.php", "<?php\nnamespace Fx\\Calls;\n\nfinal class Runner extends AbstractRunner\n{\n    public function __construct(private RunnerInterface $inner, private Helper $helper) {}\n\n    public function go(Helper $h): void\n    {\n        $this->inner->run(1);\n        $this->run(2); $this->helper->run(3);\n        parent::run(5);\n        foreach ($this->all() as $h) { $h->run(6); }\n        $r = new Runner($this->inner, $h);\n        $r->run(7);\n        $this->getList();\n        $handlers = [[$this, 'onEvent'], [self::class, 'onStatic']];\n    }\n    public static function make(): self { return new Runner(new Helper(), new Helper()); }\n    public function onEvent(): void {}\n    public function onStatic(): void {}\n    private function all(): array { return []; }\n}\n");
    write(
        root,
        "src/OnlyRegistered.php",
        "<?php\nnamespace Fx\\Calls;\nfinal class OnlyRegistered {}\n",
    );
    write(root, "di/Services.php", "<?php\nuse Fx\\Calls\\OnlyRegistered;\nreturn [\n    OnlyRegistered::class => ['className' => OnlyRegistered::class],\n];\n");
    write(
        root,
        "src/Shapes.php",
        "<?php\nnamespace Fx\\Calls;\n\ninterface Shape {}\n\nfinal class Square implements Shape\n{\n    public static function unit(): Square { return new Square(); }\n}\n",
    );
    write(
        root,
        "src/Tail.php",
        "<?php\nnamespace Fx\\Calls;\n\nenum Suit\n{\n    case Hearts;\n}\n\nfinal class Deck {}\n\n$x = Suit::Hearts;\n$d = new Deck();\n",
    );
    write(root, "tpl/template.php", "<?php $a = new Real(); ?>\n<div><?//= Loc::getMessage('X') ?></div>\n<p>Don't Panic Ghost</p>\n<?php $b = new Second(); ?>\n");

    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

#[test]
fn impact_lists_references_from_the_class_own_file() {
    let (tmp, cache) = project3();
    let v = run(
        tmp.path(),
        cache.path(),
        &["--format", "json", "impact", "Fx\\Calls\\Runner"],
    );
    let lines: Vec<String> = v["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| format!("{}:{}", r["path"].as_str().unwrap(), r["line"]))
        .collect();
    assert_eq!(lines, vec!["src/Runner.php:14", "src/Runner.php:19"]);
}

#[test]
fn callers_handle_own_class_foreach_on_the_line_and_letter_case() {
    let (tmp, cache) = project3();
    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "callers",
            "Fx\\Calls\\RunnerInterface::run",
        ],
    );
    assert_eq!(
        paths(&v, "calls"),
        vec![
            "src/Runner.php:10".to_string(),
            "src/Runner.php:11".to_string(),
            "src/Runner.php:12".to_string(),
            "src/Runner.php:15".to_string(),
        ]
    );
    assert_eq!(
        paths(&v, "unresolved"),
        vec!["src/Runner.php:13".to_string()]
    );
    assert_eq!(paths(&v, "excluded"), vec!["src/Runner.php:11".to_string()]);

    let v = run(
        tmp.path(),
        cache.path(),
        &[
            "--format",
            "json",
            "callers",
            "Fx\\Calls\\AbstractRunner::GetList",
        ],
    );
    assert_eq!(paths(&v, "calls"), vec!["src/Runner.php:16".to_string()]);
}

#[test]
fn unused_symbols_see_root_di_callables_and_letter_case() {
    let (tmp, cache) = project3();
    let v = run(
        tmp.path(),
        cache.path(),
        &["--format", "json", "unused-symbols", "--limit", "500"],
    );
    let rows = v.as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|s| s["name"].as_str().unwrap()).collect();
    // Suit and Deck are used by code after the last class of their file.
    for used in ["onEvent", "onStatic", "GetList", "Shape", "Suit", "Deck"] {
        assert!(!names.contains(&used), "{used}: {names:?}");
    }
    // Square references only itself.
    assert!(names.contains(&"Square"), "{names:?}");
    let registered = rows
        .iter()
        .find(|s| s["name"] == "OnlyRegistered")
        .expect("OnlyRegistered reported");
    assert_eq!(registered["reason"], "registered in DI only");
}

#[test]
fn closing_tag_in_a_template_comment_keeps_html_out_of_code() {
    let (tmp, cache) = project3();
    let v = run(
        tmp.path(),
        cache.path(),
        &["--format", "json", "impact", "Second"],
    );
    assert_eq!(v["summary"]["references"], 1, "{v}");
    let v = run(
        tmp.path(),
        cache.path(),
        &["--format", "json", "impact", "Don"],
    );
    assert_eq!(v["summary"]["references"], 0, "{v}");
}

fn run_failing(cwd: &Path, cache: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_ast-index"))
        .current_dir(cwd)
        .args(args)
        .env("AST_INDEX_CACHE_DIR", cache)
        .env("AST_INDEX_DISABLE_GC", "1")
        .env("NO_COLOR", "1")
        .env_remove("AST_INDEX_DB_PATH")
        .output()
        .unwrap();
    assert!(!out.status.success(), "{args:?} should fail");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A guessed namespace gets the classes of the same short name instead of an empty answer.
#[test]
fn unknown_fqn_suggests_classes_of_the_same_name() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let expected = ["App\\Mobile\\OrderDto", "App\\Order\\OrderDto"];

    let usages = run(root, cache.path(), &["usages", "App\\Wrong\\OrderDto", "--format", "json"]);
    assert_eq!(usages["did_you_mean"], serde_json::json!(expected));
    let text = run_text(root, cache.path(), &["usages", "App\\Wrong\\OrderDto"]);
    assert!(text.contains("classes named OrderDto: App\\Mobile\\OrderDto, App\\Order\\OrderDto"), "{text}");

    let impact = run(root, cache.path(), &["impact", "App\\Wrong\\OrderDto", "--format", "json"]);
    assert_eq!(impact["did_you_mean"], serde_json::json!(expected));

    let callers = run_failing(root, cache.path(), &["callers", "App\\Wrong\\OrderInvalidator::invalidate"]);
    assert!(callers.contains("classes named OrderInvalidator: App\\Cache\\OrderInvalidator"), "{callers}");
    let moved = run_failing(root, cache.path(), &["move-plan", "App\\Wrong\\Helper", "App\\Util"]);
    assert!(moved.contains("classes named Helper: App\\Order\\Helper"), "{moved}");

    // a known FQN keeps the plain answer
    let known = run(root, cache.path(), &["usages", "App\\Order\\OrderDto", "--format", "json"]);
    assert_eq!(known["did_you_mean"], serde_json::json!([]));
}
