//! Watch mode — automatically update index on file changes

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use colored::Colorize;
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;

use crate::commands::{self, management::ScopedEnvVar};
use crate::{db, indexer, minified, parsers};

fn open_watch_lock(root: &Path) -> Result<std::fs::File> {
    let lock_path = db::get_db_path(root)?.with_extension("watch.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(Into::into)
}

/// Acquire an exclusive lock for watch mode, scoped to the resolved project
/// database. The lock remains held until the returned file is dropped.
fn try_acquire_watch_lock(root: &Path) -> Result<Option<std::fs::File>> {
    use fs2::FileExt;
    let file = open_watch_lock(root)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.set_len(0)?;
            let mut f = &file;
            write!(f, "{}", std::process::id())?;
            Ok(Some(file))
        }
        Err(error) if db::lock_is_contended(&error) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Report whether this project's watch lock is currently held. This probes
/// the same lock as [`cmd_watch`], so another project's watcher cannot affect
/// the result.
fn is_watch_running(root: &Path) -> Result<bool> {
    use fs2::FileExt;
    let file = open_watch_lock(root)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(false),
        Err(error) if db::lock_is_contended(&error) => Ok(true),
        Err(error) => Err(error.into()),
    }
}

/// Print a stable watcher status. Callers that only need the exit status use
/// `--quiet`; the CLI exits successfully only while this project is watched.
pub fn cmd_watch_status(root: &Path, quiet: bool, format: &str) -> Result<bool> {
    let watching = is_watch_running(root)?;
    if !quiet {
        if format == "json" {
            println!(r#"{{"watching":{watching}}}"#);
        } else if watching {
            println!("watching");
        } else {
            println!("not-watching");
        }
        std::io::stdout().flush()?;
    }
    Ok(watching)
}

/// Watch for file changes and incrementally update the index
pub fn cmd_watch(root: &Path) -> Result<()> {
    // Held for the complete watch loop. The lease file lives outside the
    // project cache directory, so stale-cache GC cannot unlink this index
    // while the watcher is idle between SQLite connections.
    let _cache_lease = db::acquire_project_lease(root)?;
    let Some(initial) = db::open_existing_db_leased(root)? else {
        println!(
            "{}",
            "Index not found. Run 'ast-index rebuild' first.".red()
        );
        return Ok(());
    };
    drop(initial);

    // Ensure only one watch process runs at a time
    let _lock = match try_acquire_watch_lock(root)? {
        Some(lock) => lock,
        None => {
            eprintln!("{}", "Another ast-index watch is already running.".yellow());
            return Ok(());
        }
    };

    println!(
        "{}",
        format!("Watching for changes in {}...", root.display()).cyan()
    );
    println!("{}", "Press Ctrl+C to stop.".dimmed());

    let (tx, rx) = mpsc::channel();

    let mut debouncer = new_debouncer(Duration::from_millis(500), tx)?;
    debouncer.watcher().watch(root, RecursiveMode::Recursive)?;
    let mut filter = ChangeFilter::new(root);

    loop {
        match rx.recv() {
            Ok(Ok(events)) => {
                let changed: Vec<_> = events
                    .iter()
                    .filter(|e| filter.is_relevant(&e.path))
                    .collect();

                if changed.is_empty() {
                    continue;
                }

                filter.since = SystemTime::now();
                let start = Instant::now();
                let file_count = changed.len();
                let shown: Vec<String> = changed
                    .iter()
                    .take(3)
                    .map(|e| {
                        let path = e.path.strip_prefix(root).unwrap_or(&e.path);
                        format!("{} ({:?})", path.display(), e.kind)
                    })
                    .collect();
                eprintln!(
                    "{}",
                    format!(
                        "Detected {} changed file(s): {}; updating...",
                        file_count,
                        shown.join(", ")
                    )
                    .yellow()
                );

                match update_index(root) {
                    Ok((updated, deleted)) => {
                        if updated > 0 || deleted > 0 {
                            eprintln!(
                                "{}",
                                format!(
                                    "Updated {} files, deleted {} ({:?})",
                                    updated,
                                    deleted,
                                    start.elapsed()
                                )
                                .green()
                            );
                        } else {
                            eprintln!(
                                "{}",
                                format!("No index changes ({:?})", start.elapsed()).dimmed()
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("{}", format!("Update error: {}", e).red());
                    }
                }
            }
            Ok(Err(err)) => {
                eprintln!("{}", format!("Watch error: {}", err).red());
            }
            Err(e) => {
                eprintln!("{}", format!("Channel error: {}", e).red());
                break;
            }
        }
    }

    Ok(())
}

/// Which watcher events call for an update: changes to files the indexer would index.
///
/// The watcher also reports reads — the update's own walk opens directories such as
/// `components/auth.mts/`, and a running site reads ignored files — so an event counts only for
/// a file (not a directory) modified since the last update began, or removed, that passes the
/// same rules as indexing: a supported extension, not in a skipped directory, hidden only where
/// `include_hidden` allows, not gitignored, not excluded by the config.
const MTIME_MARGIN: Duration = Duration::from_secs(2);

struct ChangeFilter {
    root: PathBuf,
    hidden: indexer::HiddenPolicy,
    exclude: Option<ignore::gitignore::Gitignore>,
    /// `.gitignore` of each directory seen so far (`None` when it has none).
    gitignores: HashMap<PathBuf, Option<ignore::gitignore::Gitignore>>,
    since: SystemTime,
}

impl ChangeFilter {
    fn new(root: &Path) -> Self {
        let exclude = indexer::load_config_quiet(root)
            .and_then(|c| c.exclude)
            .filter(|patterns| !patterns.is_empty())
            .and_then(|patterns| {
                let mut gb = ignore::gitignore::GitignoreBuilder::new(root);
                for p in &patterns {
                    gb.add_line(None, p).ok();
                }
                gb.build().ok()
            });
        Self {
            root: root.to_path_buf(),
            hidden: indexer::HiddenPolicy::for_root(root),
            exclude,
            gitignores: HashMap::new(),
            since: SystemTime::now(),
        }
    }

    fn is_relevant(&mut self, path: &Path) -> bool {
        let supported = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(parsers::is_supported_extension);
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let skipped_dir = relative.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|name| indexer::EXCLUDED_DIRS.contains(&name))
        });
        if !supported
            || skipped_dir
            || minified::skip_by_name(path)
            || !self.hidden.allows_path(&self.root, path)
        {
            return false;
        }
        if self
            .exclude
            .as_ref()
            .is_some_and(|m| m.matched_path_or_any_parents(path, false).is_ignore())
            || self.is_gitignored(path)
        {
            return false;
        }
        match std::fs::metadata(path) {
            // File times are coarser than the clock: a margin keeps an edit made right as an
            // update began; at worst it costs one more update.
            Ok(meta) => {
                meta.is_file()
                    && meta
                        .modified()
                        .is_ok_and(|t| t + MTIME_MARGIN >= self.since)
            }
            Err(_) => true,
        }
    }

    /// The deepest `.gitignore` with a verdict on `path` decides, then `.git/info/exclude`.
    fn is_gitignored(&mut self, path: &Path) -> bool {
        let mut dirs: Vec<PathBuf> = path
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(&self.root))
            .map(Path::to_path_buf)
            .collect();
        dirs.reverse();
        let mut ignored = self
            .gitignore_at(&self.root.join(".git/info"), "exclude")
            .is_some_and(|g| g.matched_path_or_any_parents(path, false).is_ignore());
        for dir in dirs {
            let Some(gitignore) = self.gitignore_at(&dir, ".gitignore") else {
                continue;
            };
            match gitignore.matched_path_or_any_parents(path, false) {
                ignore::Match::Ignore(_) => ignored = true,
                ignore::Match::Whitelist(_) => ignored = false,
                ignore::Match::None => {}
            }
        }
        ignored
    }

    fn gitignore_at(&mut self, dir: &Path, name: &str) -> Option<&ignore::gitignore::Gitignore> {
        let root = self.root.clone();
        self.gitignores
            .entry(dir.join(name))
            .or_insert_with(|| {
                let file = dir.join(name);
                file.is_file().then(|| {
                    // Patterns in `.git/info/exclude` are relative to the project root.
                    let base = if name == "exclude" {
                        root.as_path()
                    } else {
                        dir
                    };
                    let mut gb = ignore::gitignore::GitignoreBuilder::new(base);
                    gb.add(&file);
                    gb.build().ok()
                })?
            })
            .as_ref()
    }
}

