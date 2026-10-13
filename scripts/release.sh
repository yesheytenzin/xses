#!/usr/bin/env bash
# Cut a release: bump the version everywhere in the project, run the gates,
# commit, tag, and push. Pushing the tag is what builds the release: CI
# (.github/workflows/release.yml) tests, builds the archives for
# linux-x86_64 / macos-x86_64 / macos-aarch64 / windows-x86_64, writes
# SHA256SUMS + build provenance, and publishes the GitHub Release.
#
#   scripts/release.sh 0.5.0              # bump, gates, commit, tag, push tag + main
#   scripts/release.sh 0.5.0 --no-push    # same, but stop before pushing
#   scripts/release.sh 0.5.0 --dry-run    # print the plan, change nothing
#   scripts/release.sh 0.5.0 --force      # push main with --force-with-lease
#                                         # (needed after a history rewrite)
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  sed -n '3,13p' "$0" | sed 's/^# \?//'
  exit 2
}

version=""
no_push=0
dry=0
force=0
for arg in "$@"; do
  case "$arg" in
    --no-push) no_push=1 ;;
    --dry-run) dry=1 ;;
    --force) force=1 ;;
    -h | --help) usage ;;
    v[0-9]* | [0-9]*) version="${arg#v}" ;;
    *)
      echo "unknown argument: $arg" >&2
      usage
      ;;
  esac
done
[ -n "$version" ] || usage
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "version must be X.Y.Z (got '$version')" >&2
  exit 2
}

tag="v$version"
branch="$(git rev-parse --abbrev-ref HEAD)"
current="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

run() {
  if [ "$dry" = 1 ]; then
    echo "+ $*"
  else
    "$@"
  fi
}

echo "release $tag  (project version $current, branch $branch)"

[ "$branch" = main ] || {
  echo "refusing to release from '$branch' (check out main first)" >&2
  exit 1
}
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "tag $tag already exists" >&2
  exit 1
fi
if [ "$dry" = 0 ] && [ -n "$(git status --porcelain)" ]; then
  echo "working tree is dirty; commit or stash first" >&2
  exit 1
fi

# 1. Version placement: manifest, lock, changelog heading.
if [ "$current" != "$version" ]; then
  run sed -i "s/^version = \"$current\"/version = \"$version\"/" Cargo.toml
  run cargo check --quiet # refreshes the xses entry in Cargo.lock
  if grep -q '^## Unreleased' CHANGELOG.md; then
    run sed -i "s/^## Unreleased\(.*\)/## $version\1/" CHANGELOG.md
    echo "  changelog: retitled the Unreleased section to $version"
  else
    echo "  changelog: no '## Unreleased' section — add notes under" \
      "'## $version' before pushing if the release needs them"
  fi
else
  echo "  version already $version; skipping the bump"
fi

# 2. The same gates CI runs on the tag.
run cargo test --all-targets --locked
run cargo clippy --all-targets --locked -- -D warnings
run cargo fmt --check

# 3. Commit what the bump changed, then tag it.
run git add -A
if [ "$dry" = 1 ]; then
  echo "+ git commit -m \"Release $version\"   # if anything changed"
elif [ -n "$(git status --porcelain)" ]; then
  git commit -m "Release $version"
else
  echo "  nothing to commit"
fi
run git tag -a "$tag" -m "xses $version"

# 4. Push: the tag builds the release, main carries the version placement.
if [ "$no_push" = 1 ]; then
  echo
  echo "skipped the pushes (--no-push). To release:"
  echo "  git push ${force:+--force-with-lease }origin main"
  echo "  git push origin $tag"
  exit 0
fi
if [ "$force" = 1 ]; then
  run git push --force-with-lease origin main
else
  run git push origin main
fi
run git push origin "$tag"

echo
echo "pushed $tag — CI is building the release:"
echo "  gh run watch \$(gh run list --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
echo "  gh release view $tag --web"
