#!/usr/bin/env bash
# Copy one GitHub release of MacDock (notes + macdock.exe + macdock.exe.sha256) into a Gitea release on the home
# server, so every version is kept there too. GitHub Actions builds the exe; this only mirrors it, no Rust needed.
#   tools/mirror-release.sh v0.2.0
# Waits up to 20 minutes for the GitHub release to have both files (the Windows build takes ~2.5 min).
# Needs curl, jq, sha256sum and a Gitea token in $GITEA_TOKEN or ~/.gitea_token. Safe to run again.
set -euo pipefail

tag="${1:?usage: mirror-release.sh <tag>}"
gh_repo="GodrezJr2/MacDock"
gitea="${GITEA_URL:-http://127.0.0.1:3030}"
gitea_repo="${GITEA_REPO:-jay/MacDock}"
token="${GITEA_TOKEN:-$(cat ~/.gitea_token)}"
auth=(-H "Authorization: token $token")

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "waiting for GitHub release $tag"
for i in $(seq 1 80); do
  if curl -fsS -H "Accept: application/vnd.github+json" "https://api.github.com/repos/$gh_repo/releases/tags/$tag" -o "$work/gh.json" 2>/dev/null \
     && [ "$(jq '[.assets[].name] | (index("macdock.exe") != null) and (index("macdock.exe.sha256") != null)' "$work/gh.json")" = true ]; then
    break
  fi
  [ "$i" = 80 ] && { echo "GitHub release $tag not complete after 20 min"; exit 1; }
  sleep 15
done

for f in macdock.exe macdock.exe.sha256; do
  url="$(jq -r --arg n "$f" '.assets[] | select(.name == $n) | .browser_download_url' "$work/gh.json")"
  curl -fsSL "$url" -o "$work/$f"
done
(cd "$work" && sha256sum -c macdock.exe.sha256)

name="$(jq -r '.name' "$work/gh.json")"
jq -n --arg t "$tag" --arg n "$name" --slurpfile g "$work/gh.json" \
  '{tag_name: $t, name: $n, body: ($g[0].body + "\n\n_Mirrored from " + $g[0].html_url + "_"), draft: false, prerelease: $g[0].prerelease}' > "$work/rel.json"

# create the Gitea release, or update the one a previous run made
if curl -fsS "${auth[@]}" "$gitea/api/v1/repos/$gitea_repo/releases/tags/$tag" -o "$work/gt.json" 2>/dev/null; then
  id="$(jq -r .id "$work/gt.json")"
  curl -fsS -X PATCH "${auth[@]}" -H "Content-Type: application/json" -d @"$work/rel.json" "$gitea/api/v1/repos/$gitea_repo/releases/$id" -o "$work/gt.json"
else
  curl -fsS -X POST "${auth[@]}" -H "Content-Type: application/json" -d @"$work/rel.json" "$gitea/api/v1/repos/$gitea_repo/releases" -o "$work/gt.json"
fi
id="$(jq -r .id "$work/gt.json")"

# replace the files
for aid in $(jq -r '.assets[]?.id' "$work/gt.json"); do
  curl -fsS -X DELETE "${auth[@]}" "$gitea/api/v1/repos/$gitea_repo/releases/$id/assets/$aid" >/dev/null
done
for f in macdock.exe macdock.exe.sha256; do
  curl -fsS -X POST "${auth[@]}" -F "attachment=@$work/$f" "$gitea/api/v1/repos/$gitea_repo/releases/$id/assets?name=$f" >/dev/null
done
echo "mirrored $tag to $gitea/$gitea_repo/releases/tag/$tag"
