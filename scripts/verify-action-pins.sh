#!/usr/bin/env bash
# Every remote action under .github/ is pinned to a full commit SHA and the
# exact release that commit is (`owner/repo@<40 hex> # vX.Y.Z`), one commit per
# repository, and every job runs on a runner image named by version
# (`ubuntu-24.04`, never `ubuntu-latest`, which GitHub moves). CI checks that
# form offline.
#
# `--upstream`, run before each release, checks each pin against GitHub and
# names a bump target: the newest release above the pin that is at least
# COOLDOWN_DAYS (default 7) old, since a hijacked release is usually caught
# within days. It edits nothing. It fails when a SHA is not the commit its tag
# names upstream (a wrong comment, or a moved tag) or when the target runs on
# another runtime (runs.using), which is how a deprecation such as Node 20
# shows, or its runtime cannot be read. Needs git, curl, jq and the network; GITHUB_TOKEN lifts the API's
# anonymous limit of 60 requests an hour (a run uses one per repository).
#
# Pins are bumped by hand, not by Dependabot: a bot PR moves a pin without
# verifying the new code, so it is either rubber-stamped, which undoes the pin,
# or reviewed as carefully as a manual bump. Revisit if pins go stale across
# releases, the set of actions grows, or someone owns a weekly review.
# To bump, read the changelog between the two tags for changes to the inputs we
# pass, look over the commits between the pinned SHA and the printed one (the
# age is the release's, and a tag moved after publication keeps it), take the
# SHA printed here, and make one commit per action. A new action
# is a design choice: a run step with a preinstalled tool needs no pin.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pin_re='^[[:space:]-]*uses: ([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)(/[^@[:space:]]+)?@([0-9a-f]{40}) # (v[0-9]+\.[0-9]+\.[0-9]+)$'

errors=0
fail() { echo "$1" >&2; errors=$((errors + 1)); }

pins=""  # one "repo|path|sha|tag" line per occurrence
while IFS=: read -r file line text; do
  [[ "${text}" =~ ^[[:space:]-]*uses:[[:space:]]+\./ ]] && continue  # in-tree action
  if [[ "${text}" =~ ${pin_re} ]]; then
    pins+="${BASH_REMATCH[1]}|${BASH_REMATCH[2]}|${BASH_REMATCH[3]}|${BASH_REMATCH[4]}"$'\n'
  else
    fail "${file#"${root}/"}:${line}: pin as owner/repo@<40-hex SHA> # vX.Y.Z"
  fi
done < <(grep -rnE --include='*.yml' --include='*.yaml' \
  '^[[:space:]]*(-[[:space:]]+)?uses:' "${root}/.github")

while IFS=: read -r file line _; do
  fail "${file#"${root}/"}:${line}: name the runner image by version, not -latest"
done < <(grep -rnE --include='*.yml' --include='*.yaml' \
  '^[[:space:]]*runs-on:.*-latest' "${root}/.github" || true)

pins="$(sort -u <<<"${pins}" | sed '/^$/d')"
for repo in $(cut -d'|' -f1,3 <<<"${pins}" | sort -u | cut -d'|' -f1 | uniq -d); do
  fail "${repo}: pinned at more than one commit; a bump updates every occurrence"
done

if [[ "${1:-}" != --upstream ]]; then
  if ((errors > 0)); then exit 1; fi
  echo "action pins well-formed"
  exit 0
fi

cooldown="${COOLDOWN_DAYS:-7}"
api=(-fsSL -H 'Accept: application/vnd.github+json')
if [[ -n "${GITHUB_TOKEN:-}" ]]; then api+=(-H "Authorization: Bearer ${GITHUB_TOKEN}"); fi

# tag_commit REPO TAG: the commit TAG names upstream, peeled if annotated.
tag_commit() {
  git ls-remote "https://github.com/$1" "refs/tags/$2" "refs/tags/$2^{}" | tail -n1 | cut -f1
}
# runtime REPO PATH REF: the action's runs.using at REF.
runtime() {
  local file yml
  for file in action.yml action.yaml; do
    yml="$(curl -fsSL "https://raw.githubusercontent.com/$1/$3$2/${file}" 2>/dev/null)" || continue
    sed -nE "/^[[:space:]]+using:/{s/^[[:space:]]+using:[[:space:]]*['\"]?([a-z0-9]+).*/\1/p;q;}" <<<"${yml}"
    return
  done
  echo unknown
}

printf '%-30s %-8s %-9s %-8s %5s %-9s %s\n' action pinned runtime target age runtime verdict
while IFS='|' read -r repo path sha tag; do
  verdict=current
  pinned_rt="$(runtime "${repo}" "${path}" "${sha}")"
  if [[ "$(tag_commit "${repo}" "${tag}")" != "${sha}" ]]; then
    verdict="MISMATCH: upstream ${tag} is another commit"
    errors=$((errors + 1))
  fi
  releases="$(curl "${api[@]}" "https://api.github.com/repos/${repo}/releases?per_page=100")" || {
    echo "GitHub API request failed for ${repo}; set GITHUB_TOKEN if rate-limited" >&2
    exit 2
  }
  # The newest vX.Y.Z release above the pin, any major, past the cooldown.
  read -r target age <<<"$(jq -r --arg pinned "${tag}" --argjson cool "${cooldown}" '
    def semver: ltrimstr("v") | split(".") | map(tonumber);
    [.[] | select((.draft or .prerelease) | not)
         | select(.tag_name | test("^v[0-9]+\\.[0-9]+\\.[0-9]+$"))
         | select((.tag_name | semver) > ($pinned | semver))
         | .age = ((now - (.published_at | fromdateiso8601)) / 86400 | floor)
         | select(.age >= $cool)]
    | if length == 0 then "- -"
      else max_by(.tag_name | semver) | "\(.tag_name) \(.age)d" end' <<<"${releases}")"
  target_rt=-
  if [[ "${target}" != - ]]; then
    target_rt="$(runtime "${repo}" "${path}" "${target}")"
    if [[ "${verdict}" == current ]]; then
      verdict="bump: ${repo}${path}@$(tag_commit "${repo}" "${target}") # ${target}"
      if [[ "${target_rt}" == unknown || "${target_rt}" != "${pinned_rt}" ]]; then
        verdict="RUNTIME ${verdict}"
        errors=$((errors + 1))
      fi
    fi
  fi
  printf '%-30s %-8s %-9s %-8s %5s %-9s %s\n' \
    "${repo}${path}" "${tag}" "${pinned_rt}" "${target}" "${age}" "${target_rt}" "${verdict}"
done <<<"${pins}"

if ((errors > 0)); then
  echo "${errors} pin(s) need a decision before tagging (MISMATCH or RUNTIME above)" >&2
  exit 1
fi
