# Maintain the Maki fork

Local `main` keeps extensions for background agents and unmerged upstream
fixes. Session close cancellation and plugin prompt replacements follow
upstream. Application profiles are preserved on
`archive/application-profiles`, based on upstream `main`. The recovery ref
`backup/main-before-application-split-20261010` keeps the complete former fork.

The application branch belongs to `mmonad/maki`. Upstream PR branches use
`laudney/maki`. Do not replace either fork's remote `main` during this split.

## Documents

| Document | Purpose |
| --- | --- |
| [Upstream maintainer guide](upstream-maintainer-guide.md) | Public review evidence, architecture preferences, and contribution guidance |
| [Background ownership design](background-ownership-design.md) | Ownership, lifecycle, frontend, and delegation contracts |

These documents stay tracked in the local fork. Keep local workflow records
out of upstream PR branches, as described below.

## Feature map

| Group | Owner | Validation boundary |
| --- | --- | --- |
| Background ownership | Agent cancellation, mailbox, Lua sessions, frontend routing | Foreground cancel, teardown, rewind, stale events, child permissions, ACP request ownership |
| Lua delegation | `plugins/task/` | Spawn, collect, cancel, reload cleanup, picker deletion, completed-session release |
| CI coverage | Rust workflow path filter | Changes to integration tests, manifests and benches start Rust CI |
| Local workflow records | `docs/` | Current contracts and valid links. Exclude these records from upstream PRs |

Find the current commits with `git log --oneline upstream/main..main`.
Commit subjects survive rebases. Commit IDs do not.

The 0.6.4 fork starts with the three commits of pending PR #1270. Keep that
first delegation slice before the full native ownership and frontend port.
Both task receipts and subagent handles use one Lua worker and result store.
The PR branch stays limited to its first slice. The renderer PR stays separate.

Upstream owns the weak child cancellation tree and native plugin task tabs.
Agent seeds share their generation token and child cancellation map with
plugin sessions. Plugin transport names its root task separately from nested
child identity. Do not restore the old parallel background billing path.

## Ownership contracts

Upstream `maki.async.spawn` owns Lua work that outlives its caller. The fork
keeps child lifetime separate from foreground runs. Foreground completion or
cancel leaves background children alive. Session close, replacement, rewind,
reload, and shutdown cancel them through their native owner.

A child event must belong to the current session generation. Permission
answers and ACP replies must reach the run or child that asked for them.
Completed results support repeated collection until generation cleanup or
plugin reload. Deleted picker rows discard late child events and closing
transcripts.

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
   Confirm upstream equivalence before dropping a commit. Remove corresponding
   local documentation commits after upstream merges the PR. Keep documentation
   that still describes retained fork changes.
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

Keep the branch rebased. Upstream `main` moves daily and a review can require
a rebase before it continues.

Expect the maintainer to rebase the branch, push fixups, or open a replacement
PR with the contributor as author. Maintainer edits can be denied on an
org-owned fork with a 403, which makes a replacement PR the normal path. This
is not a rejection; continue through the replacement PR or a follow-up.

PR branches publish to `git@github.com:laudney/maki.git`. Inspect the effective
upstream diff, run the checks for that slice, and follow AI-use disclosure
requirements. Do not push or open a PR unless the user requests it.
