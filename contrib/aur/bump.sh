#!/usr/bin/env bash
# Point the AUR files at a release: contrib/aur/bump.sh 3.0.1 checksums.txt
#
# Rewrites pkgver, pkgrel and every sha256 in both PKGBUILDs and their
# .SRCINFO. The gnu tarball digests come from the release's checksums.txt;
# the source tarball and LICENSE-MIT are fetched from GitHub and hashed here,
# since the release does not publish them. .SRCINFO is edited in place rather
# than regenerated because `makepkg --printsrcinfo` refuses to run as root,
# which is what a GitHub runner is.
set -euo pipefail

version="${1:?usage: bump.sh VERSION CHECKSUMS_FILE}"
checksums="${2:?usage: bump.sh VERSION CHECKSUMS_FILE}"
version="${version#v}"
repo="https://github.com/teddytennant/wizard"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

digest_of() {
  local asset="$1" sum
  sum="$(awk -v a="$asset" '$2 == a { print $1 }' "$checksums")"
  [ -n "$sum" ] || { echo "bump.sh: $asset is not in $checksums" >&2; exit 1; }
  printf '%s' "$sum"
}

fetch_digest() {
  curl -fsSL "$1" | sha256sum | cut -d' ' -f1
}

x86_64="$(digest_of wizard-x86_64-unknown-linux-gnu.tar.gz)"
aarch64="$(digest_of wizard-aarch64-unknown-linux-gnu.tar.gz)"
license="$(fetch_digest "$repo/raw/v$version/LICENSE-MIT")"
source_tar="$(fetch_digest "$repo/archive/refs/tags/v$version.tar.gz")"

# Set one key in a PKGBUILD (`key=value`) or .SRCINFO (`key = value`) line.
set_key() {
  local file="$1" key="$2" value="$3"
  if [ "${file##*/}" = .SRCINFO ]; then
    sed -i -E "s|^(\t$key = ).*|\1$value|" "$file"
  else
    sed -i -E "s|^($key=).*|\1$value|" "$file"
  fi
  grep -qE "^[[:blank:]]?$key ?= ?" "$file" || { echo "bump.sh: no $key in $file" >&2; exit 1; }
}

bump() {
  local dir="$1" file old
  shift
  for file in "$dir/PKGBUILD" "$dir/.SRCINFO"; do
    old="$(sed -nE 's/^[[:blank:]]?pkgver ?= ?//p' "$file")"
    set_key "$file" pkgver "$version"
    set_key "$file" pkgrel 1
    # .SRCINFO spells the version out in every source line; PKGBUILD uses $pkgver.
    sed -i "s|$old|$version|g" "$file"
    local kv
    for kv in "$@"; do
      set_key "$file" "${kv%%=*}" "${kv#*=}"
    done
  done
}

bump "$here/wizard-bin" \
  "sha256sums=('$license')" \
  "sha256sums_x86_64=('$x86_64')" \
  "sha256sums_aarch64=('$aarch64')"
bump "$here/wizard" \
  "sha256sums=('$source_tar')"

# .SRCINFO carries the digests bare, no quotes or parentheses.
sed -i -E "s/^(\tsha256sums(_[a-z0-9_]+)? = )\('([0-9a-f]+)'\)$/\1\3/" \
  "$here/wizard-bin/.SRCINFO" "$here/wizard/.SRCINFO"

echo "wizard-bin and wizard now point at v$version"
