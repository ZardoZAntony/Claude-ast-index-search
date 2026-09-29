//! Sources in a legacy single-byte encoding (Windows-1251 in old PHP templates) are indexed
//! with lossy decoding instead of failing — and being re-parsed by every update.

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

#[test]
fn windows_1251_file_is_indexed_once() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // "// Привет" in Windows-1251 — invalid as UTF-8.
    let mut content =
        b"<?php\n// \xcf\xf0\xe8\xe2\xe5\xf2\nfinal class LegacyEncoded\n{\n".to_vec();
    content.extend_from_slice(b"    public function run(): void { new Helper(); }\n}\n");
    fs::write(root.join("legacy.php"), &content).unwrap();

    let mut conn = open_fresh_db(root);
    indexer::index_directory(&mut conn, root, false, false).unwrap();
    assert!(symbol_exists(&conn, "LegacyEncoded"));
    assert_eq!(db::find_references(&conn, "Helper", 10).unwrap().len(), 1);

    let (_, changed, _) =
        indexer::update_directory_incremental(&mut conn, root, false, None, None).unwrap();
    assert_eq!(changed, 0, "unchanged legacy-encoded file was re-parsed");
}

#[test]
fn binary_file_with_nul_bytes_is_still_skipped() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("blob.php"),
        b"<?php\x00\xff\xfe final class Binary {}\n",
    )
    .unwrap();

    let mut conn = open_fresh_db(root);
    indexer::index_directory(&mut conn, root, false, false).unwrap();
    assert!(!symbol_exists(&conn, "Binary"));
}
