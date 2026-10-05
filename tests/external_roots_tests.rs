//! External directories (`external:` in .ast-index.yaml): framework core or vendor code indexed past .gitignore
//! for its definitions, kept out of search, usages, unused-symbols and duplicates unless `--external`.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn command(cwd: &Path, cache: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ast-index"))
        .current_dir(cwd)
        .args(args)
        .env("AST_INDEX_CACHE_DIR", cache)
        .env("AST_INDEX_DISABLE_GC", "1")
        .env("NO_COLOR", "1")
        .env_remove("AST_INDEX_DB_PATH")
        .env_remove("AST_INDEX_EXTERNAL")
        .env_remove("AST_INDEX_HAS_EXTERNAL")
        .output()
        .unwrap()
}

fn run(cwd: &Path, cache: &Path, args: &[&str]) -> Value {
    let out = command(cwd, cache, args);
    assert!(out.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

fn run_text(cwd: &Path, cache: &Path, args: &[&str]) -> String {
    let out = command(cwd, cache, args);
    assert!(out.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn project() -> (TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, ".gitignore", "/core/\n");
    write(root, ".ast-index.yaml", "external:\n  - core/lib\n");
    write(root, "core/lib/Web/ClientInterface.php", "<?php\nnamespace Core\\Web;\n\ninterface ClientInterface {}\n");
    write(
        root,
        "core/lib/Web/BaseClient.php",
        "<?php\nnamespace Core\\Web;\n\nabstract class BaseClient implements ClientInterface {}\n",
    );
    write(
        root,
        "core/lib/Web/HttpClient.php",
        "<?php\nnamespace Core\\Web;\n\nuse Core\\Orm\\DataManager;\n\nclass HttpClient extends BaseClient\n{\n    public function download(string $url): bool\n    {\n        return (bool)DataManager::class;\n    }\n}\n",
    );
    write(root, "core/lib/Orm/DataManager.php", "<?php\nnamespace Core\\Orm;\n\nabstract class DataManager {}\n");
    write(root, "core/lib/Orm/UserTable.php", "<?php\nnamespace Core\\Orm;\n\nclass UserTable extends DataManager {}\n");
    write(root, "core/other/NotListed.php", "<?php\nnamespace Core\\Other;\n\nclass NotListed {}\n");
    write(
        root,
        "src/Order/OrderTable.php",
        "<?php\nnamespace App\\Order;\n\nuse Core\\Orm\\DataManager;\n\nfinal class OrderTable extends DataManager {}\n",
    );
    write(
        root,
        "src/Api/Loader.php",
        "<?php\nnamespace App\\Api;\n\nuse Core\\Web\\HttpClient;\n\nfinal class Loader\n{\n    public function __construct(private HttpClient $client) {}\n\n    public function run(): void\n    {\n        $this->client->download('x');\n    }\n}\n",
    );
    write(
        root,
        "src/Api/RetryClient.php",
        "<?php\nnamespace App\\Api;\n\nuse Core\\Web\\HttpClient;\n\nfinal class RetryClient extends HttpClient {}\n",
    );
    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

#[test]
fn definitions_of_external_classes_are_found() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let class = run_text(root, cache.path(), &["class", "HttpClient"]);
    assert!(class.contains("core/lib/Web/HttpClient.php"), "{class}");
    let symbol = run_text(root, cache.path(), &["symbol", "DataManager"]);
    assert!(symbol.contains("core/lib/Orm/DataManager.php"), "{symbol}");
    assert!(!run_text(root, cache.path(), &["class", "NotListed"]).contains("NotListed.php"));

    let impact = run(root, cache.path(), &["impact", "Core\\Web\\HttpClient", "--format", "json"]);
    assert_eq!(impact["definitions"][0]["path"], "core/lib/Web/HttpClient.php");
    let paths: Vec<&str> = impact["references"].as_array().unwrap().iter().map(|r| r["path"].as_str().unwrap()).collect();
    assert!(paths.iter().all(|p| p.starts_with("src/")), "{paths:?}");

    let stats = run_text(root, cache.path(), &["stats"]);
    assert!(stats.contains("External:   5 files"), "{stats}");
}

#[test]
fn external_code_stays_out_of_search_usages_and_unused() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let search = run_text(root, cache.path(), &["search", "DataManager"]);
    assert!(!search.contains("core/lib"), "{search}");
    assert!(search.contains("src/Order/OrderTable.php"), "{search}");
    let usages = run_text(root, cache.path(), &["usages", "DataManager"]);
    assert!(!usages.contains("core/lib"), "{usages}");
    let with_external = run_text(root, cache.path(), &["usages", "DataManager", "--external"]);
    assert!(with_external.contains("core/lib/Orm/UserTable.php"), "{with_external}");

    let unused = run_text(root, cache.path(), &["unused-symbols", "--limit", "500"]);
    assert!(!unused.contains("core/lib"), "{unused}");
}

#[test]
fn implementations_go_through_external_classes_and_hide_them() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let orm = run(root, cache.path(), &["implementations", "Core\\Orm\\DataManager", "--format", "json"]);
    let fqns: Vec<&str> = orm["items"].as_array().unwrap().iter().map(|i| i["fqn"].as_str().unwrap()).collect();
    assert_eq!(fqns, vec!["App\\Order\\OrderTable"]);
    assert_eq!(orm["external_hidden"], 1);

    // RetryClient → HttpClient → BaseClient → ClientInterface: the chain runs through external classes
    let client = run(root, cache.path(), &["implementations", "Core\\Web\\ClientInterface", "--format", "json"]);
    let fqns: Vec<&str> = client["items"].as_array().unwrap().iter().map(|i| i["fqn"].as_str().unwrap()).collect();
    assert_eq!(fqns, vec!["App\\Api\\RetryClient"]);

    let all = run(root, cache.path(), &["implementations", "Core\\Orm\\DataManager", "--external", "--format", "json"]);
    assert_eq!(all["items"].as_array().unwrap().len(), 2);
}

#[test]
fn callers_show_the_external_declaration_and_project_calls() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let callers = run_text(root, cache.path(), &["callers", "Core\\Web\\HttpClient::download"]);
    assert!(callers.contains("core/lib/Web/HttpClient.php"), "{callers}");
    assert!(callers.contains("src/Api/Loader.php"), "{callers}");
}

