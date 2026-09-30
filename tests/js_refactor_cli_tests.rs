//! JS/TS/Vue module commands on a small project: exports resolved through jsconfig/tsconfig paths, aliases,
//! relative specifiers, index files and re-exports — `usages`/`impact 'path#name'`, module `impact`,
//! `move-plan` for a file, unused exports, and the migration of an index built before module facts existed.

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
    write(root, ".ast-index.yaml", "js_aliases:\n  \"@ui/\": \"web/src/ui/\"\nunused_ignore:\n  - \"**/main.js\"\n");
    write(root, "web/package.json", "{}\n");
    write(
        root,
        "web/jsconfig.json",
        "{\n  // the project alias\n  \"compilerOptions\": {\n    \"paths\": { \"@/*\": [\"src/*\"], },\n  },\n}\n",
    );
    write(
        root,
        "web/src/shared/price.js",
        "export function formatPrice(value) {\n  return `${value} ₽`\n}\n\nexport const CURRENCY = 'RUB'\n\nexport function unusedHelper() {\n  return formatPrice(0)\n}\n",
    );
    write(root, "web/src/shared/index.js", "export { formatPrice as price } from './price'\nexport * from './status.ts'\n");
    write(root, "web/src/shared/status.ts", "export enum Status { Idle = 'idle' }\nexport type StatusValue = `${Status}`\n");
    write(
        root,
        "web/src/ui/Popup.vue",
        "<template>\n  <div class=\"popup\"><slot /></div>\n</template>\n\n<script setup>\nimport { formatPrice } from '@/shared/price.js'\nimport PopupHeader from './PopupHeader.vue'\n</script>\n",
    );
    write(root, "web/src/ui/PopupHeader.vue", "<template>\n  <h2>header</h2>\n</template>\n");
    write(
        root,
        "web/src/apps/cart/CartApp.vue",
        "<template>\n  <Popup>\n    <popup-header />\n    {{ formatPrice(total) }}\n  </Popup>\n</template>\n\n<script setup>\nimport Popup from '@/ui/Popup.vue'\nimport PopupHeader from '@ui/PopupHeader.vue'\nimport { formatPrice } from '../../shared/price'\nimport * as shared from '@/shared'\n/** @type {import('@/shared/status.ts').StatusValue} */\nconst status = shared.Status.Idle\nconst label = shared.price(1)\n</script>\n",
    );
    write(
        root,
        "web/src/apps/cart/main.js",
        "import { createApp } from 'vue'\nimport CartApp from './CartApp.vue'\nconst pages = import.meta.glob('./pages/*.vue')\ncreateApp(CartApp).mount('#app')\n",
    );
    write(root, "web/src/apps/cart/pages/Summary.vue", "<template><p>summary</p></template>\n");
    write(root, "web/src/apps/cart/lazy.js", "export const load = () => import('../../ui/Popup.vue')\n");
    write(root, "web/src/legacy/dead.js", "export function nobody() {}\n");
    write(
        root,
        "web/tests/price.test.js",
        "import { formatPrice as fp } from '@/shared/price'\nvi.mock('@/shared/price.js', () => ({ formatPrice: vi.fn() }))\ntest('x', () => fp(1))\n",
    );
    write(root, "web/vite.config.js", "export default {}\n");
    write(root, "web/src/shared/format.js", "export function onlyInTests() {}\n");
    write(root, "web/src/shared/lazy.js", "export const a = 1\nexport const b = 2\nexport const c = 3\nexport const d = 4\n");
    write(
        root,
        "web/src/apps/cart/lazyUser.js",
        "export async function take() {\n  const { a } = await import('../../shared/lazy.js')\n  const ns = await import('@/shared/lazy.js')\n  return import('../../shared/lazy').then((m) => m.b + a + ns.c)\n}\n",
    );
    write(root, "web/tests/format.test.js", "import { onlyInTests } from '../src/shared/format.js'\nonlyInTests()\n");
    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

fn lines(items: &Value) -> Vec<String> {
    let mut out: Vec<String> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|r| format!("{}:{} {}", r["path"].as_str().unwrap(), r["line"], r["kind"].as_str().unwrap()))
        .collect();
    out.sort();
    out
}

#[test]
fn usages_of_an_export_follow_imports_aliases_and_reexports() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let usages = run(root, cache.path(), &["usages", "web/src/shared/price.js#formatPrice", "--format", "json"]);
    assert_eq!(
        lines(&usages["items"]),
        vec![
            "web/src/apps/cart/CartApp.vue:11 import",   // relative, no extension
            "web/src/apps/cart/CartApp.vue:12 namespace", // import * as shared from '@/shared' (the barrel)
            "web/src/apps/cart/CartApp.vue:15 use",      // shared.price(…) through the barrel's alias
            "web/src/apps/cart/CartApp.vue:4 use",       // template interpolation
            "web/src/shared/index.js:1 reexport",        // export { formatPrice as price } from './price'
            "web/src/ui/Popup.vue:6 import",             // @/ alias from jsconfig, with extension
            "web/tests/price.test.js:1 import",          // renamed import
            "web/tests/price.test.js:2 mock",            // vi.mock of the module
            "web/tests/price.test.js:3 use",             // fp(1)
        ]
    );
}

