//! End-to-end: class names that appear only in PHPDoc become usages.
//!
//! A DTO referenced only from `@var ItemDto[]` on another DTO's property is live code (a
//! runtime mapper builds it from that annotation), so `usages` must find the annotation line.

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

fn usage_lines(conn: &Connection, name: &str) -> Vec<(String, i64)> {
    let mut lines: Vec<(String, i64)> = db::find_references(conn, name, 100)
        .unwrap()
        .into_iter()
        .map(|r| (r.path, r.line))
        .collect();
    lines.sort();
    lines
}

#[test]
fn phpdoc_only_class_is_found_by_usages() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let dto = root.join("src/Dto");
    fs::create_dir_all(&dto).unwrap();

    fs::write(
        dto.join("ItemDto.php"),
        "<?php\n\nnamespace App\\Dto;\n\nfinal readonly class ItemDto\n{\n    public function __construct(public int $id) {}\n}\n",
    )
    .unwrap();
    fs::write(
        dto.join("ListDto.php"),
        r#"<?php

namespace App\Dto;

final readonly class ListDto
{
    public function __construct(
        /** @var ItemDto[] */
        public array $items,
        /** @var array<int, array{
         *     id: int,
         *     meta: MetaDto,
         * }> */
        public array $rows,
    ) {}

    /**
     * @return list<ItemDto> Items Sorted By Id
     * @throws ListException When Something Fails
     */
    public function sorted(): array
    {
        return $this->items;
    }
}
"#,
    )
    .unwrap();

    let mut conn = open_fresh_db(root);
    indexer::index_directory(&mut conn, root, false, false).unwrap();

    assert_eq!(
        usage_lines(&conn, "ItemDto"),
        vec![
            ("src/Dto/ListDto.php".to_string(), 8),
            ("src/Dto/ListDto.php".to_string(), 18),
        ]
    );
    assert_eq!(
        usage_lines(&conn, "MetaDto"),
        vec![("src/Dto/ListDto.php".to_string(), 12)]
    );
    assert_eq!(
        usage_lines(&conn, "ListException"),
        vec![("src/Dto/ListDto.php".to_string(), 19)]
    );
    // Description words after the type are prose, not references.
    assert!(usage_lines(&conn, "Sorted").is_empty());
    assert!(usage_lines(&conn, "When").is_empty());
}
