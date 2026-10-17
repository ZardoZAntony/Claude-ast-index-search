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
    write(
        root,
        "local/templates/main/components/shop/basket/custom/script.js",
        "const COMPONENT_NAME = 'shop:small.basket';\nconst runAction = (action, data) => BX.ajax.runComponentAction(COMPONENT_NAME, action, { mode: 'class', data: data });\nrunAction('load', { siteId: 's1' });\n",
    );
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

#[test]
fn path_filter_and_component_action_wrappers() {
    let (tmp, cache) = project();
    let result = run(tmp.path(), cache.path(), &["http-calls", "--path", "local/templates", "--format", "json"]);
    let calls = result["calls"].as_array().unwrap();
    assert!(calls.iter().all(|c| c["path"].as_str().unwrap().starts_with("local/templates/")), "{calls:#?}");
    let wrapped = calls
        .iter()
        .find(|c| c["path"].as_str().unwrap().ends_with("basket/custom/script.js"))
        .expect("the wrapper call");
    assert_eq!(wrapped["client"], "bx.runComponentAction");
    assert_eq!(wrapped["via"], "runAction");
    assert_eq!(wrapped["component"], "shop:small.basket");
    assert_eq!(wrapped["action"], "load");
    assert_eq!(wrapped["params"], serde_json::json!(["siteId"]));
}

const CLIENT_FACTORY: &str = r#"import axios from 'axios'
import { attachToken } from './interceptors'

export function createClient({ baseURL = '' } = {}) {
  const instance = attachToken(axios.create({ baseURL }))
  async function request(config) {
    const response = await instance.request(config)
    return response.data
  }
  return {
    get(url, config = {}) {
      return request({ ...config, method: 'get', url })
    },
    post(url, data = {}, config = {}) {
      return request({ ...config, method: 'post', url, data })
    },
  }
}
"#;

const ORDERS_API: &str = r#"import { createClient } from '../../shared/http'
import { http } from '../../shared/http/instance'
import { ORDERS_ENDPOINT, ROUTES } from '../../shared/api/endpoints'

export function createOrdersApi(client = createClient()) {
  return {
    list: (page) => client.get(ORDERS_ENDPOINT, { params: { page } }),
    create: (offerId) => client.post(ORDERS_ENDPOINT, { offerId, quantity: 1 }),
  }
}

export const loadRoute = (name) => http.get(ROUTES[name])
export const search = () => http.get(ROUTES.search)

export function createPing(api) {
  return { ping: () => api.get('/api/ping/') }
}
"#;

