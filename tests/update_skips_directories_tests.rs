//! A directory whose name looks like a source file (`auth.mts/` — Bitrix names component
//! directories with dots) must not be queued for parsing by every incremental update.

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

#[test]
fn directory_with_source_extension_is_not_reparsed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let component = root.join("components/auth.mts");
    fs::create_dir_all(&component).unwrap();
    fs::write(
        component.join("class.php"),
        "<?php\nfinal class AuthComponent {}\n",
    )
    .unwrap();

    let mut conn = open_fresh_db(root);
    indexer::index_directory(&mut conn, root, false, false).unwrap();

    let (_, changed, _) =
        indexer::update_directory_incremental(&mut conn, root, false, None, None).unwrap();
    assert_eq!(
        changed, 0,
        "a directory was queued as a changed source file"
    );
}
