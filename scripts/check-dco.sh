#!/usr/bin/env bash
# Fails unless every non-merge commit in BASE..HEAD carries a
# "Signed-off-by:" line matching its author. See CONTRIBUTING.md.
set -euo pipefail

base=${1:?usage: check-dco.sh BASE HEAD}
head=${2:?usage: check-dco.sh BASE HEAD}
missing=0

for sha in $(git rev-list --no-merges "$base..$head"); do
  author="$(git log -1 --format='%an <%ae>' "$sha")"
  if ! git log -1 --format='%(trailers:key=Signed-off-by,valueonly)' "$sha" | grep -qxF "$author"; then
    echo "::error::$(git log -1 --format='%h %s' "$sha") is missing 'Signed-off-by: $author'"
    missing=1
  fi
done

if [ "$missing" -ne 0 ]; then
  echo "Sign off with 'git commit -s'. To fix earlier commits: git rebase --signoff $base"
  exit 1
fi
echo "All commits are signed off."
