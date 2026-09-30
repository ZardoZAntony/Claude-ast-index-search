# Releases

Versioning is SemVer (`MAJOR.MINOR.PATCH`). Bugfix → patch. Additive
feature → minor. Breaking CLI change → major (rare — keep old commands
aliased where feasible).

## Version

The fork keeps upstream's base version with its own suffix: `3.55.1-php.2`, `3.55.1-php.3`… The suffix grows
with each release; move the base (`3.56.0-php.1`) only when merging a new upstream release.

## Workflow

**Always two steps: commit the change, then release on a clean tree.**

```bash
# 1. The change, committed on its own.
git add src/… tests/… README.md
git commit -m "Fix X when Y"

# 2. Release it.
scripts/bump.sh 3.55.1-php.3 --notes notes.md    # or without --notes: notes from commit subjects
```

`scripts/bump.sh` is the only sanctioned way to change the version. It refuses a dirty tree, an existing tag or a
branch without upstream, then:

- sets the version in `Cargo.toml` and the plugin manifests (`plugin/.claude-plugin`, `plugin/.codex-plugin`,
  `plugin/.cursor-plugin`, `.claude-plugin/plugin.json`, `.claude-plugin/marketplace.json`);
- builds the release binary, checks `ast-index version`, runs `scripts/test.sh`;
- commits `Bump to vX` (only those files and `Cargo.lock`), creates the annotated tag, pushes the branch and the
  tag to the branch's upstream (the fork's `main` tracks `fork/main`);
- publishes the GitHub release with `ast-index-linux-x86_64.tar.gz` (the binary at the archive root — README's
  `curl … | tar -xz -C ~/.local/bin` relies on it). Network steps are retried; the release is created once and the
  asset uploaded with `--clobber`, so a rerun after a dropped connection is safe.

`--no-release` stops after the push, `--no-push` after the local tag. It runs on Linux x86_64 (the release asset
is built there, glibc of the build machine — 2.39 now); `perl` does the in-place edits, so it also runs on macOS
with `--no-release`.

GitHub Actions do not run in the fork, so no other binaries are built: elsewhere `cargo install --git …`.

After a release: stop a running `ast-index watch` (it keeps the old binary), install the new one, update the
plugin (`claude plugin marketplace update ast-index-php && claude plugin update ast-index@ast-index-php`).

## Tags

Tags are immutable. If a tagged commit ships and then something needs a further change, release the next
version — don't delete or move the tag.

## Release notes tone

The fork keeps no changelog in README: what changed goes into the release notes (`--notes`). One bullet per
change, user-facing impact first, internal mechanics second.

Good:

```markdown
### 3.38.1
- **Fix ambiguous paths in search output under extra roots** — previously
  `search`/`symbol`/`refs`/… printed stored relative paths without
  indicating which root they belonged to. Now, when any extra root is
  configured, results resolve to absolute paths by probing roots on disk.
```

Bad:

```markdown
### 3.38.1
- Refactored index.rs.
```

## Anti-patterns

- **Hand-editing any version string.** Use `bump.sh`.
- **Releasing a dirty tree.** `bump.sh` refuses it: the binary would carry changes no commit holds.
- **Moving a tag.** Cut a new patch version instead.
- **Pushing with `--force`** to the tag or to `main`. Even if you think
  the remote is "obviously wrong", ask first.
