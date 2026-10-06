//! Step 2 of #31: CLI surface for named subtrees (`subtree add/remove/list`).
//! Drives the binary end-to-end so we cover clap routing and stdout/stderr
//! formatting on top of the DB layer covered in subtree_schema_tests.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_ast-index"))
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn rebuild(root: &Path) {
    let out = Command::new(binary())
        .current_dir(root)
        .args(["rebuild"])
        .env(
            "AST_INDEX_CACHE_DIR",
            root.parent().unwrap_or(root).join("ast-index-test-cache"),
        )
        .env("AST_INDEX_DISABLE_GC", "1")
        .env_remove("AST_INDEX_DB_PATH")
        .env_remove("KOTLIN_INDEX_DB_PATH")
        .env_remove("AST_INDEX_MAX_FILES")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "rebuild failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(binary())
        .current_dir(root)
        .args(args)
        .env(
            "AST_INDEX_CACHE_DIR",
            root.parent().unwrap_or(root).join("ast-index-test-cache"),
        )
        .env("AST_INDEX_DISABLE_GC", "1")
        .env_remove("AST_INDEX_DB_PATH")
        .env_remove("KOTLIN_INDEX_DB_PATH")
        .env_remove("AST_INDEX_MAX_FILES")
        .output()
        .unwrap()
}

#[test]
fn add_list_remove_round_trip() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    let extra = tmp.path().join("extra");
    write(
        &project.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0\"\n",
    );
    write(&project.join("src/lib.rs"), "pub fn a() {}\n");
    write(
        &extra.join("Cargo.toml"),
        "[package]\nname=\"y\"\nversion=\"0\"\n",
    );
    write(&extra.join("src/lib.rs"), "pub fn b() {}\n");

    rebuild(&project);

    // Initially no subtrees.
    let list = run(&project, &["subtree", "list"]);
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("(primary)"));
    assert!(stdout.contains("No extra subtrees attached"));

    // Add the sibling extra/.
    let add = run(&project, &["subtree", "add", "extra", "../extra"]);
    assert!(
        add.status.success(),
        "subtree add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let stdout = String::from_utf8_lossy(&add.stdout);
    assert!(stdout.contains("Attached subtree extra"));
    assert!(
        stdout.contains("source: ../extra"),
        "original path should appear"
    );

    // List shows it now.
    let list = run(&project, &["subtree", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("extra"));
    assert!(stdout.contains("../extra"));

    // JSON list returns proper structure.
    let json = run(&project, &["--format", "json", "subtree", "list"]);
    let stdout = String::from_utf8_lossy(&json.stdout);
    assert!(stdout.contains("\"name\": \"extra\""));
    assert!(stdout.contains("\"original_path\": \"../extra\""));

    // Remove and confirm.
    let remove = run(&project, &["subtree", "remove", "extra"]);
    assert!(remove.status.success());
    let stdout = String::from_utf8_lossy(&remove.stdout);
    assert!(stdout.contains("Detached subtree extra"));

    let list = run(&project, &["subtree", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("No extra subtrees attached"));
}

#[test]
fn add_with_duplicate_name_rejects() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    let extra1 = tmp.path().join("extra1");
    let extra2 = tmp.path().join("extra2");
    write(
        &project.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0\"\n",
    );
    write(&project.join("src/lib.rs"), "pub fn a() {}\n");
    write(&extra1.join("a.rs"), "fn x() {}\n");
    write(&extra2.join("b.rs"), "fn y() {}\n");

    rebuild(&project);

    let first = run(&project, &["subtree", "add", "core", "../extra1"]);
    assert!(first.status.success());

    let dup = run(&project, &["subtree", "add", "core", "../extra2"]);
    assert!(dup.status.success(), "should not crash on duplicate name");
    let stdout = String::from_utf8_lossy(&dup.stdout);
    assert!(
        stdout.contains("already attached"),
        "expected duplicate-name warning, got: {stdout}"
    );

    // Only one subtree should be in the index.
    let list = run(&project, &["--format", "json", "subtree", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 1);
}

#[test]
fn add_overlapping_with_root_rejects_without_force() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    write(
        &project.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0\"\n",
    );
    write(&project.join("src/lib.rs"), "pub fn a() {}\n");
    write(&project.join("sub/a.rs"), "fn x() {}\n");

    rebuild(&project);

    // Without --force we refuse to attach a nested directory.
    let inside = run(&project, &["subtree", "add", "inner", "./sub"]);
    assert!(inside.status.success());
    let stdout = String::from_utf8_lossy(&inside.stdout);
    assert!(
        stdout.contains("inside the project root"),
        "expected overlap warning, got: {stdout}"
    );

    // With --force the attach goes through.
    let forced = run(&project, &["subtree", "add", "inner", "./sub", "--force"]);
    assert!(forced.status.success());
    let stdout = String::from_utf8_lossy(&forced.stdout);
    assert!(stdout.contains("Attached subtree inner"));
}

#[test]
fn legacy_add_root_still_works_and_auto_names() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    let extra = tmp.path().join("legacy");
    write(
        &project.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0\"\n",
    );
    write(&project.join("src/lib.rs"), "pub fn a() {}\n");
    write(&extra.join("a.rs"), "fn x() {}\n");

    rebuild(&project);

    let add = run(&project, &["add-root", "../legacy"]);
    assert!(add.status.success());

    let list = run(&project, &["subtree", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        stdout.contains("legacy"),
        "auto-name from path basename, got: {stdout}"
    );
}

fn subtree_names(project: &Path) -> Vec<String> {
    let list = run(project, &["--format", "json", "subtree", "list"]);
    assert!(list.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    parsed
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect()
}

fn symbol_count(project: &Path, name: &str) -> usize {
    let out = run(project, &["--format", "json", "symbol", name]);
    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    parsed["items"].as_array().unwrap().len()
}

/// Inserts a subtree row the way an older binary left it, bypassing today's checks.
fn attach_behind_cli(project: &Path, name: &str, path: &Path, original: &str) {
    let cache = project.parent().unwrap().join("ast-index-test-cache");
    let db_path = fs::read_dir(&cache)
        .unwrap()
        .map(|entry| entry.unwrap().path().join("index.db"))
        .find(|path| path.is_file())
        .expect("index.db under the test cache");
    let canonical = path.canonicalize().unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "INSERT INTO subtrees (name, canonical_path, original_path) VALUES (?1, ?2, ?3)",
            [name, &*canonical.to_string_lossy(), original],
        )
        .unwrap();
}

#[test]
fn config_root_naming_the_project_itself_is_skipped() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    let extra = tmp.path().join("extra");
    write(&project.join("a.php"), "<?php class Alpha {}\n");
    write(&extra.join("b.php"), "<?php class Beta {}\n");
    write(&project.join(".ast-index.yaml"), "roots: [\".\", \"../extra\"]\n");

    rebuild(&project);

    assert_eq!(subtree_names(&project), vec!["extra".to_string()]);
    assert_eq!(symbol_count(&project, "Alpha"), 1);
    assert_eq!(symbol_count(&project, "Beta"), 1);
}

