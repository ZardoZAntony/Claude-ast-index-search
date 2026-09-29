# Releases

Versioning is SemVer (`MAJOR.MINOR.PATCH`). Bugfix → patch. Additive
feature → minor. Breaking CLI change → major (rare — keep old commands
aliased where feasible).

## Workflow

**Always two commits: the change, then the bump.** Never bundle them.

```bash
# 1. Fix or feature.
#    Edit code.
#    Add a short changelog entry under "## Changelog" in README.md.
#    The entry goes ABOVE the previous one, headed `### X.Y.Z`.
git add src/… tests/… README.md
git commit -m "Fix X when Y"

# 2. Bump on a clean tree.
./scripts/bump.sh 3.38.1
```

`scripts/bump.sh` is the only sanctioned way to change the version. It
rewrites the version in lockstep in:

- `Cargo.toml` — `version`
- `README.md` — title and changelog section
- `plugin/.claude-plugin/plugin.json`
- `.claude-plugin/plugin.json`
- `.claude-plugin/marketplace.json`

It then runs `cargo build --release`, commits `Bump to vX.Y.Z` with `git
add -A`, creates the `vX.Y.Z` tag, and pushes.

**Why the separate commit.** The bump script uses `git add -A`. If your
fix is unstaged when you run it, the fix is silently absorbed into the
bump commit and you lose the ability to revert/cherry-pick it independently.

## Tags

Tags are immutable. If a tagged commit ships and then something needs a
further change, cut the next patch — don't delete or move the tag.

```
v3.38.0 shipped with a bug
→ fix the bug on main
→ ./scripts/bump.sh 3.38.1
```

## After the tag

GitHub Actions (`.github/workflows/release.yml`) automatically:

- builds five binaries (darwin-arm64, darwin-x86_64, linux-x86_64,
  linux-arm64, windows-x86_64);
- creates the GitHub Release with artifacts.

No further local steps are required.

## Changelog tone

One bullet per change, imperative mood, user-facing impact first, internal
mechanics second.

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
- **Bumping on a dirty tree.** Unstaged fixes get absorbed into the bump
  commit. Commit first.
- **Moving a tag.** Cut a new patch version instead.
- **Pushing with `--force`** to the tag or to `main`. Even if you think
  the remote is "obviously wrong", ask first.