#[test]
fn impact_of_an_export_adds_definition_local_uses_and_tests() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let impact = run(root, cache.path(), &["impact", "web/src/shared/price.js#formatPrice", "--format", "json"]);
    assert_eq!(impact["definitions"][0]["line"], 1);
    let refs = lines(&impact["references"]);
    assert!(refs.contains(&"web/src/shared/price.js:8 local".to_string()), "{refs:?}");
    assert_eq!(impact["summary"]["tests"], 3);

    // a default export: every local name it is imported under, template tags included
    let popup = run(root, cache.path(), &["impact", "web/src/ui/Popup.vue#default", "--format", "json"]);
    let refs = lines(&popup["references"]);
    assert!(refs.contains(&"web/src/apps/cart/CartApp.vue:2 use".to_string()), "{refs:?}");
    assert!(refs.contains(&"web/src/apps/cart/CartApp.vue:9 import".to_string()), "{refs:?}");
    assert!(refs.contains(&"web/src/apps/cart/lazy.js:1 dynamic".to_string()), "{refs:?}");

    // kebab-case tag of a component imported through a js_aliases prefix
    let header = run(root, cache.path(), &["usages", "web/src/ui/PopupHeader.vue#default", "--format", "json"]);
    let refs = lines(&header["items"]);
    assert!(refs.contains(&"web/src/apps/cart/CartApp.vue:10 import".to_string()), "{refs:?}");
    assert!(refs.contains(&"web/src/apps/cart/CartApp.vue:3 use".to_string()), "{refs:?}");

    // an export reached through `export *` and a JSDoc type
    let status = run(root, cache.path(), &["usages", "web/src/shared/status.ts#StatusValue", "--format", "json"]);
    assert_eq!(lines(&status["items"]), vec!["web/src/apps/cart/CartApp.vue:13 jsdoc"]);
    let status = run(root, cache.path(), &["usages", "web/src/shared/status.ts#Status", "--format", "json"]);
    assert_eq!(lines(&status["items"]), vec!["web/src/apps/cart/CartApp.vue:12 namespace", "web/src/apps/cart/CartApp.vue:14 use"]);
}

#[test]
fn dynamic_imports_count_for_the_names_they_take() {
    let (tmp, cache) = project();
    let root = tmp.path();
    for (name, expected) in [
        ("a", vec!["web/src/apps/cart/lazyUser.js:2 dynamic", "web/src/apps/cart/lazyUser.js:4 use"]),
        ("b", vec!["web/src/apps/cart/lazyUser.js:4 dynamic"]),
        ("c", vec!["web/src/apps/cart/lazyUser.js:3 dynamic", "web/src/apps/cart/lazyUser.js:4 use"]),
        ("d", vec![]),
    ] {
        let target = format!("web/src/shared/lazy.js#{name}");
        let usages = run(root, cache.path(), &["usages", &target, "--format", "json"]);
        assert_eq!(lines(&usages["items"]), expected, "{name}");
    }
}

#[test]
fn module_impact_lists_every_reference_and_exports() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let impact = run(root, cache.path(), &["impact", "web/src/shared/price", "--format", "json"]);
    assert_eq!(impact["target"], "web/src/shared/price.js");
    let kinds = &impact["summary"]["by_kind"];
    assert_eq!(kinds["import"], 3);
    assert_eq!(kinds["reexport"], 1);
    assert_eq!(kinds["mock"], 1);
    let exports: Vec<(String, i64)> = impact["exports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["name"].as_str().unwrap().to_string(), e["references"].as_i64().unwrap()))
        .collect();
    assert!(exports.contains(&("CURRENCY".to_string(), 0)));
    assert!(exports.contains(&("unusedHelper".to_string(), 0)));

    // glob and a directory index resolve to modules too
    let summary = run(root, cache.path(), &["impact", "web/src/apps/cart/pages/Summary.vue", "--format", "json"]);
    assert_eq!(summary["summary"]["by_kind"]["glob"], 1);
    let barrel = run(root, cache.path(), &["impact", "web/src/shared/index.js", "--format", "json"]);
    assert_eq!(barrel["summary"]["by_kind"]["import"], 1);
}