fn client_project() -> (TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write_client(root);
    write(root, "src/modules/orders/api.js", ORDERS_API);
    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

/// A factory, an index re-exporting it, a client made from it, endpoint constants.
fn write_client(root: &Path) {
    write(root, "src/shared/http/createClient.js", CLIENT_FACTORY);
    write(
        root,
        "src/shared/http/interceptors.js",
        "export function attachToken(client) {\n  client.interceptors.request.use((c) => c)\n  return client\n}\n",
    );
    write(root, "src/shared/http/index.js", "export { createClient } from './createClient'\n");
    write(root, "src/shared/http/instance.js", "import { createClient } from './index'\n\nexport const http = createClient()\n");
    write(
        root,
        "src/shared/api/endpoints.js",
        "export const ORDERS_ENDPOINT = '/api/orders/'\nexport const ROUTES = Object.freeze({\n  list: '/api/catalog/list/',\n  search: '/api/catalog/search/',\n})\n",
    );
}

#[test]
fn calls_through_an_imported_client_factory_and_instance() {
    let (tmp, cache) = client_project();
    let result = run(tmp.path(), cache.path(), &["http-calls", "--format", "json"]);
    let calls = result["calls"].as_array().unwrap();
    assert!(
        calls.iter().all(|c| c["path"] == "src/modules/orders/api.js"),
        "the factory's own forwarding calls are reported at their callers: {calls:#?}"
    );
    let at = |line: u64| -> Vec<&Value> { calls.iter().filter(|c| c["line"] == line).collect() };

    let list = at(7);
    assert_eq!(list.len(), 1, "{calls:#?}");
    assert_eq!(list[0]["client"], "wrapper");
    assert_eq!(list[0]["via"], "createClient.get");
    assert_eq!(list[0]["method"], "GET");
    assert_eq!(list[0]["url"], "/api/orders/", "an imported constant");
    assert_eq!(list[0]["params"], serde_json::json!(["page"]));

    let create = at(8);
    assert_eq!(create[0]["method"], "POST");
    assert_eq!(create[0]["params"], serde_json::json!(["offerId", "quantity"]));

    // A client made by the re-exported factory in another module; an imported map indexed by
    // a variable gives one call per value.
    let routes: Vec<&str> = at(12).iter().map(|c| c["url"].as_str().unwrap()).collect();
    assert_eq!(routes, vec!["/api/catalog/list/", "/api/catalog/search/"]);
    assert_eq!(at(12)[0]["via"], "createClient.get");
    assert_eq!(at(13)[0]["url"], "/api/catalog/search/");

    assert!(at(16).is_empty(), "a client parameter without a default is no call");
    assert_eq!(calls.len(), 5, "{calls:#?}");

    let literals: Vec<&str> = result["literals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["url"].as_str().unwrap())
        .collect();
    assert_eq!(literals, vec!["/api/ping/"], "the constants the calls use are no longer outside calls");
}

const USERS_API: &str = r#"import { get } from '../../shared/api/request'
import { makeClient } from '../../shared/http/barrel'
import send from '../../shared/api/send'
import remove, { api, ROUTES } from '../../shared/api/extra'

export const loadUsers = () => get('/api/users/', { page: 1 })
const http = makeClient()
export const ping = () => http.get('/api/ping/')
export const save = (user) => send('/api/users/save/', { name: user.name })
export const drop = (id) => remove(`/api/users/${id}/`)
export const profile = () => api.get(ROUTES.profile)
;(function () {
  const get = (url) => $.get(url)
  get('/api/local/')
})()
"#;

const EXTRA_API: &str = r#"import axios from 'axios'

const api = axios.create()
const ROUTES = { profile: '/api/profile/', other: '/api/other/' }
const remove = (url) => api.delete(url)

export default remove
export { api, ROUTES }
"#;

#[test]
fn calls_through_chains_of_exports_barrels_and_default_wrappers() {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write_client(root);
    // factory → index → client → wrapper around the client → caller: one link per round.
    write(
        root,
        "src/shared/api/request.js",
        "import { http } from '../http/instance'\n\nexport function get(url, params) {\n  return http.get(url, { params })\n}\n",
    );
    write(
        root,
        "src/shared/http/barrel.js",
        "import { createClient } from './createClient'\n\nexport { createClient as makeClient }\n",
    );
    write(
        root,
        "src/shared/api/send.js",
        "import axios from 'axios'\n\nconst api = axios.create()\n\nexport default function send(url, data) {\n  return api.post(url, data)\n}\n",
    );
    write(root, "src/shared/api/extra.js", EXTRA_API);
    write(root, "src/modules/users/api.js", USERS_API);
    run(root, cache.path(), &["rebuild"]);

    let result = run(root, cache.path(), &["http-calls", "--format", "json"]);
    let calls = result["calls"].as_array().unwrap();
    let shown: Vec<(u64, &str, &str, &str)> = calls
        .iter()
        .map(|c| {
            (
                c["line"].as_u64().unwrap(),
                c["via"].as_str().unwrap_or(""),
                c["method"].as_str().unwrap_or(""),
                c["url"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        shown,
        vec![
            (6, "get", "GET", "/api/users/"),
            (8, "createClient.get", "GET", "/api/ping/"),
            (9, "send", "POST", "/api/users/save/"),
            (10, "remove", "DELETE", "/api/users/{id}/"),
            (11, "api.get", "GET", "/api/profile/"),
            (14, "get", "GET", "/api/local/"),
        ],
        "`export default` of a wrapper variable, `export {{ … }}` of an instance and of an object \
         constant, a local name beside an imported one: {calls:#?}"
    );
    assert!(calls.iter().all(|c| c["path"] == "src/modules/users/api.js"));
    assert_eq!(calls[0]["params"], serde_json::json!(["page"]));
}

#[test]
fn scripts_without_modules_still_have_calls() {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "js/legacy.js", "$.get('/api/legacy/', { id: 1 })\n");
    write(
        root,
        "local/templates/main/page/template.php",
        "<div></div>\n<script>\n  $.post('/ajax/save.php', { a: 1 })\n</script>\n",
    );
    run(root, cache.path(), &["rebuild"]);
    let result = run(root, cache.path(), &["http-calls", "--format", "json"]);
    let urls: Vec<&str> = result["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["url"].as_str().unwrap())
        .collect();
    assert_eq!(urls, vec!["/api/legacy/", "/ajax/save.php"]);
}
