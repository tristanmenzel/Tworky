#!/usr/bin/env bash
# Prints the release notes for the release <tag> of <sha>: one bullet per `Change:` trailer in the
# commits since the newest other published release, then `commit: <sha>`, which update.rs reads.
#
#   scripts/release-notes.sh <sha> <tag>
#
# Needs full history and tags, and a gh that can read the repo's releases (GH_REPO picks the repo).
set -euo pipefail

sha=$(git rev-parse --verify "${1:?usage: release-notes.sh <sha> <tag>}^{commit}")
tag=${2:?usage: release-notes.sh <sha> <tag>}

prev_tag=$(gh release list --exclude-drafts --limit 100 --json tagName,createdAt \
  --jq "[.[] | select((.tagName | startswith(\"v\") or startswith(\"build-\")) and .tagName != \"$tag\")]
        | sort_by(.createdAt) | last | .tagName // empty")

prev=
if [ -n "$prev_tag" ]; then
  prev=$(git rev-parse -q --verify "refs/tags/$prev_tag^{commit}" || true)
  [ -n "$prev" ] || echo "release-notes: tag $prev_tag isn't in this clone" >&2
fi

changes=
if [ -n "$prev" ] && git merge-base --is-ancestor "$prev" "$sha"; then
  changes=$(git log --format=%B "$prev..$sha" | sed -n 's/^Change: \(.*\)$/- \1/p')
fi

if [ -n "$changes" ]; then
  printf '%s\n' "$changes"
else
  echo "No user-facing changes."
fi
echo
echo "commit: $sha"
