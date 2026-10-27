# Upstream maintainer guide

This guide summarizes public GitHub comments by `tontinton`, the owner and
primary maintainer of `tontinton/maki`. Use it to decide which fork changes to
propose upstream, prepare small pull requests, and discuss larger changes.

This is an evidence-based work guide, not a private personality profile.
Statements about communication style describe public project interactions.
Inferences are labeled and should not be treated as fixed personal traits.

## Navigation

- [Summary and upstream requirements](#executive-summary)
- [Architecture and design preferences](#observed-project-values)
- [Review checks](#risk-based-coding-and-review-checks) and
  [evidence by change type](#evidence-expected-by-change-type)
- [PR preparation and review](#pr-acceptance-playbook)
- [Larger proposals](#discuss-a-larger-proposal-in-good-faith) and
  [fork decisions](#fork-decision-guide)
- [Final checklist](#compact-upstream-checklist)

For the current fork, its ownership map, and its port specifications, use
the [fork maintenance guide](fork-maintenance.md).

## Coverage and limits

The review covered all currently accessible maintainer content in these
repository-wide GitHub API surfaces. The corpus was first collected for
activity to 2026-07-28 and was fully re-pulled on 2026-10-16:

| Surface | Reviewed | Date range |
| --- | ---: | --- |
| Issue and PR conversation comments | 513 | 2026-04-03 to 2026-10-13 |
| Inline PR review comments | 889 | 2026-04-03 to 2026-10-13 |
| Submitted PR reviews | 720 across 400 PRs | 2026-04-03 to 2026-10-16 |
| Non-empty submitted review bodies | 172 | Included above |

The 513 conversation comments contain 288 issue comments and 225 PR comments.
The submitted reviews comprise 343 approvals, 234 change requests, 142
comment-only reviews, and one dismissed review. Review state is not always a
complete verdict. An approval can still contain a small required action, and
most detailed review feedback is in inline comments.

The review also inspected 412 maintainer-authored issue and PR opening bodies
for project context. These are supporting evidence, not part of the comment
count. There are no maintainer commit comments in the current public API
results. Maintainer activity in GitHub Discussions was inspected separately
because Discussions are outside the literal issue and PR scope; the 14 public
discussions contain 30 maintainer comments.

GitHub exposes the current body of an edited comment, not its full edit
history. Deleted or private comments are not available through the normal API.
Thus, “all comments” means all records currently accessible from GitHub. The
corpus ends on 2026-10-16 and this guide must be refreshed as upstream activity
continues.

An earlier focused update on 2026-08-19 added the maintainer's reviews of
[PR #734](https://github.com/tontinton/maki/pull/734) and his
[post-merge simplification](https://github.com/tontinton/maki/commit/793b9a16586119fd1d6cb518a9f6bb8eb1bd7b28).
The full refresh includes that content.

The corpus was retrieved from `tontinton/maki` with the paginated REST
endpoints `/issues/comments`, `/pulls/comments`, and `/pulls/{number}/reviews`,
plus a search query for the set of PRs with a submitted review. Records were
matched by the exact author login `tontinton` and counted by unique GitHub
record ID. Review bodies were counted after trimming whitespace.

Unless a statement is directly attributed and cited, priorities and practical
rules below are synthesis from repeated examples. They are guidance, not
quoted maintainer policy or a guarantee of a future decision. Current upstream
repository rules override this synthesis.

## Executive summary

A useful synthesis of recurring evaluation questions is:

1. Does it solve a real and demonstrated user problem?
2. Should the feature exist at all, or does it add a second way to do one
   thing?
3. Does an existing mechanism, config, or plugin API already cover it?
4. Does it belong in Maki, and does the owning layer make sense?
5. Can optional behavior be a Lua plugin or a small plugin API?
6. Is there a simpler design with less state, code, context, or maintenance?
7. Is it correct through failure, cancellation, restore, and compatibility
   paths?
8. Is the benefit supported by the correct evidence?
9. Is the new public surface the smallest one a current consumer needs?
10. Is the pull request small, clean, documented, and easy to review?

The most reliable upstream contribution is a small and independent change that
uses existing abstractions, has one clear owner for state, proves the real
behavior, passes CI, freezes only the public surface it needs, and explains why
it is needed. Large changes can succeed, but usually only when they implement
an agreed architectural direction, remove a class of bugs, and arrive as
staged pull requests.

### Current upstream requirements

Check the current `CONTRIBUTING.md` before each proposal. The current file asks contributors to:

- keep the core minimal, prefer Lua plugins, and discuss missing APIs that
  have no Neovim equivalent before submitting them;
- search open and closed issues before opening an issue;
- describe AI use in a PR and include prompts when unsure.

It also identifies `just ci` as the main local equivalent of blocking CI checks
and offers `🤖🤖🤖` at the end of a PR title as automated-agent opt-in.

The file now also publishes the maintainer's own review prompts. They state the
same preferences this guide records: KISS, DRY, SRP, no unnecessary state and
no state explosion; tests consolidated, with bullshit and flaky tests removed;
trivial comments removed and the remaining comments explaining why on
non-obvious code; prose that is down to earth and warm, tells a story, avoids
em dashes, and does not read as AI-written; commit messages of 2 to 5
sentences; plans and commits judged by whether the code is easy to fix “at 2am
in a year”; bug classes removed by design instead of single bugs; a single
source of truth; and verification with several differently prompted subagents.

The repository `AGENTS.md` mirrors the same code guidelines and names `just
check`, `just lint`, and `just test` as the cheap-first commands. Treat both
files as project requirements, not as background reading.

## Observed project values

### A small, extensible core

A repeated and explicit long-term direction is a lean Rust core with optional
policy and
behavior in Lua. The open plugin roadmap calls for tools, UI features,
keybindings, slash commands, and provider behavior to become controllable with
Neovim-like APIs ([issue #112](https://github.com/tontinton/maki/issues/112)).
The maintainer states that dynamic providers “should absolutely be plugins”
([comment](https://github.com/tontinton/maki/issues/112#issuecomment-4270949803))
and says “if possible always a plugin” for optional behavior
([issue #227](https://github.com/tontinton/maki/issues/227#issuecomment-5100542877)).

This is not a rule that all code must be Lua. Native Rust is appropriate for
mechanisms that need safe ownership, lifecycle, protocol, performance, or a
small capability that plugins cannot provide. In one discussion, short and
clean native code was accepted as a practical quick win even though a plugin
was the preferred final location
([issue #651](https://github.com/tontinton/maki/issues/651#issuecomment-5059773874)).

**Practical rule:** put optional policy and product-specific behavior in a
plugin. Add only the smallest native primitive that the plugin cannot safely
implement.

The direction has hardened. Upstream now says it will not add more builtin
plugins: “I don't plan to add more builtin plugins”
([PR #693](https://github.com/tontinton/maki/pull/693#issuecomment-5151060133))
and “I'm not sure I will add any more builtin plugins to be honest”
([issue #703](https://github.com/tontinton/maki/issues/703#issuecomment-5108568595)).
A monitor plugin was told to ship outside the repository once its core API
landed, and external plugins are announced in the “Plugin showcase” discussion
instead of the README.

Providers followed the same path. Seven builtin providers moved from Rust to
Lua plugins because “every provider change needed a Rust release”
([PR #1104](https://github.com/tontinton/maki/pull/1104)), and he plans to
“move some of the less popular ones we have now out of core”
([PR #1073](https://github.com/tontinton/maki/pull/1073#issuecomment-5845488910)).
Core admission is now explicitly popularity gated: “Every provider in core is
code we have to keep working forever, same reason we don't take most plugins
into core”
([PR #1085](https://github.com/tontinton/maki/pull/1085#issuecomment-5845488697)).
A proposal that adds a builtin provider should expect “is there a reason to get
this provider as a built-in one?”
([PR #1149](https://github.com/tontinton/maki/pull/1149#issuecomment-5973762683)).

### Simplicity means the smallest robust design

The maintainer repeatedly asks for minimal changes, KISS, DRY, SRP, and no
unnecessary state. Examples include requests for “less / more minimal changes”
([PR #12](https://github.com/tontinton/maki/pull/12#issuecomment-4183655712)),
removal of a cache where there is nothing worth caching
([PR #13](https://github.com/tontinton/maki/pull/13#discussion_r3034306109)),
and rejection of optimizations whose extra lines do not pay for themselves
([PR #573](https://github.com/tontinton/maki/pull/573#issuecomment-5012617208)).

Minimal does not always mean a one-line patch. If a patch hides confused
ownership or an invalid model, the maintainer can ask for a structural fix. A
numeric guard was challenged because it appeared to cover “a bigger design
issue” ([PR #119](https://github.com/tontinton/maki/pull/119#issuecomment-4273697110)).
A larger MCP redesign was requested to give concurrent state one serialized
owner and one source of truth
([PR #31](https://github.com/tontinton/maki/pull/31#pullrequestreview-4068170452)).

**Practical rule:** prefer the fewest concepts, owners, and states that solve
the complete problem. Do not optimize only for the smallest line count.

The post-merge cleanup of PR #734 shows that passing lifecycle review is not
enough when the implementation remains larger than the behavior requires. It
removed a shadow flag, overlapping event guards, a one-call helper with a test
for the helper, and a hand-written preview loop while keeping the drain-ordering
invariant. The replacement derived state from the existing enum, routed events
through one match, and used a lazy standard iterator
([commit](https://github.com/tontinton/maki/commit/793b9a16586119fd1d6cb518a9f6bb8eb1bd7b28)).

**Practical rule:** after the behavior is correct, try to delete every state
mirror, single-use abstraction, overlapping condition, and custom loop. Keep
one only when the simpler expression cannot preserve the invariant.

### Public surface is a permanent cost

Functions, fields, ids, slots, flags, and config options that reach plugins or
users become compatibility promises. The maintainer trims a proposed surface to
the part with a concrete consumer today:

- a plugin prompt model offered 9 prompt ids x 8 sections, and the review asked
  for only the used ones: “ship the four, add the rest when someone asks?”
  ([PR #1213](https://github.com/tontinton/maki/pull/1213#discussion_r4263447289));
- exposing `MakiId::generate` was cut because it “makes the internal id encoding
  public plugin API forever”
  ([PR #707](https://github.com/tontinton/maki/pull/707#discussion_r3695456907));
- a live streaming API was questioned because it adds a right-align API “we
  then keep forever”
  ([PR #636](https://github.com/tontinton/maki/pull/636#pullrequestreview-4776476142));
- a new tool parameter was declined because “every new param and description
  line gets sent on every request”
  ([PR #1087](https://github.com/tontinton/maki/pull/1087#pullrequestreview-5325632004));
- an option was declined because “almost nobody's going to know it exists”
  ([PR #1225](https://github.com/tontinton/maki/pull/1225#pullrequestreview-5494038122)).

The same test applies to behavior. A suppression flag becomes a template that
every later surface must copy, which is the stated reason to prefer one general
slot over a per-surface flag: “we'd owe one per surface”
([review](https://github.com/tontinton/maki/pull/1023#pullrequestreview-5226360273)).

**Practical rule:** add public API when a named consumer needs it now. Sketch
future extension points in the proposal, not in the diff. Prefer one general
mechanism over one flag per surface.

### One owner and one source of truth

Reviews often object to parallel stores, copied configuration, stale caches,
and fields that encode state already represented elsewhere. The maintainer
explicitly asks for “one store, not two”
([issue #676](https://github.com/tontinton/maki/issues/676#issuecomment-5080254463)).
He prefers two enums when merging them would make invalid states representable
([PR #465](https://github.com/tontinton/maki/pull/465#discussion_r3557533796))
and typed values instead of stringify-and-parse paths
([PR #465](https://github.com/tontinton/maki/pull/465#discussion_r3557533802)).

Common preferred changes are:

- move an invariant to the data boundary instead of fixing one UI consumer;
- use one resource owner with explicit teardown;
- centralize repeated parsing or provider behavior;
- use `Option` for meaningful absence instead of a valid-looking placeholder;
- remove dead fields, APIs, derives, and speculative extension points;
- avoid caches unless invalidation is complete and the measured value is real.

Two ownership patterns are named directly in reviews. “One choke point”
replaces emission from every transition site with one diff fired when the value
changed, so a new path cannot silently forget the event
([PR #708](https://github.com/tontinton/maki/pull/708#discussion_r3695456956)).
“Every request flows through that one gate” is the reason to fix a shared
entry point instead of the call sites that were noticed
([PR #756](https://github.com/tontinton/maki/pull/756),
[PR #950](https://github.com/tontinton/maki/pull/950)).

### Make lifecycle intent hard to misuse

The review of terminal notifications in PR #734 makes the ownership standard
more concrete. Related per-run fields and their reset rules form one state
machine. Put them behind one owning type with intent methods such as run start,
cancel, completion, and drain. Do not require each event-dispatch arm to know
which subset of fields to reset. This includes state that sits in a nearby type
but participates in the same transition
([review comment](https://github.com/tontinton/maki/pull/734#discussion_r3761410657)).

The same review gives three related implementation and evidence rules:

- Do not add small free functions or generic production APIs only so tests can
  call them with simpler types. Keep the concrete production owner and test
  through it when the behavior is important
  ([review comment](https://github.com/tontinton/maki/pull/734#discussion_r3761410666)).
- Delete tests that only repeat an assignment or conditional. Keep tests that
  drive the real transition and prove reconciliation, ordering, or another
  observable invariant
  ([review comment](https://github.com/tontinton/maki/pull/734#discussion_r3761410666)).
- When two input forms need the same detailed algorithm, such as whitespace
  normalization and character limits, use one helper over the shared input
  concept. Do not keep copies that can count or normalize differently
  ([review comment](https://github.com/tontinton/maki/pull/734#discussion_r3761410673)).

The post-merge cleanup adds two sharper rules. Derive questions such as
"waiting for drain" from the state-machine variant instead of maintaining a
parallel boolean. Route one event through one match instead of several guard
chains that can overlap. Both changes reduced invalid combinations without
changing event order
([commit](https://github.com/tontinton/maki/commit/793b9a16586119fd1d6cb518a9f6bb8eb1bd7b28)).

Platform documentation is also a behavior claim. If a terminal or multiplexer
does not report focus, focus suppression can disable notifications after the
first input. Test the documented workflow on that platform. Then either adapt
the behavior to its capability or document the required setup and limitation
([review comment](https://github.com/tontinton/maki/pull/734#discussion_r3761410680)).

**Practical rule:** optimize for future correctness, not only fewer lines.
Make valid transitions easy, stale state difficult, and tests and documentation
representative of the behavior that users receive.

### Correctness over speculative optimization

Correctness, recoverability, and compatibility usually outweigh small speed
wins. Reviews trace provider wire formats, cancellation, retries, stale events,
restored sessions, old configuration, partial writes, and platform behavior.
Examples include:

- preserving caller-provided protocol identifiers
  ([PR #437](https://github.com/tontinton/maki/pull/437#discussion_r3532003513));
- blocking a silent security downgrade in existing permission configuration
  ([PR #465](https://github.com/tontinton/maki/pull/465#discussion_r3557533794));
- rejecting timer-based persistence of unsafe mid-stream agent state
  ([PR #578](https://github.com/tontinton/maki/pull/578#issuecomment-5025443175));
- requiring crash-safe temporary-file and rename behavior for session data
  ([PR #637](https://github.com/tontinton/maki/pull/637#issuecomment-5070514208));
- rejecting a cache that can return stale Lua tool descriptions for only a
  millisecond-level gain
  ([PR #575](https://github.com/tontinton/maki/pull/575#issuecomment-5012617235)).

Optimizations need realistic measurements and a demonstrated bottleneck. For
compression work, the maintainer requested actual session ratios, competing
algorithm benchmarks, and comparison with a simpler compact-on-close design
([PR #544](https://github.com/tontinton/maki/pull/544#issuecomment-5038072938)).
A real-file algorithm regression was treated as a blocker
([PR #674](https://github.com/tontinton/maki/pull/674#pullrequestreview-4801197399)).

Measurements also decide scope, including against earlier plans. A four-PR
package manager stack was cut back after review because there were “zero
external plugins and zero startup measurements yet” and “Neovim shipped
`vim.pack` without lazy loading too”
([issue #721](https://github.com/tontinton/maki/issues/721#issuecomment-5428728911)).
Before accepting compression work he measured his own store first: “2.7 GB /
3489 files”
([issue #453](https://github.com/tontinton/maki/issues/453#issuecomment-5462573475)).

**Practical rule:** bring the number, or bring the reason the number cannot
matter. A mechanism with no measured problem is deferred, not merged.

### User control and predictable behavior

The maintainer is wary of automation that surprises users, changes data, costs
money, or hides a large behavior change behind configuration. Examples include:

- removing copy transformation because changing clipboard content was
  unexpected ([issue #409](https://github.com/tontinton/maki/issues/409#issuecomment-4845291546));
- preferring no API request until the user actively sends a prompt
  ([issue #474](https://github.com/tontinton/maki/issues/474#issuecomment-4937034656));
- rejecting provider-specific browser behavior for users who do not use that
  provider ([PR #118](https://github.com/tontinton/maki/pull/118#discussion_r3105176760));
- preferring a plugin over a major behavior change hidden behind a setting
  ([issue #409](https://github.com/tontinton/maki/issues/409#issuecomment-4983223128)).

Configuration is welcome when users have a real choice to make and it prevents
a concrete conflict. It is not welcome as a way to place two product models in
one core path.

New code must clear an existing-mechanism check first. On a model-cache
proposal the review was “the TUI already solved this with the on-disk model
cache (`fetch_all_models_cached`), and ACP just never got switched over to it.
Want to make that change instead?”
([PR #1225](https://github.com/tontinton/maki/pull/1225#pullrequestreview-5494038122)).

A feature can also fail by never being found: `edit_lines` “was opt-in, so
nobody used it” and was flipped on by default
([PR #872](https://github.com/tontinton/maki/pull/872)). The alternative is
tested before it is trusted: for notification UX he judged “this should be
default TUI behavior honestly” and asked to implement it and watch whether it
gets annoying
([issue #709](https://github.com/tontinton/maki/issues/709#issuecomment-5193038502)).
Experience with other tools is also direct evidence: an auto-memory feature was
rejected because “I hated this functionality in opencode, it confused the model
and made opening opencode in a rust or java project unbearable”
([issue #1190](https://github.com/tontinton/maki/issues/1190#issuecomment-6066863915)).

### Failure isolation and safe defaults

Recovery paths are reviewed as strictly as the happy path:

- “one bad package shouldn't stop maki from starting”
  ([PR #827](https://github.com/tontinton/maki/pull/827#discussion_r3823953460));
- a pid-file lock design was redirected to `flock` because “the kernel releases
  it when the process dies”, while `Drop` does not run on SIGKILL or Ctrl-C
  ([PR #828](https://github.com/tontinton/maki/pull/828#discussion_r3855734209));
- the model must not be able to escalate billing: `fast` was pulled from the
  tool schema so “the model shouldn't be able to put a session on 2x billing”
  ([PR #983](https://github.com/tontinton/maki/pull/983#issuecomment-5632830721));
- security defaults are closed and opt-in, and a gate must show what it
  actually catches: “what would the trust gate check for?”
  ([issue #789](https://github.com/tontinton/maki/issues/789#issuecomment-5302170596));
- a trust boundary must be explainable in one sentence. Gating
  `.maki/commands` while leaving `.claude/commands` open made the rule
  “halfway”, which was called “the bit that'll rot”
  ([PR #880](https://github.com/tontinton/maki/pull/880#discussion_r3887020921)).

**Practical rule:** state what happens when a component is missing, corrupt,
cancelled, or killed, and keep the blast radius at the failing unit.

### Token and context efficiency

Token cost is a product constraint, not only an implementation detail. The
maintainer objects to repeated or niche global instructions and says “we must
not waste tokens” in always-loaded guidance
([PR #15](https://github.com/tontinton/maki/pull/15#discussion_r3034341852)).
Permissions or tool metadata are questioned when they can add substantial
context ([issue #400](https://github.com/tontinton/maki/issues/400#issuecomment-4835547699)).
At the same time, prompts may become more explicit when trials show that local
models need the detail.

**Practical rule:** measure instruction value against its per-request context
cost. Load specialized guidance only when relevant.

Later examples make the accounting concrete. A new tool parameter is paid on
every request: “this goes out on every request, so worth trimming”
([PR #1198](https://github.com/tontinton/maki/pull/1198#discussion_r4232164211)),
while a number the model can act on (“capped at 2000 lines”) is kept because it
changes paging behavior
([PR #1198](https://github.com/tontinton/maki/pull/1198#discussion_r4232163922)).
A context update that fires for default values was rejected because every user
upgrading a session would pay about a kilobyte plus a notice
([PR #1213](https://github.com/tontinton/maki/pull/1213#discussion_r4263447239)).

Cache discipline is part of the same budget. Tools and the system prompt head
the cached prefix, so rebuilding them rewrites the conversation at cache-write
rates ([PR #1114](https://github.com/tontinton/maki/pull/1114)). That is why
`system` and `tools` are append-only within a request frame.

### Neovim compatibility

Where Maki exposes Lua APIs, the maintainer prefers familiar Neovim semantics
over bespoke equivalents. Command handlers were requested to match
`nvim_create_user_command` with `opts`, raw `args`, split `fargs`, and `nargs`
([PR #643](https://github.com/tontinton/maki/pull/643#issuecomment-5070630450)).
Existing Neovim/plugin ecosystem solutions should be examined before a new
cross-platform API is invented
([issue #602](https://github.com/tontinton/maki/issues/602#issuecomment-5045820024)).

Compatibility is a design benefit, but not a reason to copy an API when its
ownership or lifecycle does not fit Maki.

The direction is broad: “in general we should just copy Neovim, and not have
too many maki changes”
([issue #721](https://github.com/tontinton/maki/issues/721#issuecomment-5192864291))
and “keep it identical to nvim's”
([issue #1190](https://github.com/tontinton/maki/issues/1190#issuecomment-6067127429)).
A copied name is not enough when the semantics differ: the Lua
`maki.pack.update` mirrored `vim.pack.update` but meant “update on every
launch”, so it was dropped and the scripted path moved to the CLI
([PR #829](https://github.com/tontinton/maki/pull/829#discussion_r3880495778)).

### Lean dependency choices

Dependency decisions are contextual:

- compile time, native build dependencies, accuracy, and maintenance burden are
  real costs;
- small and maintained crates are acceptable when they replace risky owned
  protocol or cryptographic code;
- forks are strongly disfavored because they lose upstream security and
  feature updates.

The maintainer first questioned a parser dependency and then approved it after
learning that it was very small
([PR #69](https://github.com/tontinton/maki/pull/69#discussion_r3070067881)).
Conversely, specialized AWS crates were preferred over maintaining hundreds of
lines of protocol and signing code
([PR #133](https://github.com/tontinton/maki/pull/133#issuecomment-4284309277)).
A `crossterm` fork was opposed because of upstream drift
([issue #336](https://github.com/tontinton/maki/issues/336#issuecomment-4758138269)).

Runtime weight is judged the same way. A telemetry exporter was hand rolled
because “the official stack drags in tokio, tonic and prost, a second runtime
inside a TUI tuned for startup time”
([PR #866](https://github.com/tontinton/maki/pull/866)). A runtime migration
was also justified by build time: “I started that way, but compile times went
up so much, that I migrated to smol immediately”
([issue #1100](https://github.com/tontinton/maki/issues/1100#issuecomment-5875269886)).
A dependency that would raise the MSRV must be a deliberate call, not a default
([PR #746](https://github.com/tontinton/maki/pull/746#discussion_r3761419244)).

## Risk-based coding and review checks

Use these checks when they are relevant to the changed risk. Do not create
artifacts for unaffected paths only to satisfy a checklist.

### State and lifecycle

Expect review of:

- owner and teardown of resources;
- cancellation and retry transitions;
- stale events after a replacement operation;
- reset, reload, and plugin unload;
- detached-task accumulation;
- concurrent sessions and subagents;
- partial state persisted across restart;
- lock ownership across crashes, restarts, and two processes;
- UI and executor blocking.

Do not call blocking work from async or UI paths. Reviews have flagged
synchronous HTTP that could block the UI for seconds
([PR #291](https://github.com/tontinton/maki/pull/291#discussion_r3351256242))
and `smol::block_on` inside Smol execution
([PR #328](https://github.com/tontinton/maki/pull/328#discussion_r3469591623)).
Independent calls should run concurrently when ordering does not matter, but
the design must still handle cancellation and deterministic output where
needed.

### Errors and observability

Do not swallow an error and later report a misleading fallback. Preserve the
cause, put the error at the owning boundary, and log independent failures when
the operation can continue. Examples include requests for warnings on failed
provider calls ([PR #372](https://github.com/tontinton/maki/pull/372#discussion_r3485775191))
and an objection to authentication errors becoming “unknown provider”
([PR #235](https://github.com/tontinton/maki/pull/235#discussion_r3408182980)).

A warning is not a substitute for returning an error when correctness or
security requires rejection. Invalid configuration should not panic or silently
become permissive.

Cancellation is a user-visible outcome, not only an error path. When cancelled
and timed-out tools replied with a bare error, the model claimed it never saw
output the user just watched stream by, so those replies now carry the partial
output and state that work was cut off
([PR #794](https://github.com/tontinton/maki/pull/794)).

### Cross-platform claims

Upstream CI does not run tests on Windows, and the maintainer reasons from the
standard library when a change adds platform code. A `ReplaceFileW` wrapper was
cut because `std::fs::rename` already replaces files atomically on Windows,
while the wrapper would add “~45 lines of unsafe FFI plus a new `windows-sys`
dep” in the path every core write uses
([PR #707](https://github.com/tontinton/maki/pull/707#discussion_r3695456903)).
Documented platform behavior is also a claim to verify, such as terminal focus
reporting in multiplexers
([PR #734](https://github.com/tontinton/maki/pull/734#discussion_r3761410680)).

**Practical rule:** do not add an unsafe platform path, dependency, or option
that CI cannot exercise unless the standard library cannot express the
behavior and the failure mode is stated.

### Tests

Tests must prove the changed production behavior. The maintainer rejects:

- tautological tests
  ([PR #11](https://github.com/tontinton/maki/pull/11#discussion_r3032500854));
- tests that only show a closure receives its argument;
- tests that only assert no panic when output can be checked;
- tests of an unchanged helper instead of the changed entry point
  ([PR #524](https://github.com/tontinton/maki/pull/524#discussion_r3605498280));
- tests that hide the actual production failure
  ([PR #177](https://github.com/tontinton/maki/pull/177#discussion_r3173099268));
- tests that depend on real credentials, home directories, cache state,
  network access, timing sleeps, or shared environment variables.

Good tests cover meaningful boundaries: empty input, EOF, multiple valid
records, provider-invalid message order, repeated transitions, failed cleanup,
legacy state, cancellation, and deterministic order after concurrency changes.
When possible, confirm that the regression test fails without the fix.
Build state through production transition methods and assert observable
behavior instead of matching private enum variants. Fold repeated cases into a
`#[test_case]` table, and remove assertions that only restate their inputs
([commit](https://github.com/tontinton/maki/commit/793b9a16586119fd1d6cb518a9f6bb8eb1bd7b28)).

Two concrete test failures are worth copying as habits. A test that runs
through the wrong path is noted plainly: “Your test uses a legacy `.json`,
which fails in a different, stricter place, so the real case is not covered”
([PR #863](https://github.com/tontinton/maki/pull/863#issuecomment-5414776441)).
An assertion that cannot fail on the target behavior is rejected: “This test is
vacuous: `printf %s ""` outputs zero bytes, so `seen` is empty and the assert
passes whether the empty arg is passed through or dropped”
([PR #649](https://github.com/tontinton/maki/pull/649#discussion_r3761419791)).

### Documentation

Update documentation for user-visible behavior and providers. Use the existing
generator when a page is generated rather than editing generated output by
hand ([PR #47](https://github.com/tontinton/maki/pull/47#discussion_r3068251803)).
Documentation should be concise, direct, and free of repeated detail. The
maintainer has explicitly requested less verbose prose and no em dashes
([PR #12](https://github.com/tontinton/maki/pull/12#issuecomment-4183655712)).

The pull request description is reviewed like code. A description that
advertised `maki.pack.update` after it was dropped was noted in review
([PR #829](https://github.com/tontinton/maki/pull/829#discussion_r3887019940)),
and deliberate deviations were asked to be “written down in the description
rather than changed”
([PR #880](https://github.com/tontinton/maki/pull/880#pullrequestreview-5058543383)).

### Comments and commit messages

Code comments should explain a non-obvious reason, constraint, or invariant,
not narrate visible code. Verbose, AI-shaped comments and commits reduce trust
and reviewability. One review called a broad change “too vibe coded” because
of verbose commits, trivial comments, unexpected behavior, and weak
configuration design
([PR #118](https://github.com/tontinton/maki/pull/118#pullrequestreview-4134379588)).
Attach a comment to the item it describes. A correct comment on the wrong
function still misleads readers and was corrected in the PR #734 post-merge
cleanup
([commit](https://github.com/tontinton/maki/commit/793b9a16586119fd1d6cb518a9f6bb8eb1bd7b28)).

Keep commit history clean. Either one commit or a short sequence of real atomic
commits is acceptable. “Fix previous commit” history is noise and harder to
revert ([PR #316](https://github.com/tontinton/maki/pull/316#pullrequestreview-4536983146)).
Squash intermediate repair commits before final review. A review asked to
“consolidate the last 3 commits”
([PR #888](https://github.com/tontinton/maki/pull/888#pullrequestreview-5042293275)),
while a large single commit was rejected as “too much to review properly”
([PR #1083](https://github.com/tontinton/maki/pull/1083#pullrequestreview-5325602339)).

## Evidence expected by change type

| Change | Strong evidence |
| --- | --- |
| Bug fix | Minimal reproduction, root cause, focused regression test |
| Lifecycle/concurrency | Owner, ordering, cancellation, stale-event, and teardown analysis |
| UI | Screenshot or video, manual workflow test, long and boundary states |
| Performance | Baseline, realistic workload, path frequency, before/after result |
| Provider/protocol | Official semantics, request examples, auth and unknown-field cases |
| Prompt/agent behavior | Before/after transcripts or repeated trials across relevant models |
| Storage/migration | Old and new formats, partial-write, restart, recovery, and data-loss tests |
| Cross-platform | Explicit Unix and Windows behavior, fallback or no-op behavior |
| Dependency | Existing alternatives, build cost, maintenance, security, and size |
| New public API or config | One named consumer, smallest frozen surface, removal cost, docs |
| Large refactor | Bug class removed, single owner, migration plan, staged review |

Passing CI is necessary, but it does not replace this evidence.

## PR acceptance playbook

### Before proposing a change

Answer these questions:

1. What exact user workflow fails or becomes costly?
2. Can the problem be reproduced?
3. Should the feature exist at all, or does it add a second way to do one
   thing?
4. Can configuration or an existing mechanism, cached data, or plugin API
   solve it?
5. If not, what is the smallest missing primitive?
6. Which layer owns the behavior and its state?
7. What measured or observed benefit does it provide?
8. What code, context, runtime, dependency, and maintenance costs does it add?
9. Which public surface would be frozen, and who consumes it today?
10. What happens on cancellation, error, reset, restore, and shutdown?
11. Does it work in TUI, headless, SDK, and ACP paths where applicable?
12. Does one bad input stay contained instead of breaking startup?
13. Can the work be split into independently useful pull requests?

If ownership or product fit is uncertain, open an issue before implementation.
The maintainer is more receptive to design discussion before code than to a
large completed implementation that chose the wrong boundary.

The existence question is explicit and early: “steering is a whole new
interaction model and I want to decide if maki should have it at all before
reviewing an implementation”
([PR #593](https://github.com/tontinton/maki/pull/593#issuecomment-5258260997)),
and “why not do this as a Lua plugin? why does this need to exist in core?”
([PR #1180](https://github.com/tontinton/maki/pull/1180#issuecomment-6056487929)).

### Shape the change

- Reuse existing abstractions before adding a parallel path.
- Keep one source of truth.
- Keep provider-specific behavior out of UI and shared models.
- Keep optional policy in Lua.
- Prefer one general mechanism over one flag per surface.
- Expose data, events, and views; let Lua own presentation and policy.
- Avoid new fields, flags, channels, caches, and dependencies unless each has a
  concrete need.
- Preserve raw protocol identifiers and typed values.
- Make invalid states unrepresentable where practical.
- Do not silently change user data, permissions, or billing behavior.
- Include safe fallback and recovery paths for core startup and persistence.

### Prepare the pull request

1. Rebase on current upstream.
2. Inspect the complete diff for accidental reversions and unrelated changes.
3. Split independent features and cleanup.
4. Prefer `just ci`, the main local equivalent of upstream CI at this revision.
5. Run focused reproduction or integration checks for behavior that CI does
   not exercise, and perform manual UI checks when relevant.
6. Recheck error, cancellation, cleanup, restore, and platform paths.
7. Remove trivial comments and speculative APIs.
8. Keep one clean commit or a short clean atomic series.
9. Explain the problem, root cause, design, evidence, risks, and checks.
10. Describe AI use, include prompts when unsure, and use the optional
    automated-agent PR title marker when applicable under current upstream
    contribution instructions.
11. Keep the title and description equal to the final diff; the description is
    reviewed like code
    ([PR #829](https://github.com/tontinton/maki/pull/829#discussion_r3887019940)).
12. Answer “why not a plugin?” in the body when the change adds new core
    behavior or a builtin surface.
13. Expect maintenance by the maintainer. He rebases branches, pushes fixups,
    or picks a PR up himself while preserving contributor authorship
    ([PR #938](https://github.com/tontinton/maki/pull/938#issuecomment-5538017799)).

Small diffs are directly praised as easy to review
([PR #690](https://github.com/tontinton/maki/pull/690#pullrequestreview-4801110115)).
A six-feature bundle was rejected because it could not be reviewed as one unit
([PR #629](https://github.com/tontinton/maki/pull/629#issuecomment-5070513879)).
Do not use a draft PR as a substitute for a ready review request. Open a
regular PR when asking for normal review, or agree on an early design review
first ([PR #430](https://github.com/tontinton/maki/pull/430#issuecomment-4983250244)).

Expect the maintainer to commit fixups, rewrite the branch, or replace the PR.
He has pushed cleanups with a changelog (“pushed a fixup: ...”,
[PR #722](https://github.com/tontinton/maki/pull/722#pullrequestreview-4909227875)),
rebased contributor branches himself
([PR #829](https://github.com/tontinton/maki/pull/829#issuecomment-5445405365)),
and opened a replacement PR when maintainer edits were denied by an org-owned
fork ([PR #646](https://github.com/tontinton/maki/pull/646#issuecomment-5078768579)).
His words on the habit: “no worries about me overwriting PRs, it's just with AI
it's too easy for me to sometimes do it myself and push instead of leaving
comments, gets things in faster, I hope it doesn't discourage you guys”
([PR #938](https://github.com/tontinton/maki/pull/938#issuecomment-5538017799)).

### Respond to review

For each review comment:

1. Identify the underlying concern, not only the requested line edit.
2. Trace adjacent callers and failure paths for the same defect.
3. Make the smallest complete correction.
4. Add or strengthen a test only if it proves behavior.
5. Reply with what changed and what evidence verifies it.
6. If you disagree, restate the concern, provide concrete counterevidence, and
   offer a simpler alternative.
7. Recheck the complete diff after substantial changes.
8. Squash repair commits and run CI before requesting another review.
9. Expect compact verdicts. A changes-requested review may open with
   “direction:” followed by the accepted design, the required edits, and what
   to drop or defer
   ([PR #1011](https://github.com/tontinton/maki/pull/1011#pullrequestreview-5226345917)).
10. Adopt a reversed decision without defending sunk work. The maintainer
    reverses his own scope calls with stated reasons and expects the same
    ([issue #721](https://github.com/tontinton/maki/issues/721#issuecomment-5428728911)).
11. Rebase promptly when asked; upstream moves fast and a review may close with
    “it also needs a rebase”
    ([PR #1211](https://github.com/tontinton/maki/pull/1211#pullrequestreview-5509481955)).
12. Take an offered follow-up, or say when it will land. “Want to take either
    of those? :)” is a real handoff
    ([issue #721](https://github.com/tontinton/maki/issues/721#issuecomment-5428728911)).

The maintainer reopens findings that remain unfixed
([PR #268](https://github.com/tontinton/maki/pull/268#discussion_r3343681119))
but also accepts adequate alternatives when the invariant is met, even if a
more efficient option exists
([PR #178](https://github.com/tontinton/maki/pull/178#discussion_r3176590492)).

## Discuss a larger proposal in good faith

Do not optimize only for acceptance. State material trade-offs, failed trials,
who does not benefit, and when a fork-only solution is better. Accept a no or
defer decision without pressure or repeated reframing.

The public record suggests that agreement works best through shared project
constraints, not rhetoric.

### Lead with the workflow and invariant

State:

1. the user workflow;
2. the existing mechanisms;
3. the exact missing capability;
4. why its owner must be core or plugin;
5. the smallest primitive or model change;
6. failure paths and lifecycle;
7. alternatives and their measured costs;
8. an ordered sequence of independent PRs.

Issue #676 is a strong example. The discussion moved from a user workflow to a
small primitive, then to one resource store, a real message kind for untrusted
output, explicit ownership, and a four-PR split
([first response](https://github.com/tontinton/maki/issues/676#issuecomment-5080254463),
[lifecycle correction](https://github.com/tontinton/maki/issues/676#issuecomment-5109121510)).

### Connect the vision to accepted direction

A proposal is stronger when it:

- advances the Lua/plugin roadmap;
- reduces core policy and maintenance;
- removes duplicated state or a class of failures;
- improves token efficiency without losing semantics;
- preserves Neovim-compatible APIs;
- improves startup, responsiveness, or reliability with measurements.

Do not argue only that another coding agent has the feature. External precedent
can establish user demand, but Maki still needs the correct ownership and cost.

### Quantify both benefit and burden

Include:

- how often the workflow occurs;
- current failure rate or latency;
- before/after behavior;
- lines and concepts added or removed;
- prompt/context cost;
- dependency/build impact;
- new states and cleanup obligations;
- compatibility and migration cost.

The maintainer changes position when evidence answers the cost question. A
small dependency was accepted after its size was checked
([PR #69](https://github.com/tontinton/maki/pull/69#discussion_r3070067881)).

### Offer choices before code

For uncertain architecture, present:

- a fork-only plugin;
- a small upstream primitive plus plugin;
- a deeper core model correction;
- the cost and limits of each.

Ask for the preferred boundary before implementation. This respects the
maintainer’s design ownership and prevents sunk-cost arguments.

### Weak persuasion methods

These are unlikely to be sufficient:

- “Other agents have it.”
- “The implementation is already complete.”
- “It might be faster.”
- “It is generic and future-proof.”
- “Tests pass.”
- “An AI reviewer approved it.”
- A large diff offered as proof of effort.

Each misses at least one of fit, demonstrated value, ownership, maintenance,
or reviewability.

## Communication practices supported by the record

Public interactions support these practical communication choices:

- be informal and concise;
- be direct about blockers;
- give technical detail in proportion to risk;
- treat questions such as “WDYT?” as invitations to compare designs;
- state uncertainty or lack of test coverage;
- correct a conclusion when evidence changes;
- acknowledge useful work and preserve contributor credit.

Examples include “oh you’re totally right, nice” after a correction
([PR #392](https://github.com/tontinton/maki/pull/392#discussion_r3488317451)),
“I don't use mistral myself” when asking a contributor for model confidence
([PR #397](https://github.com/tontinton/maki/pull/397#issuecomment-4827232660)),
and explicit praise for small reviewable diffs
([PR #690](https://github.com/tontinton/maki/pull/690#pullrequestreview-4801110115)).

He also changes his own published decision with its reasons: “Ok went over all
4 again thinking about design this time, and I'm changing my mind on lazy
loading, sorry for the churn”
([issue #721](https://github.com/tontinton/maki/issues/721#issuecomment-5428728911)).
He keeps contributor authorship when he rewrites a branch, names AI-assisted
review honestly, and says when a decision is still open: “Nothing's decided, so
if you have ideas I'd love to hear them”
([PR #765](https://github.com/tontinton/maki/pull/765#issuecomment-6101475664)).

Rejections usually include a reason and often a smaller path forward. “Closing”
commonly means the current shape is not acceptable, not that the user problem
has no value. Useful pieces are often invited back as separate PRs
([PR #578](https://github.com/tontinton/maki/pull/578#issuecomment-5025443175)).

### Working inference

Review interactions appear most effective when a contributor:

- is concise and candid;
- separates fact from uncertainty;
- demonstrates the real behavior;
- accepts simplification without defending sunk work;
- treats maintenance burden as a first-class cost;
- asks for architectural direction early;
- responds to the invariant behind a review comment;
- batches related fixes when practical and states what was rerun.

This is a working interaction model, not a claim about private personality or
an instruction to imitate the maintainer's tone.

## Context-sensitive rules

### Plugin-first is not plugin-only

Optional and maintenance-heavy behavior should normally be a plugin. Small,
broadly useful native primitives and lifecycle-sensitive mechanisms can belong
in Rust.

### Minimal is not always local

A local patch is preferred when it fits the model. A structural fix is preferred
when the patch adds a flag, duplicates ownership, or preserves invalid state.

### Avoid dependencies is not absolute

Reject marginal dependencies. Accept a small, maintained dependency when it
removes substantial risky code or ongoing protocol ownership.

### Avoid duplication is not absolute

Extract stable repeated logic. Limited duplication can be clearer than a
premature shared abstraction, especially for prompt text with different future
semantics.

### High UX standards allow risk-based judgment

Core persistence, security, and lifecycle paths receive strict review.
Optional and isolated integrations can merge with known rough edges when they
are usable and easy to disable
([issue #363](https://github.com/tontinton/maki/issues/363#issuecomment-4792744883)).

### Clean history does not require one commit

One squashed commit or a clean atomic series can both work. The actual rule is
to remove noisy commits that only repair earlier commits in the same PR.

## Warning signs before upstream review

Stop and revise if any of these are true:

- the pull request contains more than one independently useful feature;
- a feature needs several flags or type-tag branches to fit;
- two stores, queues, or maps represent the same resource;
- the change adds a new builtin plugin or provider;
- ids, slots, flags, or config are frozen for consumers that do not exist yet;
- a context update or notice fires for defaults that did not change;
- one bad package, manifest, or config directory can prevent startup;
- a lock cannot be released by a killed process;
- a Windows-only or multiplexer-specific path has no test path in CI;
- provider-specific logic enters UI or a shared model;
- the implementation adds a cache without complete invalidation and a measured
  bottleneck;
- a background task has no explicit owner or teardown;
- a test depends on real credentials, home state, network, timing, or shared
  process environment;
- a performance claim has no baseline;
- the only test asserts that code did not panic;
- the PR includes “while here” cleanup;
- generated docs or dependency metadata are stale;
- commit history is a sequence of fixups;
- the PR body needs a long implementation tour but cannot state the user
  problem and invariant briefly;
- the title, description, or docs advertise behavior the diff does not contain.

## Fork decision guide

Keep a change in the fork when it is:

- a local workflow preference;
- implementable as a plugin without a missing upstream primitive;
- too specialized to justify upstream context or maintenance cost;
- an experimental UX without enough evidence;
- coupled to fork-specific policy or infrastructure.

Propose it upstream when it:

- fixes a reproducible general bug;
- adds a small missing primitive needed by many plugins;
- corrects ownership or removes a class of lifecycle failures;
- advances an explicit roadmap item;
- improves compatibility, safety, or recovery without large policy growth;
- has measured value and a small reviewable implementation.

Discuss before coding when it:

- changes a core data model or owner;
- adds background work or persistent state;
- changes security, permissions, billing, or provider semantics;
- needs a migration;
- spans several crates or frontends;
- introduces a major dependency or plugin API.

If upstream declines, defers, or does not answer after one reasonable
follow-up, keep the change fork-only or plugin-only and document the divergence.
Do not reopen the same proposal without material new evidence.

The upstream direction moves faster than its documentation. Providers, UI
surfaces, and policy have moved to bundled or external Lua plugins
([PR #1104](https://github.com/tontinton/maki/pull/1104)), and new core surface
is popularity gated: “Every provider in core is code we have to keep working
forever, same reason we don't take most plugins into core”
([PR #1085](https://github.com/tontinton/maki/pull/1085#issuecomment-5845488697)).
Check the current release and recent upstream pull requests before proposing a
new builtin, and prefer shaping the change as a plugin or a small plugin API.

A close is often a contract rather than a verdict on the problem. Closes come
with a resubmit spec (“a resubmit with just per-model `thinking_dialect` /
`thinking_fields` / `body_override` would be an easy merge”,
[PR #717](https://github.com/tontinton/maki/pull/717#issuecomment-5258258746)),
a reopen condition (“if you can still reproduce on current main please reopen”,
[PR #981](https://github.com/tontinton/maki/pull/981#issuecomment-5736503422)),
a wait for groundwork
([PR #1073](https://github.com/tontinton/maki/pull/1073#issuecomment-5797563075)),
or an invitation to take a follow-up yourself.

## Compact upstream checklist

Before sending a PR, confirm:

- [ ] The problem is reproduced or supported by concrete evidence.
- [ ] No existing mechanism, config, or plugin API already solves it.
- [ ] The behavior is in the correct owning layer.
- [ ] The design has one source of truth and explicit teardown.
- [ ] The public surface added is the smallest one with a current consumer.
- [ ] The diff contains one coherent change.
- [ ] Simpler alternatives were evaluated.
- [ ] Failure, cancellation, restore, and compatibility paths were checked.
- [ ] One bad input does not break startup or the whole app.
- [ ] Tests prove the changed behavior and fail without the fix when practical.
- [ ] Performance and UI claims have measurements or visual proof.
- [ ] User-facing docs and generated files are current.
- [ ] Upstream CI checks and relevant focused checks pass.
- [ ] The complete rebased diff contains no unrelated changes.
- [ ] The title, description, and docs match the final diff.
- [ ] Commit history is concise and free of fixup noise.
- [ ] The PR body explains need, design, evidence, risks, and checks.
- [ ] Current upstream AI-contribution instructions are followed.

The target contribution can be summarized in one sentence:

> It solves a demonstrated problem, fits the plugin-oriented architecture,
> uses the smallest robust model and public surface, proves behavior in
> proportion to risk, and is easy to review and maintain.
