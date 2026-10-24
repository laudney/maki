#!/usr/bin/env bash
# Runs the upstream review bot's audit locally, the way
# tontinton/maki's .github/workflows/claude-review.yml runs it: `claude -p`
# with prompt.md in a repository holding only the PR's history, with no user
# config, memory, plugins, hooks, MCP servers or home directory.
#
# Usage: review.sh <base>..<head> [pr-number]
# Needs a token from `claude setup-token` in ~/.config/maki-review/oauth-token,
# and bwrap and socat, which Claude Code's Bash sandbox uses, as in CI.
set -euo pipefail

RANGE=${1:?usage: review.sh <base>..<head> [pr-number]}
PR=${2:-none}
REPO=tontinton/maki
REVIEWER='maki-review[bot]'
MODEL=claude-opus-5-5
EFFORT=high
TOKEN_FILE=${MAKI_REVIEW_TOKEN_FILE:-$HOME/.config/maki-review/oauth-token}

skill_dir=$(cd "$(dirname "$0")" && pwd)
repo_dir=$(git -C "$skill_dir" rev-parse --show-toplevel)
for tool in bwrap socat jq gh git claude; do
    command -v "$tool" >/dev/null || {
        echo "missing $tool: install it first" >&2
        exit 1
    }
done
[[ -r $TOKEN_FILE ]] || {
    echo "missing $TOKEN_FILE: run \`claude setup-token\` and save the token there" >&2
    exit 1
}

[[ $RANGE == ?*..?* && $RANGE != *...* ]] || {
    echo "the range must be <base>..<head>" >&2
    exit 1
}
BASE=$(git -C "$repo_dir" rev-parse --verify -q "${RANGE%%..*}^{commit}")
HEAD=$(git -C "$repo_dir" rev-parse --verify -q "${RANGE##*..}^{commit}")
git -C "$repo_dir" merge-base --is-ancestor "$BASE" "$HEAD" || {
    echo "$BASE is not an ancestor of $HEAD: rebase the PR or pass its real base" >&2
    exit 1
}

run_dir=$(mktemp -d "${TMPDIR:-/tmp}/maki-review.XXXXXX")
checkout=$run_dir/repo
trap 'rm -rf "${run_dir:?}/config" "${run_dir:?}/scratch" "${run_dir:?}/bin" "$checkout"' EXIT
mkdir -p "$run_dir/config" "$run_dir/scratch"
# Its own repository holding only the history of the head, so no other
# branch, local spec or uncommitted file can be read.
git init -q "$checkout"
git -C "$checkout" fetch -q --no-tags \
    --upload-pack="git -c uploadpack.allowAnySHA1InWant=true upload-pack" \
    "$repo_dir" "$HEAD"
git -C "$checkout" checkout -q --detach "$HEAD"
mkdir -p "$run_dir/bin"
cp "$skill_dir/gh-readonly.sh" "$run_dir/bin/gh"
ln -s "$(readlink -f "$(command -v git)")" "$run_dir/bin/git"
claude_bin=$(readlink -f "$(command -v claude)")
gh_bin=$(readlink -f "$(command -v gh)")

prompt=$(<"$skill_dir/prompt.md")
prompt=${prompt//\$HEAD/$HEAD}
prompt=${prompt//\$BASE/$BASE}
prompt=${prompt//\$PR/$PR}
prompt=${prompt//\$REPO/$REPO}
prompt=${prompt//\$REVIEWER/$REVIEWER}

scratch=$run_dir/scratch
# An empty environment, so no ANTHROPIC_BASE_URL or session variable of the
# caller reroutes the token or reaches the review. An empty $HOME and /tmp,
# so the PR cannot steer a read of local secrets, CLAUDE.md files or notes.
# A private PID namespace hides host command lines. The checkout and gh
# shim stay read-only; config, scratch and the private /tmp are writable.
env -i \
    HOME="$HOME" USER="$USER" LANG="${LANG:-C.UTF-8}" TERM=dumb \
    PATH="$run_dir/bin:/usr/bin:/bin" \
    REAL_GH="$gh_bin" \
    GH_TOKEN="$(gh auth token)" \
    CLAUDE_CONFIG_DIR="$run_dir/config" \
    CLAUDE_CODE_OAUTH_TOKEN="$(<"$TOKEN_FILE")" \
    CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1 \
    XDG_CACHE_HOME="$run_dir/config/cache" \
    XDG_STATE_HOME="$run_dir/config/state" \
    XDG_DATA_HOME="$run_dir/config/data" \
    GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
    DISABLE_AUTOUPDATER=1 \
    bwrap --ro-bind / / --tmpfs "$HOME" --tmpfs /tmp \
    --ro-bind "$run_dir" "$run_dir" --bind "$run_dir/config" "$run_dir/config" \
    --bind "$scratch" "$scratch" --ro-bind "$claude_bin" "$claude_bin" \
    --ro-bind "$gh_bin" "$gh_bin" --remount-ro "$HOME" \
    --dev /dev --proc /proc --unshare-pid --die-with-parent --chdir "$checkout" \
    "$claude_bin" -p "$prompt" \
    --model "$MODEL" \
    --effort "$EFFORT" \
    --setting-sources user \
    --strict-mcp-config \
    --no-session-persistence \
    --add-dir "$scratch" \
    --allowedTools "Read(./**),Grep(./**),Glob(./**),Read(/$scratch/**),Edit(/$scratch/**),Write(/$scratch/**),WebSearch,WebFetch,Bash(git log:*),Bash(git show:*),Bash(git diff:*),Bash(git blame:*),Bash(gh pr view:*),Bash(gh pr diff:*),Bash(gh api:*)" \
    --output-format stream-json --verbose \
    | tee "$run_dir/stream.jsonl" \
    | jq -j --unbuffered '
        if .type == "assistant" then .message.content[]
          | if .type == "tool_use" then "> \(.name) \(.input | tostring | .[0:300])\n"
            else empty end
        elif .type == "result" then
          "\n== \(.subtype) turns=\(.num_turns) cost=$\(.total_cost_usd)\n",
          (.permission_denials // [] | .[] | "denied: \(.tool_name) \(.tool_input | tostring | .[0:300])\n")
        else empty end' >&2 || status=$?

jq -r 'select(.type == "result") | .result // empty' "$run_dir/stream.jsonl" \
    | tee "$run_dir/report.md"
echo "report: $run_dir/report.md" >&2
exit "${status:-0}"
