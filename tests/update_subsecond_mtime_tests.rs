//! Incremental update must notice an equal-size edit made within the same second as the
//! previous indexing — with second-precision mtimes such an edit was never re-parsed.

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

fn write_with_mtime(path: &Path, content: &str, mtime: SystemTime) {
    fs::write(path, content).unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

#[test]
fn equal_size_edit_within_the_same_second_is_reindexed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let file = root.join("B.php");
    let second = UNIX_EPOCH + Duration::from_secs(1_800_000_000);

    write_with_mtime(
        &file,
        "<?php\nfinal class Kap1 {}\n",
        second + Duration::from_millis(100),
    );
    let mut conn = open_fresh_db(root);
    indexer::index_directory(&mut conn, root, false, false).unwrap();
    assert!(symbol_exists(&conn, "Kap1"));

    // Same length, same wall-clock second, different sub-second mtime.
    write_with_mtime(
        &file,
        "<?php\nfinal class Lam1 {}\n",
        second + Duration::from_millis(600),
    );
    indexer::update_directory_incremental(&mut conn, root, false, None, None).unwrap();

    assert!(
        symbol_exists(&conn, "Lam1"),
        "edit within the same second was not re-indexed"
    );
    assert!(
        !symbol_exists(&conn, "Kap1"),
        "stale symbol survived the update"
    );
}
