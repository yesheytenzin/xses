#!/usr/bin/env sh
set -eu

repo="yesheytenzin/xses"
version="${XSES_VERSION:-latest}"
prefix="${PREFIX:-${HOME}/.local}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os:$arch" in
  Linux:x86_64) platform="linux-x86_64" ;;
  Darwin:x86_64) platform="macos-x86_64" ;;
  Darwin:arm64|Darwin:aarch64) platform="macos-aarch64" ;;
  *)
    echo "Unsupported platform: $os/$arch" >&2
    echo "Download a release manually from https://github.com/$repo/releases" >&2
    exit 1
    ;;
esac

if [ "$version" = "latest" ]; then
  base="https://github.com/$repo/releases/latest/download"
else
  base="https://github.com/$repo/releases/download/v$version"
fi

archive="xses-${version}-${platform}.tar.gz"
if [ "$version" = "latest" ]; then
  archive="xses-latest-${platform}.tar.gz"
  release_api="https://api.github.com/repos/$repo/releases/latest"
  tag="$(curl -fsSL "$release_api" | sed -n 's/.*"tag_name": "\([^"]*\)".*/\1/p' | head -n 1)"
  [ -n "$tag" ] || { echo "Could not determine the latest xses release" >&2; exit 1; }
  archive="xses-${tag#v}-${platform}.tar.gz"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

curl -fsSL "$base/$archive" -o "$tmp/$archive"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"

expected="$(awk -v file="$archive" '$2 == file || $2 == "*" file { print $1; exit }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || { echo "Checksum entry missing for $archive" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$archive" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')"
fi
[ "$expected" = "$actual" ] || { echo "Checksum verification failed" >&2; exit 1; }

mkdir -p "$prefix/bin"
tar -xzf "$tmp/$archive" -C "$tmp"
install -m 0755 "$tmp/xses" "$prefix/bin/xses"
echo "Installed xses to $prefix/bin/xses"
case ":${PATH}:" in
  *:"$prefix/bin":*) ;;
  *) echo "Add $prefix/bin to PATH if it is not already there." ;;
esac
