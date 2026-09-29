//! Front-end noise: minified bundles are recorded but not parsed, and the project config may
//! live in the git directory (outside the working tree, shared by worktrees).

use std::fs;
use std::path::Path;

use ast_index::{db, indexer};
use rusqlite::Connection;
use tempfile::TempDir;

fn open_fresh_db(project_root: &Path) -> Connection {
    if db::db_exists(project_root) {
        db::delete_db(project_root).unwrap();
    }
    let conn = db::open_db(project_root).unwrap();
    db::init_db(&conn).unwrap();
    conn
}

fn symbol_exists(conn: &Connection, name: &str) -> bool {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM symbols WHERE name = ?1",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .unwrap();
    count > 0
}

fn file_recorded(conn: &Connection, rel: &str) -> bool {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE path = ?1",
            rusqlite::params![rel],
            |row| row.get(0),
        )
        .unwrap();
    count > 0
}

#[test]
fn minified_bundle_is_recorded_but_not_parsed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let mut bundle = String::from("function MinifiedEntry(){var a=1;");
    while bundle.len() < 5000 {
        bundle.push_str("function q(a,b){return a+b};");
    }
    bundle.push_str("}\n");
    fs::write(root.join("bundle.js"), &bundle).unwrap();

    // Handwritten source with one long line (inline SVG path) stays parseable.
    let svg = "M0 0 ".repeat(200);
    fs::write(
        root.join("icon.js"),
        format!(
            "export function HandwrittenIcon() {{\n  const a = 1;\n  const b = 2;\n  return '{svg}';\n}}\n\nexport function SecondHelper() {{\n  return HandwrittenIcon();\n}}\n\nexport function ThirdHelper() {{\n  return SecondHelper();\n}}\n\nexport function FourthHelper() {{\n  return ThirdHelper();\n}}\n{}",
            "// padding line with ordinary length to keep the long line a minority\n".repeat(20)
        ),
    )
    .unwrap();

    let mut conn = open_fresh_db(root);
    indexer::index_directory(&mut conn, root, false, false).unwrap();

    assert!(
        file_recorded(&conn, "bundle.js"),
        "bundle must stay tracked for update"
    );
    assert!(
        !symbol_exists(&conn, "MinifiedEntry"),
        "minified bundle was parsed"
    );
    assert!(symbol_exists(&conn, "HandwrittenIcon"));
}

#[test]
fn config_in_git_dir_applies_to_clone_and_linked_worktree() {
    let tmp = TempDir::new().unwrap();
    let main = tmp.path().join("main");
    fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
    fs::write(
        main.join(".git/ast-index.yaml"),
        "include_hidden:\n  - .default\n",
    )
    .unwrap();
    fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();

    let wt = tmp.path().join("wt");
    fs::create_dir_all(&wt).unwrap();
    fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
    )
    .unwrap();

    for root in [&main, &wt] {
        let config = indexer::load_config(root).expect("config from the git directory");
        assert_eq!(config.include_hidden, Some(vec![".default".to_string()]));
    }

    // A config in the working tree wins over the git-directory one.
    fs::write(main.join(".ast-index.yaml"), "exclude:\n  - vendor\n").unwrap();
    let config = indexer::load_config(&main).unwrap();
    assert_eq!(config.include_hidden, None);
    assert_eq!(config.exclude, Some(vec!["vendor".to_string()]));
}
