//! `events` (Bitrix event subscriptions) and `new '<FQN>'` (instantiations with folded
//! arguments) on a small module.

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

const EVENTS: &str = r#"<?php

declare(strict_types=1);

use Bitrix\Main\EventManager;
use Shop\Delivery\Bridge;

$eventManager = EventManager::getInstance();

foreach (['OnAfterAdd', 'OnAfterUpdate'] as $eventType) {
    $eventManager->addEventHandler('', 'TariffRule' . $eventType, [Bridge::class, 'onTariffRuleChange']);
}

$eventManager->addEventHandler(
    'sale',
    Bridge::SALE_EVENT,
    [Bridge::class, 'onBasketSaved'],
);
"#;

const BRIDGE: &str = r#"<?php
namespace Shop\Delivery;

final class Bridge extends BaseBridge
{
    public const SALE_EVENT = 'OnSale' . 'BasketItemSaved';

    public static function onTariffRuleChange(Event $event): void
    {
        self::handle($event);
    }
}
"#;

const BASE_BRIDGE: &str = r#"<?php
namespace Shop\Delivery;

abstract class BaseBridge
{
    public static function onBasketSaved(Event $event): void
    {
    }
}
"#;

const REGISTRY: &str = r#"<?php
namespace Shop\Ui;

use Shop\Ui\Dto\Descriptor;
use Shop\Ui\Filter\HttpMethod;

final class Registry
{
    public function getActions(): array
    {
        $prefilters = [new HttpMethod([HttpMethod::POST]), new Csrf()];

        return [
            new Descriptor(name: 'load', action: LoadAction::class, prefilters: $prefilters),
        ];
    }
}
"#;

fn project() -> (TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "local/modules/shop.delivery/events/events.php", EVENTS);
    write(root, "local/modules/shop.delivery/lib/Bridge.php", BRIDGE);
    write(root, "local/modules/shop.delivery/lib/BaseBridge.php", BASE_BRIDGE);
    write(root, "local/modules/shop.ui/lib/Registry.php", REGISTRY);
    write(root, "local/modules/shop.ui/lib/Dto/Descriptor.php", "<?php\nnamespace Shop\\Ui\\Dto;\n\nfinal class Descriptor\n{\n    public function __construct(public string $name, public string $action, public array $prefilters = []) {}\n}\n");
    write(root, "local/modules/shop.ui/lib/Filter/HttpMethod.php", "<?php\nnamespace Shop\\Ui\\Filter;\n\nfinal class HttpMethod\n{\n    public const POST = 'POST';\n    public function __construct(array $methods) {}\n}\n");
    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

#[test]
fn events_unroll_loops_fold_constants_and_locate_handlers() {
    let (tmp, cache) = project();
    let entries = run(tmp.path(), cache.path(), &["events", "--format", "json"]);
    let entries = entries.as_array().unwrap();
    assert_eq!(entries.len(), 2, "{entries:#?}");

    let tariff = &entries[0];
    assert_eq!(tariff["path"], "local/modules/shop.delivery/events/events.php");
    assert_eq!(tariff["line"], 11);
    assert_eq!(tariff["module"], "");
    assert_eq!(
        tariff["events"],
        serde_json::json!(["TariffRuleOnAfterAdd", "TariffRuleOnAfterUpdate"])
    );
    assert_eq!(tariff["handler"], "Shop\\Delivery\\Bridge::onTariffRuleChange");
    assert_eq!(tariff["handler_at"], "local/modules/shop.delivery/lib/Bridge.php:8-11");

    let sale = &entries[1];
    assert_eq!(sale["line"], 14, "a call over several lines is reported where it starts");
    assert_eq!(sale["events"], serde_json::json!(["OnSaleBasketItemSaved"]));
    assert_eq!(sale["handler_declared_in"], "Shop\\Delivery\\BaseBridge");
    assert_eq!(sale["handler_at"], "local/modules/shop.delivery/lib/BaseBridge.php:6-8");

    let filtered = run(
        tmp.path(),
        cache.path(),
        &["events", "--event", "basketitem", "--format", "json"],
    );
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    let other_module = run(
        tmp.path(),
        cache.path(),
        &["events", "--module", "local/modules/shop.ui", "--format", "json"],
    );
    assert!(other_module.as_array().unwrap().is_empty());
}

#[test]
fn new_lists_instantiations_with_folded_arguments() {
    let (tmp, cache) = project();
    let result = run(
        tmp.path(),
        cache.path(),
        &["new", "Shop\\Ui\\Dto\\Descriptor", "--format", "json"],
    );
    let items = result["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["path"], "local/modules/shop.ui/lib/Registry.php");
    assert_eq!(item["line"], 14);
    assert_eq!(item["in"], "Shop\\Ui\\Registry::getActions");
    assert_eq!(item["named"]["name"], "load");
    assert_eq!(item["named"]["action"], "Shop\\Ui\\LoadAction");
    let prefilters = item["named"]["prefilters"].as_array().unwrap();
    assert_eq!(prefilters[0]["new"], "Shop\\Ui\\Filter\\HttpMethod");
    assert_eq!(prefilters[0]["args"], serde_json::json!([["POST"]]));
    assert_eq!(prefilters[1]["new"], "Shop\\Ui\\Csrf");
}