#[test]
fn subtree_add_of_project_root_is_refused_even_with_force() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    write(&project.join("a.php"), "<?php class Alpha {}\n");

    rebuild(&project);

    for args in [
        &["subtree", "add", "self", ".", "--force"][..],
        &["add-root", ".", "--force"][..],
    ] {
        let out = run(&project, args);
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("project root itself"), "{args:?}: {stdout}");
    }
    assert!(subtree_names(&project).is_empty());
    rebuild(&project);
}

#[test]
fn rebuild_drops_saved_subtree_pointing_at_project_root() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    write(&project.join("a.php"), "<?php class Alpha {}\n");

    rebuild(&project);

    // An index attached to its own root by an older binary (`subtree add . --force`).
    attach_behind_cli(&project, "self", &project, ".");
    assert_eq!(subtree_names(&project), vec!["self".to_string()]);

    rebuild(&project);

    assert!(subtree_names(&project).is_empty());
    assert_eq!(symbol_count(&project, "Alpha"), 1);
}

#[test]
fn config_roots_overlapping_the_project_or_each_other_are_skipped() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    let lib = tmp.path().join("lib");
    write(&project.join("a.php"), "<?php class Alpha {}\n");
    write(&project.join("sub/b.php"), "<?php class Beta {}\n");
    write(&lib.join("c.php"), "<?php class Gamma {}\n");
    write(&lib.join("core/d.php"), "<?php class Delta {}\n");
    write(
        &project.join(".ast-index.yaml"),
        "roots: [\"sub\", \"..\", \"../lib\", \"../lib/core\"]\n",
    );

    let out = run(&project, &["rebuild"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    for (raw, reason) in [
        ("sub", "is inside the project root"),
        ("..", "contains the project root"),
        ("../lib/core", "overlaps the attached root"),
    ] {
        let warning = format!("skipped config root '{raw}': it {reason}");
        assert!(stderr.contains(&warning), "no '{warning}' in: {stderr}");
    }

    assert_eq!(subtree_names(&project), vec!["lib".to_string()]);
    for class in ["Alpha", "Beta", "Gamma", "Delta"] {
        assert_eq!(symbol_count(&project, class), 1, "{class}");
    }
}

#[test]
fn nested_config_root_saved_by_an_older_binary_is_dropped() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    write(&project.join("sub/b.php"), "<?php class Beta {}\n");

    rebuild(&project);
    attach_behind_cli(&project, "sub", &project.join("sub"), "sub");
    write(&project.join(".ast-index.yaml"), "roots: [\"sub\"]\n");

    rebuild(&project);

    assert!(subtree_names(&project).is_empty());
    assert_eq!(symbol_count(&project, "Beta"), 1);
}

#[test]
fn forced_nested_subtree_survives_rebuild() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    write(&project.join("sub/b.php"), "<?php class Beta {}\n");

    rebuild(&project);
    let forced = run(&project, &["subtree", "add", "inner", "./sub", "--force"]);
    assert!(forced.status.success());

    rebuild(&project);

    assert_eq!(subtree_names(&project), vec!["inner".to_string()]);
}
