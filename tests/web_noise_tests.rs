//! Front-end noise: minified bundles stay out of the index.

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
fn minified_bundle_stays_out_of_the_index() {
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

    assert!(!file_recorded(&conn, "bundle.js"), "minified bundle was indexed");
    assert!(
        !symbol_exists(&conn, "MinifiedEntry"),
        "minified bundle was parsed"
    );
    assert!(symbol_exists(&conn, "HandwrittenIcon"));
}
