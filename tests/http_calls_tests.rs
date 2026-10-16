//! `http-calls`: backend calls across JS modules (constants and wrappers imported from other
//! modules), a Vue component, and the inline script of a PHP template with a `window.X` global.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn run(cwd: &Path, cache: &Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_ast-index"))
        .current_dir(cwd)
        .args(args)
        .env("AST_INDEX_CACHE_DIR", cache)
        .env("AST_INDEX_DISABLE_GC", "1")
        .env_remove("AST_INDEX_DB_PATH")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

const REQUEST: &str = r#"import axios from 'axios'

const api = axios.create({ baseURL: '' })

export async function get(url, params = {}) {
  const resp = await api.get(url, { params })
  return resp.data
}

export default { api, get }
"#;

const ENDPOINTS: &str = "export const SUMMARY_ENDPOINT = '/api/v1/basket/summary'\n";

const STORE: &str = r#"import request from '../api/request'
import { SUMMARY_ENDPOINT } from '../api/endpoints'

export async function loadSummary(siteId) {
  const summary = await request.get(SUMMARY_ENDPOINT, { siteId })
  return summary.items
}
"#;

const COMPONENT: &str = r#"<template>
  <div>{{ salons.length }}</div>
</template>

<script>
import axios from 'axios'

export default {
  methods: {
    load(offerId) {
      axios
        .get('/api/v1/delivery/reserve-salons/', { params: { offerId } })
        .then((response) => {
          this.salons = response.data.data
        })
    },
  },
}
</script>
"#;

const TEMPLATE: &str = r#"<?php
$dataApp = ['products' => $products, 'offers' => $offers];
?>
<div id="app"></div>
<script>
  window.dataApp = <?= json_encode($dataApp) ?>;
  BX.ajax.runComponentAction('shop:users.list', 'getFiltered', {
    mode: 'class',
    data: { filters: {} },
  }).then(function (response) {
    render(response.data.RESULT);
  });
</script>
"#;

fn project() -> (TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "frontend/src/api/request.js", REQUEST);
    write(root, "frontend/src/api/endpoints.js", ENDPOINTS);
    write(root, "frontend/src/stores/basket.js", STORE);
    write(root, "frontend/src/components/Salons.vue", COMPONENT);
    write(root, "frontend/src/main.js", "const products = window.dataApp.products\n");
    write(root, "local/templates/main/components/shop/list/custom/template.php", TEMPLATE);
    write(root, "frontend/dist/app.js", "fetch('/api/v1/should-not-appear')\n");
    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

#[test]
fn calls_resolve_imported_constants_and_wrappers_vue_and_templates() {
    let (tmp, cache) = project();
    let result = run(tmp.path(), cache.path(), &["http-calls", "--format", "json"]);
    let calls = result["calls"].as_array().unwrap();
    let at = |path: &str| -> &Value {
        calls
            .iter()
            .find(|c| c["path"].as_str().unwrap().ends_with(path))
            .unwrap_or_else(|| panic!("no call in {path}: {calls:#?}"))
    };
    assert!(
        calls.iter().all(|c| !c["path"].as_str().unwrap().contains("/dist/")),
        "dist/ is skipped"
    );

    let summary = at("stores/basket.js");
    assert_eq!(summary["client"], "wrapper");
    assert_eq!(summary["via"], "request.get");
    assert_eq!(summary["method"], "GET");
    assert_eq!(summary["url"], "/api/v1/basket/summary");
    assert_eq!(summary["params"], serde_json::json!(["siteId"]));
    assert_eq!(summary["response_keys"], serde_json::json!(["items"]), "the wrapper returns the body");

    let salons = at("components/Salons.vue");
    assert_eq!(salons["line"], 12);
    assert_eq!(salons["url"], "/api/v1/delivery/reserve-salons/");
    assert_eq!(salons["params"], serde_json::json!(["offerId"]));
    assert_eq!(salons["response_keys"], serde_json::json!(["data"]));

    let action = at("custom/template.php");
    assert_eq!(action["client"], "bx.runComponentAction");
    assert_eq!(action["line"], 7);
    assert_eq!(action["component"], "shop:users.list");
    assert_eq!(action["action"], "getFiltered");
    assert_eq!(action["mode"], "class");
    assert_eq!(action["response_keys"], serde_json::json!(["RESULT"]));

    let data_app = &result["globals"]["dataApp"];
    assert_eq!(data_app["writes"][0]["keys"], serde_json::json!(["products", "offers"]));
    assert_eq!(data_app["reads"][0]["key"], "products");
    assert!(data_app["reads"][0]["path"].as_str().unwrap().ends_with("main.js"));
}
