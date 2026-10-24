---
name: maki-ci-review
description: Run the official maki CI review bot's audit locally on a branch or open PR before pushing or opening an upstream PR to tontinton/maki, so its findings get fixed first. Read-only; reports findings in the bot's format and never posts to GitHub.
---

# Local maki CI review

This is the audit that `maki-review[bot]` runs on tontinton/maki PRs, run
locally before a PR is opened or updated. Run it on every upstream PR branch
before `gh pr create`, and again before each push that answers review
feedback. Fix every CRITICAL/HIGH/MEDIUM it reports, or record why a finding
is accepted, before pushing.

## How it runs

The bot is upstream's `.github/workflows/claude-review.yml`: `claude -p`
with the review prompt, `claude-opus-5-5` at effort `high`, in a checkout
of the PR and nothing else. `review.sh` does the same:

- `prompt.md` is the prompt, sent as is with only `$BASE`, `$HEAD`, `$PR`,
  `$REPO` and `$REVIEWER` filled in.
- The input is the PR's commit range, resolved to SHAs. Nothing is fetched.
- The checkout is a fresh repository that holds only the head's history,
  so no other branch, local spec or uncommitted file is there.
- The environment is empty but for what the run needs, so no
  `ANTHROPIC_BASE_URL` or session variable reaches it, and `gh` is a shim
  that only reads.
- An empty `CLAUDE_CONFIG_DIR` keeps out user settings, CLAUDE.md, memory,
  plugins and hooks. `--strict-mcp-config` keeps out MCP servers. The tools
  are the bot's read-only allow list.
- `claude` runs under `bwrap` with an empty, read-only `$HOME`, a private
  `/tmp`, and a private PID namespace that hides host command lines. Its
  config and scratch dirs are writable. The checkout and gh shim are
  read-only, so the review cannot plant a git config or replace the shim.

Never run the audit as an Agent-tool subagent, and never add anything to
the prompt: no change summary, rationale, spec, earlier findings or hints.
The bot sees only the PR, so extra context hides what it will catch.

## Run it

One-time setup, done by the user in their own terminal:

- Install `bwrap` and `socat` (`sudo pacman -S bubblewrap socat`); CI
  installs both for Claude Code's Bash sandbox.
- Run `claude setup-token` and save the token in
  `~/.config/maki-review/oauth-token` (mode 600). Never print or read the
  token into the conversation.

```
.claude/skills/maki-ci-review/review.sh <base>..<head> [pr-number]
```

`<base>..<head>` is exactly the PR's commits, e.g. `b816264a..pr/test-base-url`.
Run it in the background; it takes
minutes. Its stdout is the review, and it ends by naming the report file.
Relay the review verbatim, then act on it.

## Acting on the report

- Fix real CRITICAL/HIGH/MEDIUM findings before pushing, then run the
  project checks again (the audit itself never runs them).
- A finding that is a known, accepted limit stays only if the PR description
  names it.
- A maintainer decision goes into the PR description or a PR comment as a
  question, with the recommended option.
- Re-run the skill after the fixes when they were substantial.
