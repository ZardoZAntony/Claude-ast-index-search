//! `callees 'Type::method'` on a small project shaped like a Bitrix event chain:
//! subscription bridge → handler → dispatcher interface with one implementation.

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

const BASE_BRIDGE: &str = r#"<?php
namespace Share\Event;

abstract class AbstractBridge
{
    /**
     * @template T of HandlerInterface
     * @param class-string<T> $handlerClass
     * @return T
     */
    protected static function getHandler(string $handlerClass): HandlerInterface
    {
        return Locator::instance()->get($handlerClass);
    }
}
"#;

const BRIDGE: &str = r#"<?php
namespace Basket\Event;

use Share\Event\AbstractBridge;

final class BasketBridge extends AbstractBridge
{
    public static function onSaved(Event $event): void
    {
        /** @var ChangedHandler $handler */
        $handler = self::getHandler(ChangedHandler::class);
        $handler->handle($event);
    }
}
"#;

const HANDLER: &str = r#"<?php
namespace Basket\Event;

use Share\Cache\DispatcherInterface;
use Share\Cache\EntityType;

final readonly class ChangedHandler
{
    public function __construct(private DispatcherInterface $dispatcher) {}

    public function handle(Event $event): void
    {
        $this->dispatcher->dispatch(
            1,
            EntityType::BASKET_SUMMARY,
        );
    }
}
"#;

const DISPATCHER: &str = r#"<?php
namespace Share\Cache;

interface DispatcherInterface
{
    public function dispatch(int $id, EntityType $type): void;
}
"#;

const DISPATCH_USE_CASE: &str = r#"<?php
namespace Share\Cache;

final class DispatchUseCase implements DispatcherInterface
{
    public function dispatch(int $id, EntityType $type): void
    {
        $this->queue($id);
    }

    private function queue(int $id): void {}
}
"#;

fn project() -> (TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "share/Event/AbstractBridge.php", BASE_BRIDGE);
    write(root, "basket/Event/BasketBridge.php", BRIDGE);
    write(root, "basket/Event/ChangedHandler.php", HANDLER);
    write(root, "share/Cache/DispatcherInterface.php", DISPATCHER);
    write(root, "share/Cache/DispatchUseCase.php", DISPATCH_USE_CASE);
    write(root, "share/Cache/EntityType.php", "<?php\nnamespace Share\\Cache;\n\nenum EntityType: string\n{\n    case BASKET_SUMMARY = 'basket_summary';\n}\n");
    run(root, cache.path(), &["rebuild"]);
    (tmp, cache)
}

fn find<'v>(calls: &'v Value, call: &str) -> &'v Value {
    calls
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["call"] == call)
        .unwrap_or_else(|| panic!("{call} not in {calls:#}"))
}

#[test]
fn follows_bridge_to_handler_to_interface_dispatch() {
    let (tmp, cache) = project();
    let tree = run(
        tmp.path(),
        cache.path(),
        &["callees", "Basket\\Event\\BasketBridge::onSaved", "--depth", "2", "--format", "json"],
    );
    assert_eq!(tree["method"], "Basket\\Event\\BasketBridge::onSaved");
    assert_eq!(tree["path"], "basket/Event/BasketBridge.php");

    let get_handler = find(&tree["calls"], "Basket\\Event\\BasketBridge::getHandler");
    assert_eq!(get_handler["declared_in"], "Share\\Event\\AbstractBridge");
    assert_eq!(get_handler["classes"][0], "Basket\\Event\\ChangedHandler");

    let handle = find(&tree["calls"], "Basket\\Event\\ChangedHandler::handle");
    assert_eq!(handle["line"], 12);
    assert_eq!(handle["resolved_by"], "@var");
    assert_eq!(
        handle["declared_at"],
        "basket/Event/ChangedHandler.php:11-17"
    );

    let dispatch = find(&handle["children"], "Share\\Cache\\DispatcherInterface::dispatch");
    assert_eq!(dispatch["line"], 13, "a call over several lines is one call at its start");
    assert_eq!(dispatch["path"], "basket/Event/ChangedHandler.php");
    assert_eq!(dispatch["abstract"], true);
    assert_eq!(dispatch["implementations"][0], "Share\\Cache\\DispatchUseCase");
    assert_eq!(dispatch["constants"][0]["name"], "EntityType::BASKET_SUMMARY");
    assert_eq!(dispatch["constants"][0]["fqn"], "Share\\Cache\\EntityType::BASKET_SUMMARY");
    assert!(dispatch.get("children").is_none(), "implementations expand only on request");
}

#[test]
fn implementations_flag_expands_and_stop_cuts_the_tree() {
    let (tmp, cache) = project();
    let tree = run(
        tmp.path(),
        cache.path(),
        &[
            "callees",
            "Basket\\Event\\ChangedHandler::handle",
            "--depth",
            "3",
            "--implementations",
            "--format",
            "json",
        ],
    );
    let dispatch = find(&tree["calls"], "Share\\Cache\\DispatcherInterface::dispatch");
    let implementation = find(&dispatch["children"], "Share\\Cache\\DispatchUseCase::dispatch");
    assert_eq!(implementation["kind"], "implementation");
    find(&implementation["children"], "Share\\Cache\\DispatchUseCase::queue");

    let stopped = run(
        tmp.path(),
        cache.path(),
        &[
            "callees",
            "Basket\\Event\\BasketBridge::onSaved",
            "--depth",
            "3",
            "--stop",
            "Basket\\Event\\ChangedHandler",
            "--format",
            "json",
        ],
    );
    let handle = find(&stopped["calls"], "Basket\\Event\\ChangedHandler::handle");
    assert_eq!(handle["stopped"], true);
    assert!(handle.get("children").is_none());
}

#[test]
fn a_trait_method_is_expanded_for_the_class_using_it() {
    let tmp = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "src/Notifies.php", "<?php\nnamespace App;\n\ntrait Notifies\n{\n    public function notify(): void\n    {\n        $this->send();\n    }\n}\n");
    write(
        root,
        "src/Mailer.php",
        "<?php\nnamespace App;\n\nfinal class Mailer\n{\n    use Notifies;\n\n    public function run(): void\n    {\n        $this->notify();\n    }\n\n    public function send(): void\n    {\n    }\n}\n",
    );
    run(root, cache.path(), &["rebuild"]);
    let tree = run(root, cache.path(), &["callees", "App\\Mailer::run", "--depth", "2", "--format", "json"]);
    let notify = find(&tree["calls"], "App\\Mailer::notify");
    assert_eq!(notify["declared_in"], "App\\Notifies");
    let send = find(&notify["children"], "App\\Mailer::send");
    assert!(send.get("undeclared").is_none(), "{send:#?}");
    assert_eq!(send["declared_at"], "src/Mailer.php:13-15");
}
