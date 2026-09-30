#!/usr/bin/env bash
# Release a version of the fork: set the version in every manifest, build and test, commit, tag, push, and publish
# the GitHub release with the Linux x86_64 binary.
#
#   scripts/bump.sh 3.55.1-php.3                   # everything; release notes from commit subjects since the last tag
#   scripts/bump.sh 3.55.1-php.3 --notes notes.md  # release notes from a file
#   scripts/bump.sh 3.55.1-php.3 --no-release      # commit, tag and push, no GitHub release
#   scripts/bump.sh 3.55.1-php.3 --no-push         # commit and tag locally only (implies --no-release)
#
# Run on a clean tree: the bump commit takes only the version files, an uncommitted fix would ship in the binary
# without a commit of its own. Tags are immutable — a mistake is fixed by the next version, never by moving a tag.
set -euo pipefail

usage() {
    sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
}

die() {
    echo "Error: $*" >&2
    exit 1
}

# github.com drops connections now and then: network steps are retried
retry() {
    local attempt
    for attempt in 1 2 3; do
        "$@" && return 0
        echo "  retry $attempt/3: $*" >&2
        sleep 5
    done
    return 1
}

VERSION=""
NOTES=""
PUSH=1
RELEASE=1
while [ $# -gt 0 ]; do
    case "$1" in
        --notes)
            NOTES="${2:-}"
            [ -n "$NOTES" ] || die "--notes needs a file"
            shift 2
            ;;
        --no-release) RELEASE=0; shift ;;
        --no-push) PUSH=0; RELEASE=0; shift ;;
        -h|--help) usage; exit 0 ;;
        -*) die "unknown option $1" ;;
        *) VERSION="$1"; shift ;;
    esac
done
[ -n "$VERSION" ] || { usage; exit 1; }
echo "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?$' \
    || die "version must be X.Y.Z or X.Y.Z-suffix, e.g. 3.55.1-php.3 (got: $VERSION)"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
TAG="v$VERSION"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
MANIFESTS=(
    plugin/.claude-plugin/plugin.json
    plugin/.codex-plugin/plugin.json
    plugin/.cursor-plugin/plugin.json
    .claude-plugin/plugin.json
    .claude-plugin/marketplace.json
)

# --- checks before anything changes ---------------------------------------------------------------------------
[ -z "$(git status --porcelain --untracked-files=no)" ] || die "the tree has uncommitted changes — commit them first"
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    die "tag $TAG exists — tags are immutable, release the next version"
fi
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
REMOTE=""
if [ "$PUSH" -eq 1 ]; then
    UPSTREAM="$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null)" \
        || die "$BRANCH has no upstream to push to (git branch --set-upstream-to=<remote>/$BRANCH)"
    REMOTE="${UPSTREAM%%/*}"
fi
if [ "$RELEASE" -eq 1 ]; then
    command -v gh >/dev/null || die "gh is needed for the release (or pass --no-release)"
    [ "$(uname -s)-$(uname -m)" = "Linux-x86_64" ] \
        || die "the release asset is the Linux x86_64 binary; build it there or pass --no-release"
    REPO="$(git remote get-url "$REMOTE" | sed -E 's#^(https://github\.com/|git@github\.com:)##; s#\.git$##')"
    [ -z "$NOTES" ] || [ -f "$NOTES" ] || die "no notes file $NOTES"
fi

CURRENT="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
[ -n "$CURRENT" ] || die "no version in Cargo.toml"
[ "$CURRENT" != "$VERSION" ] || die "the version is $VERSION already"
echo "Bumping $CURRENT → $VERSION"

# --- versions -------------------------------------------------------------------------------------------------
# perl instead of sed -i: the same command on GNU and BSD systems
CUR="$CURRENT" NEW="$VERSION" perl -pi -e \
    'if (!$done && s/^version = "\Q$ENV{CUR}\E"$/version = "$ENV{NEW}"/) { $done = 1 }' Cargo.toml
grep -q "^version = \"$VERSION\"$" Cargo.toml || die "Cargo.toml was not updated"
echo "  ✓ Cargo.toml"
for file in "${MANIFESTS[@]}"; do
    CUR="$CURRENT" NEW="$VERSION" perl -pi -e 's/"version": "\Q$ENV{CUR}\E"/"version": "$ENV{NEW}"/g' "$file"
    grep -q "\"version\": \"$VERSION\"" "$file" || die "$file has no \"version\": \"$CURRENT\""
    echo "  ✓ $file"
done

# --- build and test -------------------------------------------------------------------------------------------
echo "Building release..."
cargo build --release
BUILT="$("$TARGET_DIR/release/ast-index" version 2>&1)"
[ "$BUILT" = "ast-index v$VERSION" ] || die "the built binary says '$BUILT'"
echo "  ✓ $BUILT"
echo "Running tests..."
scripts/test.sh

# --- commit, tag, push ----------------------------------------------------------------------------------------
git add Cargo.toml Cargo.lock "${MANIFESTS[@]}"
git commit -q -m "Bump to $TAG"
git tag -a "$TAG" -m "ast-index $VERSION"
echo "  ✓ commit and tag $TAG"
if [ "$PUSH" -eq 0 ]; then
    echo "Not pushed (--no-push). Later: git push <remote> $BRANCH && git push <remote> $TAG"
    exit 0
fi
retry git push "$REMOTE" "$BRANCH"
retry git push "$REMOTE" "$TAG"
echo "  ✓ pushed $BRANCH and $TAG to $REMOTE"
if [ "$RELEASE" -eq 0 ]; then
    echo "No GitHub release (--no-release)."
    exit 0
fi

# --- GitHub release -------------------------------------------------------------------------------------------
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK:?}"' EXIT
cp "$TARGET_DIR/release/ast-index" "$WORK/ast-index"
chmod 755 "$WORK/ast-index"
# the binary at the archive root: README's `curl … | tar -xz -C ~/.local/bin` relies on it
ASSET="$WORK/ast-index-linux-x86_64.tar.gz"
tar -C "$WORK" -czf "$ASSET" ast-index
GLIBC="$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+$' || true)"

if [ -z "$NOTES" ]; then
    NOTES="$WORK/notes.md"
    PREVIOUS="$(git describe --tags --abbrev=0 "$TAG^" 2>/dev/null || true)"
    {
        echo "Changes since ${PREVIOUS:-the start}:"
        echo
        git log --no-merges --format='- %s' "${PREVIOUS:+$PREVIOUS..}$TAG^"
        echo
        echo "**Linux x86_64, glibc ${GLIBC:-2.39}+:**"
        echo
        echo '```bash'
        echo 'mkdir -p ~/.local/bin'
        echo "curl -fsSL https://github.com/$REPO/releases/latest/download/ast-index-linux-x86_64.tar.gz | tar -xz -C ~/.local/bin"
        echo "ast-index version   # ast-index v$VERSION"
        echo '```'
    } > "$NOTES"
fi

# create once, upload with --clobber: a retry after a dropped connection neither fails nor duplicates
publish() {
    gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1 \
        || gh release create "$TAG" --repo "$REPO" --title "ast-index $VERSION" --notes-file "$NOTES" --verify-tag \
        || return 1
    gh release upload "$TAG" "$ASSET" --repo "$REPO" --clobber
}
retry publish
echo "  ✓ https://github.com/$REPO/releases/tag/$TAG"
echo
echo "Then: stop a running 'ast-index watch' (it keeps the old binary), install the release, and update the plugin:"
echo "  claude plugin marketplace update ast-index-php && claude plugin update ast-index@ast-index-php"
