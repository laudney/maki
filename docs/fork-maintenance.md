# Maintain the Maki fork

Local `main` keeps unmerged upstream PRs and fork workflow records.
Session close cancellation, plugin prompt replacements, bounded output,
and background tasks follow upstream. Application profiles are preserved on
`archive/application-profiles`, based on upstream `main`. The recovery ref
`backup/main-before-application-split-20261010` keeps the complete former fork.

The application branch belongs to `mmonad/maki`. Upstream PR branches use
`laudney/maki`. Do not replace either fork's remote `main` during this split.

## Documents

| Document | Purpose |
| --- | --- |
| [Upstream maintainer guide](upstream-maintainer-guide.md) | Public review evidence, architecture preferences, and contribution guidance |

These documents stay tracked in the local fork. Keep local workflow records
out of upstream PR branches, as described below.

## Feature map

| Group | Owner | Validation boundary |
| --- | --- | --- |
| Sandbox guidance (#1299) | `plugins/code_execution/` | Sandbox capabilities, tool description, generated docs |
| Provider test isolation (#1344) | `maki-providers::test_support`, provider and deferral tests | Temporary user directories, endpoint overrides, replay fixtures |
| Background work (upstream) | `CallScope`, session mailbox, Lua tasks, frontend routing | Ctrl+B moves, receipts, stop, teardown, rewind, lost results, headless waits |
| CI coverage | Rust workflow path filter | Changes to integration tests, manifests and benches start Rust CI |
| Local workflow records | `docs/` | Current contracts and valid links. Exclude these records from upstream PRs |

Find the current commits with `git log --oneline upstream/main..main`.
Commit subjects survive rebases. Commit IDs do not.

The fork is based on upstream `85a5ae27` (v0.7.0). Keep its history
linear. The public PR patches come first, then the fork-only work. The
remaining PR patches are, in this order:

| PR | Public commits | Corresponding fork commits |
| --- | --- | --- |
| [#1299](https://github.com/tontinton/maki/pull/1299), sandbox guidance, branch `pr/code-execution-guidance` | `a144a386`, `6d6bdac5` | `d8709fcb`, `cd4a626b` |
| [#1344](https://github.com/tontinton/maki/pull/1344), provider test isolation, branch `pr/test-base-url` | `84adb70b`, `a1227204` | `30fc7fa6`, `b859bf8d` |

PRs #1382 and #1383 merged upstream on 2026-10-30. Their five fork patches
and the bounded job output specifications are removed. The upstream
versions include maintainer changes to queue construction, reader tests,
and background output logs. Keep those versions.

Tony's background task implementation landed in `ff2d8ad9` and `16f73341`,
with later fixes. Closed PR #1346 is superseded. Its seven commits were
already absent from `main`. The `pr/background-key` branch and its worktree
remain as reference, with the other unrelated branches and worktrees.

The v0.7.0 release enables background subagents by default. The fork
follows upstream's background task model. Results are pushed into
the chat. The user stops tasks from `/tasks`, and the model can use
`task_stop`. There are no collection tools. The former fork-only collection
API (`subagent`, `task_result`, `/agents`) is kept only on
`backup/main-pre-subagent-removal-20261024`.

The renderer PR #1272 and skills PR #1330 stay separate from `main`.
Closed PR #1300 and its reverted cache implementation and specifications
are absent from this history. Closed PR #1293 and its native output
specifications are absent too.

The runtime now uses Tokio, HTTP uses reqwest, and MCP uses rmcp. The
provider isolation patches keep upstream's transport fixtures and async
tests. The ACP design and survey documents record the earlier source
baseline. Recheck their runtime, transport, and lifecycle assumptions
before further implementation.

## Ownership contracts

Upstream owns background work. `CallScope` detaches a moved call from its
turn and gives it a task owner. A foreground completion or cancel leaves
moved work alive. `maki.async.run` follows its caller's owner, while
`maki.async.spawn` lives until it returns, is cancelled, or its plugin
unloads.

The session mailbox tracks pending receipts until their results reach the
model. Rewind stops tasks whose receipts leave history. Resume, fork, and
reload report lost results. Preserve these paths and the shared cost tally.

A moved subagent can ask for permission in the TUI. Other moved calls
cannot prompt. Headless subagent prompts are denied. Keep upstream's ask
routing and background wait settings for print, SDK, and ACP modes.

Keep delegation policy in the task plugin. Keep application prompts and
service configuration in downstream repositories. The existing package
manager owns package approvals, lockfiles, and revision leases.

Requests are append-only. A session keeps its system prompt and tools fixed
within a request frame. Use context updates for supported changes and an
announced new frame for a replaced template. Check resume and compaction when
changing prompt construction.

## Refresh upstream

A refresh is repository administration. Preserve unrelated worktrees and
branch refs. Use the existing checkout for a temporary review branch.

1. Read `AGENTS.md`, `CONTRIBUTING.md`, and this guide. Check status, remotes,
   and worktrees. Fetch upstream with `git fetch upstream --prune --tags`.
2. Create a dated recovery ref from `main`. Create a review branch and rebase
   with `rebase.updateRefs=false`. Use `--autosquash` for owned fixups.
3. Inspect the range diff, ancestry, changed files, and skipped patches.
   Confirm upstream equivalence before dropping a commit. When upstream
   merges a PR, drop its fork commits by hand if the maintainer changed the
   patch. Remove its specifications and its row in the PR table. Keep
   documentation that still describes retained fork changes.
4. Check the boundaries upstream changed. Use focused checks first and
   `just ci` for required integration coverage. Reuse passing results only
   while the tested inputs remain unchanged.
5. Present the candidate and remaining limits before replacing `main`.
   Follow the user's authorization. Keep the recovery ref and verify the
   final tip, clean status, and unrelated refs.
6. Remove the completed review branch. Push only when requested. For a
   rewritten remote branch, use an explicit expected SHA with
   `--force-with-lease` and stop if the remote tip differs.

`just ci` checks formatting, all-target Clippy, Python, workspace Nextest,
generated docs, and dependency usage. Select doctests and dependency checks
when the changed content requires them.

The 2026-10-31 refresh removes seven obsolete fork commits and retains
nine commits. The recovery ref is
`backup/main-pre-upstream-rebase-20261031`. The quality pass also preserves
`backup/main-pre-quality-review-20261031`. The v0.7.0 follow-up also keeps
`backup/main-pre-v070-20261031`. Rebase runs with
`rebase.updateRefs=false`, so unrelated branch refs stay fixed.

Validation passed: `just ci` with 9,088 workspace tests and one existing
skip, 11 focused deferral and provider tests, and
`cargo hack check -p maki-providers --each-feature`. `cargo audit` found
no vulnerabilities and six warnings for unmaintained dependencies. All
dependency versions and sources match upstream. The only lockfile
difference is the existing provider test-support self dependency.

The quality pass keeps all test directories, including the derived log
directory, below the temporary home guard. The containment regression fails
with the former helper. A private PID namespace also keeps host command
lines out of the local review runner. Its mock fixture fails before that
change and passes after it. ShellCheck, shell syntax, 19 read-only gh
argument cases, and 171 tests with inherited provider settings passed.

One earlier adversarial run stalled in the synthetic malformed-SSE golden
and was stopped. Its cause was not established. The final full suite and
adversarial run passed, as did ten isolated repeats of that golden.

Run tests with a private `TMPDIR`. Some tests can leave a project in
`/tmp/.maki`, which can affect later temporary directory tests. The provider
isolation patches clear endpoint overrides in the affected tests.

## Prepare an upstream PR

Read [the maintainer guide](upstream-maintainer-guide.md) and current upstream
instructions. Discuss new native APIs when upstream requires it. Answer “why
not a plugin?” before proposing new core behavior: providers, UI surfaces, and
policy are moving to Lua plugins, and new builtins are popularity gated.

Split PR branches into several small logical commits that reviewers can
browse in order. Keep each code commit buildable and include its relevant
tests. Preserve this sequence when addressing review feedback.

Use a separate branch from current upstream and one independently useful
change. Keep design and implementation specifications on the local feature
branch and local `main` until the upstream PR merges. Exclude those records,
this guide, and the maintainer guide from the PR branch.

Keep the title and description equal to the final diff. The maintainer
reviews the description, and behavior it advertises but the diff lacks is a
review finding.

Before opening a PR, and before each push that answers its review, run the
`maki-ci-review` skill in `.claude/skills/`. It runs the upstream review bot's
audit locally and never posts, so its findings are fixed before the bot
raises them.

Keep the branch rebased. Upstream `main` moves daily and a review can require
a rebase before it continues.

Expect the maintainer to rebase the branch, push fixups, or open a replacement
PR with the contributor as author. Maintainer edits can be denied on an
org-owned fork with a 403, which makes a replacement PR the normal path. This
is not a rejection; continue through the replacement PR or a follow-up.

PR branches publish to `git@github.com:laudney/maki.git`. Inspect the effective
upstream diff, run the checks for that slice, and follow AI-use disclosure
requirements. Do not push or open a PR unless the user requests it.
