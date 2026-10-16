//! `routes`: routing files with prefix groups, controller actions with the prefilters Bitrix
//! runs, and component actions.

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

const ROUTES: &str = r#"<?php
use Bitrix\Main\Routing\RoutingConfigurator;
use Shop\Controller\BasketController;
use Shop\Controller\FormController;

return static function (RoutingConfigurator $routes): void {
    $routes->post('/api/v1/basket/items', [BasketController::class, 'addAction']);
    // $routes->get('/api/v1/basket/old', [BasketController::class, 'oldAction']);
    $routes->prefix('api')->group(function (RoutingConfigurator $routes) {
        $routes->post('form/send/', [FormController::class, 'send']);
    });
};
"#;

const ABSTRACT_CONTROLLER: &str = r#"<?php
namespace Shop\Controller;

use Bitrix\Main\Engine\Controller;
use Shop\Filter\LoggerFilter;

abstract class AbstractController extends Controller
{
    protected function getDefaultPreFilters(): array
    {
        return [
            new LoggerFilter(),
        ];
    }
}
"#;

const BASKET_CONTROLLER: &str = r#"<?php
namespace Shop\Controller;

use Shop\Attribute\RateLimit;
use Shop\Attribute\WithoutCsrf;
use Shop\Filter\AuthFilter;

#[WithoutCsrf]
final class BasketController extends AbstractController
{
    protected function getDefaultPreFilters(): array
    {
        return array_merge(parent::getDefaultPreFilters(), [new AuthFilter('basket')]);
    }

    #[RateLimit(5)]
    public function addAction(): array
    {
        return [];
    }
}
"#;

const FORM_CONTROLLER: &str = r#"<?php
namespace Shop\Controller;

use Bitrix\Main\Engine\ActionFilter;
use Bitrix\Main\Engine\Controller;
use Shop\Filter\LoggerFilter;

final class FormController extends Controller
{
    public function configureActions(): array
    {
        return [
            'send' => [
                '-prefilters' => [ActionFilter\Csrf::class, ActionFilter\Authentication::class],
                '+prefilters' => [new LoggerFilter()],
            ],
        ];
    }

    public function sendAction(): array
    {
        return [];
    }
}
"#;

const COMPONENT: &str = r#"<?php
use Bitrix\Main\Engine\ActionFilter;
use Bitrix\Main\Engine\Contract\Controllerable;

class UsersList extends CBitrixComponent implements Controllerable
{
    public function configureActions(): array
    {
        return [
            'getFiltered' => ['prefilters' => [new ActionFilter\HttpMethod(['POST'])]],
        ];
    }

    public function getFilteredAction(array $filters): array
    {
        return [];
    }

    public function plainAction(): array
    {
        return [];
    }
}
"#;

fn project() -> (TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "local/modules/shop.basket/routes.php", ROUTES);
    write(root, "local/modules/shop.basket/lib/Controller/AbstractController.php", ABSTRACT_CONTROLLER);
    write(root, "local/modules/shop.basket/lib/Controller/BasketController.php", BASKET_CONTROLLER);
    write(root, "local/modules/shop.basket/lib/Controller/FormController.php", FORM_CONTROLLER);
    write(root, "local/components/shop/users.list/class.php", COMPONENT);
    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

fn classes(filters: &Value) -> Vec<String> {
    filters
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            let class = f["class"].as_str().unwrap().to_string();
            if f["implicit"] == true {
                format!("{class}*")
            } else {
                class
            }
        })
        .collect()
}

#[test]
fn routes_carry_handlers_attributes_and_prefilters_in_effect() {
    let (tmp, cache) = project();
    let entries = run(tmp.path(), cache.path(), &["routes", "--kind", "route", "--format", "json"]);
    let entries = entries.as_array().unwrap();
    assert_eq!(entries.len(), 2, "the commented route is no route: {entries:#?}");

    let add = &entries[0];
    assert_eq!(add["method"], "POST");
    assert_eq!(add["path"], "/api/v1/basket/items");
    assert_eq!(add["line"], 7);
    assert_eq!(add["handler"], "Shop\\Controller\\BasketController::addAction");
    assert_eq!(add["action"], "add");
    assert_eq!(add["attributes"], serde_json::json!(["#[RateLimit(5)]"]));
    assert_eq!(add["class_attributes"][0]["attribute"], "#[WithoutCsrf]");
    assert_eq!(classes(&add["prefilters"]), vec!["LoggerFilter", "AuthFilter"]);
    assert_eq!(add["prefilters"][0]["from"], "Shop\\Controller\\AbstractController");
    assert_eq!(add["prefilters"][1]["from"], "Shop\\Controller\\BasketController");
    assert_eq!(add["prefilters"][1]["args"], serde_json::json!(["basket"]));

    let send = &entries[1];
    assert_eq!(send["path"], "/api/form/send/");
    assert_eq!(send["handler"], "Shop\\Controller\\FormController::sendAction");
    // Core defaults (the core is outside this index), minus Csrf and Authentication, plus
    // the configured LoggerFilter.
    assert_eq!(classes(&send["prefilters"]), vec!["HttpMethod", "LoggerFilter"]);
    assert_eq!(send["prefilters"][0]["from"], "Bitrix\\Main\\Engine\\Controller");
    assert_eq!(send["prefilters"][1]["from"], "configureActions");
}

#[test]
fn component_actions_use_their_configuration_or_core_defaults() {
    let (tmp, cache) = project();
    let entries = run(tmp.path(), cache.path(), &["routes", "--kind", "component", "--format", "json"]);
    let entries = entries.as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let filtered = entries.iter().find(|e| e["action"] == "getFiltered").unwrap();
    assert_eq!(filtered["component"], "shop:users.list");
    assert_eq!(filtered["mode"], "class");
    assert_eq!(filtered["handler"], "UsersList::getFilteredAction");
    assert_eq!(classes(&filtered["prefilters"]), vec!["HttpMethod", "Csrf*"]);
    let plain = entries.iter().find(|e| e["action"] == "plain").unwrap();
    assert_eq!(classes(&plain["prefilters"]), vec!["Authentication", "HttpMethod", "Csrf"]);
}