#[test]
fn update_follows_the_config() {
    let (tmp, cache) = project();
    let root = tmp.path();
    write(root, "core/lib/Web/Cookie.php", "<?php\nnamespace Core\\Web;\n\nfinal class Cookie {}\n");
    run(root, cache.path(), &["update"]);
    assert!(run_text(root, cache.path(), &["class", "Cookie"]).contains("core/lib/Web/Cookie.php"));

    // a directory dropped from `external:` leaves the index (it is gitignored)
    write(root, ".ast-index.yaml", "external: []\n");
    run(root, cache.path(), &["update"]);
    assert!(!run_text(root, cache.path(), &["class", "HttpClient"]).contains("core/lib"));

    // a directory that is not gitignored switches between project and external code on update
    write(root, ".gitignore", "");
    run(root, cache.path(), &["update"]);
    assert!(run_text(root, cache.path(), &["search", "UserTable"]).contains("core/lib/Orm/UserTable.php"));
    write(root, ".ast-index.yaml", "external:\n  - core/lib\n");
    run(root, cache.path(), &["update"]);
    assert!(!run_text(root, cache.path(), &["search", "UserTable"]).contains("core/lib/Orm/UserTable.php"));
    assert!(run_text(root, cache.path(), &["class", "UserTable"]).contains("core/lib/Orm/UserTable.php"));
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Main worktree with the external core next to its sources, and a linked worktree made by `git worktree add`:
/// of the core it has only the file the repository keeps.
fn worktrees() -> (TempDir, TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let main = tmp.path().join("main");
    write(&main, ".gitignore", "/core/\n");
    write(&main, ".ast-index.yaml", "external:\n  - core/lib\n");
    write(&main, "core/lib/Web/HttpClient.php", "<?php\nnamespace Core\\Web;\n\nclass HttpClient {}\n");
    write(&main, "core/lib/Web/Patched.php", "<?php\nnamespace Core\\Web;\n\nclass Patched {}\n");
    write(
        &main,
        "src/Api/RetryClient.php",
        "<?php\nnamespace App\\Api;\n\nuse Core\\Web\\HttpClient;\n\nfinal class RetryClient extends HttpClient {}\n",
    );
    git(&main, &["init", "-q"]);
    git(&main, &["add", ".gitignore", ".ast-index.yaml", "src"]);
    git(&main, &["add", "--force", "core/lib/Web/Patched.php"]);
    git(&main, &["commit", "-qm", "init"]);
    let linked = tmp.path().join("linked");
    git(&main, &["worktree", "add", "-q", "-b", "feature", linked.to_str().unwrap()]);
    (tmp, cache, main.canonicalize().unwrap(), linked)
}

#[test]
fn linked_worktree_reads_external_code_from_the_main_worktree() {
    let (_tmp, cache, main, linked) = worktrees();
    run(&linked, cache.path(), &["rebuild"]);

    let main_path = main.join("core/lib/Web/HttpClient.php").display().to_string();
    let class = run_text(&linked, cache.path(), &["class", "HttpClient"]);
    assert!(class.contains(&main_path), "{class}");
    let impact = run(&linked, cache.path(), &["impact", "Core\\Web\\HttpClient", "--format", "json"]);
    assert_eq!(impact["definitions"][0]["path"], main_path.as_str());
    let references: Vec<&str> =
        impact["references"].as_array().unwrap().iter().map(|r| r["path"].as_str().unwrap()).collect();
    // `use` and `extends` in the worktree's own code, with paths relative to it
    assert!(!references.is_empty(), "{impact}");
    assert!(references.iter().all(|path| *path == "src/Api/RetryClient.php"), "{references:?}");

    // an unchanged update keeps the external files of the main worktree instead of deleting them
    run(&linked, cache.path(), &["update"]);
    assert!(run_text(&linked, cache.path(), &["class", "HttpClient"]).contains(&main_path));
}

#[test]
fn linked_worktree_with_its_own_installed_copy_reads_it() {
    let (_tmp, cache, main, linked) = worktrees();
    // dependencies installed in the linked worktree itself: files git does not track
    write(&linked, "core/lib/Web/HttpClient.php", "<?php\nnamespace Core\\Web;\n\nclass HttpClient {}\n");
    run(&linked, cache.path(), &["rebuild"]);

    let class = run_text(&linked, cache.path(), &["class", "HttpClient"]);
    assert!(class.contains("core/lib/Web/HttpClient.php"), "{class}");
    assert!(!class.contains(&main.display().to_string()), "{class}");
}