#[test]
fn move_plan_rewrites_specifiers_in_their_own_style() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let plan = run(root, cache.path(), &["move-plan", "web/src/shared/price.js", "web/src/money/", "--format", "json"]);
    assert_eq!(plan["to"], "web/src/money/price.js");
    let edits: Vec<String> = plan["edits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| format!("{}:{} {} -> {}", e["path"].as_str().unwrap(), e["line"], e["from"].as_str().unwrap(), e["to"].as_str().unwrap()))
        .collect();
    assert_eq!(
        edits,
        vec![
            "web/src/apps/cart/CartApp.vue:11 ../../shared/price -> ../../money/price",
            "web/src/shared/index.js:1 ./price -> ../money/price",
            "web/src/ui/Popup.vue:6 @/shared/price.js -> @/money/price.js",
            "web/tests/price.test.js:1 @/shared/price -> @/money/price",
            "web/tests/price.test.js:2 @/shared/price.js -> @/money/price.js",
        ]
    );

    let plan = run(root, cache.path(), &["move-plan", "web/src/ui/Popup.vue", "web/src/ui/popup/Popup.vue", "--format", "json"]);
    let own: Vec<String> = plan["own_imports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| format!("{} -> {}", e["from"].as_str().unwrap(), e["to"].as_str().unwrap()))
        .collect();
    assert_eq!(own, vec!["./PopupHeader.vue -> ../PopupHeader.vue"]);
    let dynamic = plan["edits"].as_array().unwrap().iter().find(|e| e["kind"] == "dynamic").unwrap();
    assert_eq!(dynamic["to"], "../../ui/popup/Popup.vue");
}

#[test]
fn unused_exports_and_unreferenced_modules() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let rows = run(root, cache.path(), &["unused-symbols", "--module", "web/", "--format", "json"]);
    let rows: Vec<(String, String, String)> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_string(),
                r["name"].as_str().unwrap().to_string(),
                r["reason"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    assert!(rows.contains(&("web/src/shared/price.js".into(), "CURRENCY".into(), "export nobody imports".into())));
    assert!(rows.contains(&(
        "web/src/shared/price.js".into(),
        "unusedHelper".into(),
        "export nobody imports".into()
    )));
    assert!(rows.iter().any(|r| r.0 == "web/src/legacy/dead.js" && r.2.starts_with("module nothing references")));
    // entry point in unused_ignore, tests and tool configs are not reported; formatPrice is used
    assert!(!rows.iter().any(|r| r.0.ends_with("main.js") || r.0.contains("tests/") || r.0.ends_with("vite.config.js")));
    assert!(!rows.iter().any(|r| r.1 == "formatPrice"));
    assert!(rows.contains(&("web/src/shared/format.js".into(), "onlyInTests".into(), "export only tests import".into())));
    // import() taken apart by name: destructuring, a bound module object, .then((m) => m.b)
    let lazy: Vec<&str> = rows.iter().filter(|r| r.0 == "web/src/shared/lazy.js").map(|r| r.1.as_str()).collect();
    assert_eq!(lazy, vec!["d"]);
    // namespace import takes the whole barrel, glob takes the pages
    assert!(!rows.iter().any(|r| r.0.ends_with("shared/index.js") || r.0.ends_with("Summary.vue")));
}

#[test]
fn short_names_route_to_the_single_exporter_or_list_them() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let text = run_text(root, cache.path(), &["impact", "formatPrice"]);
    assert!(text.starts_with("Impact of web/src/shared/price.js#formatPrice"), "{text}");
    write(root, "web/src/other/price.js", "export function formatPrice() {}\n");
    run(root, cache.path(), &["update"]);
    let many = run(root, cache.path(), &["impact", "formatPrice", "--format", "json"]);
    assert_eq!(many["did_you_mean"].as_array().unwrap().len(), 2);
    let err = command(root, cache.path(), &["impact", "web/src/shared/price.js#nothing"]);
    assert!(String::from_utf8_lossy(&err.stderr).contains("its exports: CURRENCY, formatPrice, unusedHelper"));
}

#[test]
fn index_built_before_module_facts_is_filled_by_update() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let db_path = run_text(root, cache.path(), &["db-path"]).trim().to_string();
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("DROP TABLE js_uses; DROP TABLE js_exports; DROP TABLE js_imports;").unwrap();
    }
    run(root, cache.path(), &["update"]);
    let usages = run(root, cache.path(), &["usages", "web/src/shared/price.js#formatPrice", "--format", "json"]);
    assert_eq!(usages["pagination"]["total"], 9);
}

#[test]
fn extractor_version_change_reparses_js_once() {
    let (tmp, cache) = project();
    let root = tmp.path();
    let db_path = run_text(root, cache.path(), &["db-path"]).trim().to_string();
    // a fresh index is current: nothing to re-parse
    let fresh = run_text(root, cache.path(), &["update"]);
    assert!(fresh.contains("Index is up to date."), "{fresh}");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("UPDATE metadata SET value = '1' WHERE key = 'js_modules_version'", []).unwrap();
        conn.execute("DELETE FROM js_imports", []).unwrap();
    }
    let after = run_text(root, cache.path(), &["update"]);
    assert!(after.contains("Updated:"), "{after}");
    let usages = run(root, cache.path(), &["usages", "web/src/shared/price.js#formatPrice", "--format", "json"]);
    assert_eq!(usages["pagination"]["total"], 9);
}
