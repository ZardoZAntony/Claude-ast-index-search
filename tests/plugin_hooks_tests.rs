//! Plugin hooks: usage rules at session start in indexed projects only, and the note on grep/rg searches for
//! code symbols — without a permission decision, throttled per project.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const SCRIPTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/plugin/hooks/scripts");

struct Project {
    root: TempDir,
    cache: TempDir,
    state: TempDir,
}

fn project(indexed: bool) -> Project {
    let project = Project {
        root: TempDir::new().unwrap(),
        cache: TempDir::new().unwrap(),
        state: TempDir::new().unwrap(),
    };
    let root = project.root.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/OrderDto.php"),
        "<?php\nnamespace App\\Order;\n\nfinal class OrderDto {}\n",
    )
    .unwrap();
    if indexed {
        let out = Command::new(env!("CARGO_BIN_EXE_ast-index"))
            .current_dir(root)
            .arg("rebuild")
            .env("AST_INDEX_CACHE_DIR", project.cache.path())
            .env("AST_INDEX_DISABLE_GC", "1")
            .env_remove("AST_INDEX_DB_PATH")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    project
}

fn path_with_binary() -> String {
    let bin_dir: PathBuf = Path::new(env!("CARGO_BIN_EXE_ast-index")).parent().unwrap().into();
    format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default())
}

fn hook(project: &Project, script: &str, input: &str, interval: &str) -> String {
    let mut child = Command::new("bash")
        .arg(format!("{SCRIPTS}/{script}"))
        .current_dir(project.root.path())
        .env("PATH", path_with_binary())
        .env("CLAUDE_PROJECT_DIR", project.root.path())
        .env("AST_INDEX_CACHE_DIR", project.cache.path())
        .env("AST_INDEX_DISABLE_GC", "1")
        .env("AST_INDEX_REMINDER_STATE_DIR", project.state.path())
        .env("AST_INDEX_REMINDER_INTERVAL", interval)
        .env_remove("AST_INDEX_DB_PATH")
        .env_remove("AST_INDEX_HOOK_SKIP_REMINDER")
        .env_remove("AST_INDEX_HOOK_SKIP_CONTEXT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn grep(pattern: &str) -> String {
    serde_json::json!({"tool_name": "Grep", "tool_input": {"pattern": pattern}}).to_string()
}

fn bash(command: &str) -> String {
    serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}}).to_string()
}

/// The note, or None when the hook stays silent. A note never carries a permission decision.
fn note(project: &Project, input: &str) -> Option<String> {
    let out = hook(project, "search-reminder.sh", input, "0");
    if out.trim().is_empty() {
        return None;
    }
    let json: Value = serde_json::from_str(&out).unwrap();
    let output = &json["hookSpecificOutput"];
    assert_eq!(output["hookEventName"], "PreToolUse");
    assert!(output.get("permissionDecision").is_none(), "the hook must not approve the call: {out}");
    Some(output["additionalContext"].as_str().unwrap().to_string())
}

#[test]
fn session_start_gives_rules_only_to_indexed_projects() {
    let indexed = project(true);
    let rules = hook(&indexed, "session-start-context.sh", "{}", "0");
    assert!(rules.contains("ast-index impact '<FQN>'"), "{rules}");
    assert!(rules.contains("ast-index callers '<Type>::<method>'"), "{rules}");

    let bare = project(false);
    assert_eq!(hook(&bare, "session-start-context.sh", "{}", "0"), "");
}

#[test]
fn grep_tool_for_a_code_symbol_gets_a_note() {
    let p = project(true);
    assert!(note(&p, &grep("OrderDto")).unwrap().contains("'OrderDto'"));
    assert!(note(&p, &grep(r"\bOrderRepository::findById\b")).is_some());
    assert!(note(&p, &grep("useSizeGuide")).is_some());
    let both = note(&p, &grep("OrderDto|OrderRepository")).unwrap();
    assert!(both.contains("'OrderDto, OrderRepository'"), "{both}");
    assert_eq!(note(&p, &grep("OrderDto|cron")), None, "every alternative must be a symbol");
}

#[test]
fn plain_words_and_regexes_get_no_note() {
    let p = project(true);
    for pattern in ["cron", "TODO", "Order", "Order.*Dto", "foo|bar", "use strict"] {
        assert_eq!(note(&p, &grep(pattern)), None, "{pattern}");
    }
}

#[test]
fn bash_searches_over_files_get_a_note() {
    let p = project(true);
    assert!(note(&p, &bash("rg -n -w useSizeGuide src --glob '!dist'")).is_some());
    assert!(note(&p, &bash("cd src && rg -e getList -g '*.php'")).is_some());
    assert!(note(&p, &bash("grep -n OrderDto src/OrderDto.php")).is_some());
    assert!(note(&p, &bash("git grep -n OrderDto")).is_some());
    assert!(note(&p, &bash(r#"grep -rn "FiasNalogVersionSource\\|HttpClient" src"#)).is_some());
    let fqn = note(&p, &bash(r#"grep -rn "App\\\\Order\\\\OrderDto" src"#)).unwrap();
    assert!(fqn.contains(r"'App\Order\OrderDto'"), "{fqn}");
}

#[test]
fn bash_filters_and_other_commands_get_no_note() {
    let p = project(true);
    for command in [
        "ps aux | grep PhpStorm",
        "git log --oneline | grep OrderDto",
        "grep OrderDto",
        "rg TODO",
        "ls -la",
        "rg -g '*.php' -t php cron",
    ] {
        assert_eq!(note(&p, &bash(command)), None, "{command}");
    }
}

#[test]
fn note_is_throttled_per_project() {
    let p = project(true);
    let input = grep("OrderDto");
    assert!(!hook(&p, "search-reminder.sh", &input, "600").trim().is_empty());
    assert_eq!(hook(&p, "search-reminder.sh", &input, "600").trim(), "");
}

#[test]
fn project_without_index_gets_no_note() {
    let p = project(false);
    assert_eq!(note(&p, &grep("OrderDto")), None);
}