fn update_index(root: &Path) -> Result<(usize, usize)> {
    // Watch is long-lived, so take the common mutation lock only for one
    // coalesced update batch. Readers remain concurrent through SQLite WAL.
    let _mutation_guard = db::acquire_rebuild_guard(root)?;
    let _experimental_fast_rebuild_env = ScopedEnvVar::set_bool(
        "AST_INDEX_EXPERIMENTAL_FAST_REBUILD",
        commands::try_is_experimental_fast_rebuild_enabled(root)?,
    );

    let mut conn = db::open_existing_db_leased(root)?
        .ok_or_else(|| anyhow::anyhow!("Index was cleared; run 'ast-index rebuild' first."))?;

    // Honour .ast-index.yaml so watch stays scoped to the same paths as rebuild/update.
    let config = indexer::load_config(root).unwrap_or_default();
    let config_include = config.include.as_deref();
    let exclude_matcher: Option<ignore::gitignore::Gitignore> = config
        .exclude
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(|patterns| {
            let mut gb = ignore::gitignore::GitignoreBuilder::new(root);
            for p in patterns {
                gb.add_line(None, p).ok();
            }
            gb.build().ok()
        })
        .flatten();

    let (updated, changed, deleted) = indexer::update_directory_incremental(
        &mut conn,
        root,
        false,
        config_include,
        exclude_matcher.as_ref(),
    )?;
    let _ = changed; // suppress unused
    Ok((updated, deleted))
}

#[cfg(test)]
mod change_filter_tests {
    use super::*;

    #[test]
    fn only_indexable_modified_files_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let write = |rel: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "<?php\n").unwrap();
            path
        };
        write(".gitignore");
        std::fs::write(root.join(".gitignore"), "/bitrix\n").unwrap();
        std::fs::create_dir_all(root.join("local/components/auth.mts")).unwrap();
        let old = write("src/Old.php");
        let past = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();

        let mut filter = ChangeFilter::new(root);
        let fresh = write("src/New.php");
        let ignored = write("bitrix/cache/Page.php");
        let hidden = write(".idea/Hidden.php");

        assert!(filter.is_relevant(&fresh));
        assert!(
            !filter.is_relevant(&old),
            "not modified since the filter started"
        );
        assert!(!filter.is_relevant(&ignored), "gitignored");
        assert!(!filter.is_relevant(&hidden), "hidden");
        assert!(
            !filter.is_relevant(&root.join("local/components/auth.mts")),
            "a directory"
        );
        assert!(
            filter.is_relevant(&root.join("src/Removed.php")),
            "a removed file"
        );
    }
}
