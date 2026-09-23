//! `rebuild --type modules|deps|files` refreshes only its own tables; every
//! other table must survive into the published generation.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn run(root: &Path, args: &[&str]) -> String {
    run_full(root, args).0
}

/// (stdout, stderr) of a successful invocation.
fn run_full(root: &Path, args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ast-index"))
        .current_dir(root)
        .args(args)
        .env("AST_INDEX_CACHE_DIR", root.parent().unwrap().join("cache"))
        .env("AST_INDEX_DISABLE_GC", "1")
        .env_remove("AST_INDEX_DB_PATH")
        .env_remove("KOTLIN_INDEX_DB_PATH")
        .env_remove("AST_INDEX_MAX_FILES")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn count(root: &Path, table: &str) -> String {
    let json = run(root, &["query", &format!("SELECT COUNT(*) AS c FROM {table}")]);
    json.split("\"c\":").nth(1).unwrap().trim().split(|c: char| !c.is_ascii_digit()).next().unwrap().to_string()
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn partial_rebuilds_keep_symbols_and_modules() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("project");
    write(&root, "settings.gradle.kts", "include(\":app\", \":core\")\n");
    write(&root, "core/build.gradle.kts", "");
    write(
        &root,
        "app/build.gradle.kts",
        "dependencies { implementation(project(\":core\")) }\n",
    );
    write(&root, "core/src/main/kotlin/Repo.kt", "interface Repo\n");
    write(&root, "app/src/main/kotlin/RepoImpl.kt", "class RepoImpl : Repo\n");
    run(&root, &["rebuild"]);

    for index_type in ["modules", "deps", "files"] {
        run(&root, &["rebuild", "--type", index_type]);
        let class = run(&root, &["class", "RepoImpl"]);
        assert!(
            class.contains("app/src/main/kotlin/RepoImpl.kt"),
            "symbols lost after --type {index_type}: {class}"
        );
        let deps = run(&root, &["deps", "app"]);
        assert!(
            deps.contains("core"),
            "module deps lost after --type {index_type}: {deps}"
        );
    }
}

#[test]
fn rebuild_type_modules_keeps_module_owned_resources() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("project");
    write(&root, "settings.gradle.kts", "include(\":app\")\n");
    write(&root, "app/build.gradle.kts", "");
    write(
        &root,
        "app/src/main/res/values/strings.xml",
        "<resources><string name=\"title\">T</string></resources>\n",
    );
    write(&root, "app/src/main/kotlin/Screen.kt", "class Screen { val t = R.string.title }\n");
    run(&root, &["rebuild"]);
    let resources = count(&root, "resources");
    assert_ne!(resources, "0");

    run(&root, &["rebuild", "--type", "modules"]);

    assert_eq!(count(&root, "resources"), resources);
}

#[test]
fn update_refreshes_module_graph_when_build_files_change() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("project");
    write(&root, "settings.gradle.kts", "include(\":app\", \":core\")\n");
    write(&root, "core/build.gradle.kts", "");
    write(
        &root,
        "app/build.gradle.kts",
        "dependencies { implementation(project(\":core\")) }\n",
    );
    write(&root, "app/src/main/kotlin/App.kt", "class App\n");
    run(&root, &["rebuild"]);
    assert!(run(&root, &["deps", "app"]).contains("core"));

    // Edit a build file (not a parsed source) and add a new module.
    write(&root, "app/build.gradle.kts", "dependencies { implementation(project(\":feature\")) }\n");
    write(&root, "feature/build.gradle.kts", "");
    run(&root, &["update"]);

    let deps = run(&root, &["deps", "app"]);
    assert!(deps.contains("feature"), "new dependency missing: {deps}");
    assert!(!deps.contains("core ("), "removed dependency still listed: {deps}");
}

#[test]
fn unread_manifests_are_reported_only_by_verbose_rebuild() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("project");
    write(&root, "Tuist.swift", "");
    write(
        &root,
        "Modules/Odd/Project.swift",
        "let project = Project(name: Odd.self, features: [.feature(name: .A)])\n",
    );
    write(&root, "Modules/Odd/A/Sources/A.swift", "struct A {}\n");

    let (_, quiet) = run_full(&root, &["rebuild"]);
    assert!(!quiet.contains("Modules/Odd/Project.swift"), "{quiet}");
    let (_, verbose) = run_full(&root, &["rebuild", "--verbose"]);
    assert!(verbose.contains("Modules/Odd/Project.swift"), "{verbose}");

    let (out, err) = run_full(&root, &["dependents", "A"]);
    assert!(!out.contains("Project.swift") && !err.contains("Project.swift"), "{out}{err}");
}
