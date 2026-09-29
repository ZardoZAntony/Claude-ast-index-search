//! `include_hidden` in `.ast-index.yaml` lets walks enter selected hidden paths.
//!
//! Bitrix keeps component templates in `.default/` and module wiring in `.settings.php`; without
//! the option every dot-entry is skipped, and with it only the listed ones are walked. Rebuild
//! and update must agree, otherwise update would drop what rebuild indexed.

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

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn project(root: &Path) {
    write(
        root,
        "tpl/.default/template.php",
        "<?php\nfinal class TemplateHelper {}\n",
    );
    write(
        root,
        "module/.settings.php",
        "<?php\nfinal class SettingsWiring {}\n",
    );
    write(
        root,
        ".other/Other.php",
        "<?php\nfinal class HiddenOther {}\n",
    );
    write(root, "src/Visible.php", "<?php\nfinal class Visible {}\n");
}

#[test]
fn hidden_paths_stay_skipped_without_config() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    project(root);

    let mut conn = open_fresh_db(root);
    indexer::index_directory(&mut conn, root, false, false).unwrap();

    assert!(symbol_exists(&conn, "Visible"));
    assert!(!symbol_exists(&conn, "TemplateHelper"));
    assert!(!symbol_exists(&conn, "SettingsWiring"));
    assert!(!symbol_exists(&conn, "HiddenOther"));
}

#[test]
fn listed_hidden_paths_are_indexed_and_kept_by_update() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    project(root);
    write(
        root,
        ".ast-index.yaml",
        "include_hidden:\n  - .default\n  - .settings.php\n",
    );

    let mut conn = open_fresh_db(root);
    indexer::index_directory(&mut conn, root, false, false).unwrap();

    assert!(symbol_exists(&conn, "Visible"));
    assert!(symbol_exists(&conn, "TemplateHelper"));
    assert!(symbol_exists(&conn, "SettingsWiring"));
    assert!(
        !symbol_exists(&conn, "HiddenOther"),
        "unlisted hidden dir was walked"
    );

    write(
        root,
        "tpl/.default/template.php",
        "<?php\nfinal class TemplateHelperRenamed {}\n",
    );
    indexer::update_directory_incremental(&mut conn, root, false, None, None).unwrap();

    assert!(symbol_exists(&conn, "TemplateHelperRenamed"));
    assert!(!symbol_exists(&conn, "TemplateHelper"));
    assert!(
        symbol_exists(&conn, "SettingsWiring"),
        "update dropped a listed hidden file"
    );
    assert!(!symbol_exists(&conn, "HiddenOther"));
}
