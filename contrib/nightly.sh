#!/usr/bin/env bash
# Cut the nightly release, if there is anything to release: contrib/nightly.sh
#
# Run by .github/workflows/nightly.yml from a full clone of main, with GH_TOKEN
# set. DRY_RUN=true stops before anything is pushed.
set -euo pipefail

repo="${GITHUB_REPOSITORY:-teddytennant/wizard}"

# The last published release, not the last tag: a tag whose Release run failed
# published nothing, so what it carried is still waiting to go out.
last="$(gh release view --repo "$repo" --json tagName --jq .tagName)"

# Bot commits have no CI run, because a push with GITHUB_TOKEN starts no
# workflows, and on their own they are not worth a release.
newest="$(git log --format='%H %an' "$last..HEAD" | awk '$2 != "github-actions[bot]" && !found { found = $1 } END { print found }')"
if [ -z "$newest" ]; then
  echo "Nothing has landed since $last."
  exit 0
fi

run="$(gh run list --repo "$repo" --workflow ci.yml --branch main --commit "$newest" \
  --limit 1 --json databaseId --jq '.[0].databaseId // empty')"
if [ -z "$run" ]; then
  echo "::error::$newest has no CI run, so it was not released" >&2
  exit 1
fi
gh run watch "$run" --repo "$repo" --interval 30 >/dev/null || true
conclusion="$(gh run view "$run" --repo "$repo" --json conclusion --jq .conclusion)"
case "$conclusion" in
  success) ;;
  cancelled)
    # A newer push cancelled it, and that push is the next candidate.
    echo "CI on $newest was cancelled; leaving it for tomorrow."
    exit 0
    ;;
  *)
    echo "::error::CI on $newest is $conclusion, so it was not released" >&2
    exit 1
    ;;
esac

year="$(date -u +%Y)"
month="$(date -u +%-m)"
n="$(git tag --list "v$year.$month.*" | sed 's/.*\.//' | sort -n | tail -1)"
version="$year.$month.$((${n:-0} + 1))"

sed -i -E "0,/^version = /s/^version = .*/version = \"$version\"/" Cargo.toml gui/Cargo.toml
cargo update --workspace
(cd gui && cargo update --workspace)

# Promote the changelog when there is something to promote. An empty section
# gets no heading, and GitHub's generated notes still list the commits.
first="$(grep -n '^## \[' CHANGELOG.md | sed -n 1p | cut -d: -f1)"
next="$(grep -n '^## \[' CHANGELOG.md | sed -n 2p | cut -d: -f1)"
section="$(sed -n "$((first + 1)),$((next - 1))p" CHANGELOG.md)"
if [ -n "${section//[[:space:]]/}" ]; then
  printf '\n## [%s] - %s\n' "$version" "$(date -u +%F)" | sed -i "${first}r /dev/stdin" CHANGELOG.md
fi

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git add Cargo.toml Cargo.lock gui/Cargo.toml gui/Cargo.lock CHANGELOG.md
git diff --cached --quiet || git commit -q -m "release $version"
git tag "v$version"

if [ "${DRY_RUN:-}" = true ]; then
  echo "Would release v$version at $(git rev-parse HEAD), covering $last..$newest:"
  git show --stat --format=%s HEAD
  exit 0
fi

# Atomic and with no rebase: if main moved, what moved has not passed CI here,
# so this fails and the next night picks it up.
git push --atomic "https://x-access-token:${GH_TOKEN}@github.com/$repo" HEAD:main "refs/tags/v$version"

# A tag pushed with GITHUB_TOKEN does not start release.yml, a dispatch does.
gh workflow run release.yml --repo "$repo" --ref "v$version"

# Then wait on it, so a release that fails fails this run too. This is the run
# GitHub emails about; the dispatched one belongs to the bot.
release=""
for _ in $(seq 30); do
  release="$(gh run list --repo "$repo" --workflow release.yml --event workflow_dispatch --limit 10 \
    --json databaseId,headBranch --jq "[.[] | select(.headBranch == \"v$version\")][0].databaseId // empty")"
  [ -n "$release" ] && break
  sleep 10
done
if [ -z "$release" ]; then
  echo "::error::started the release of v$version but cannot find its run" >&2
  exit 1
fi
echo "Releasing v$version: https://github.com/$repo/actions/runs/$release" | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
gh run watch "$release" --repo "$repo" --interval 60 --exit-status >/dev/null
