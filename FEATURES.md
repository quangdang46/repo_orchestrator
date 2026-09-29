# `ro` — feature reference

What each command actually does, and when it is worth reaching for it.

`ro` tracks many GitHub repos from one local SQLite database, keeps working
copies in sync, and automates the boring parts of working across a fleet.

- **Binary**: `ro`
- **State**: `$XDG_STATE_HOME/ro` (override with `--state-dir`)
- **Config**: `$XDG_CONFIG_HOME/ro` (override with `--config-dir`)

---

## The commands you will use most

```bash
ro add owner/repo          # track a repository
ro status                  # what state is each one in
ro sync                    # bring each one up to date
ro doctor                  # is this machine set up correctly
```

> The `ro sweep` namespace was removed in Phase 4. Its commit grouping and
> the agent entry point were rebuilt on top of the engine trait as
> `ro commit`, `ro push` and `ro ship`.

There is no conventional-commit bucketing. The **engine** writes the message:
`claude` and `codex` are asked to read the diff and group it into a few
logically connected commits, and the `git` engine has no basis for a better
subject than `wip on <branch>`. A generated `feat(scope): …` subject is a rule
that produces plausible prose for changes that do not fit it, and a user who
trusted one had no reason to read the diff. `--message <MSG>` overrides it —
one commit per repo, with that subject.

### Flags

These are the flags `ro commit`, `ro push` and `ro ship` share. `ro sync` has
its own; see "Working copies" below.

| Flag | Effect |
|---|---|
| *(none)* | **Act.** Commit, and for `push`/`ship` push. |
| `--dry-run` | Preview without writing. The opt-in, not the default. |
| `--message <MSG>` | One commit per repo, with this subject. |
| `--prompt <TEXT>` | The instruction handed to the agent, replacing the built-in one. |
| `--amend` | *(commit only)* Fold the work into HEAD rather than adding a commit. |
| `--onto <BRANCH>` | Land the work on a branch other than the current one. |
| `--resolve` | Let the engine resolve a merge conflict, then continue the rebase. |
| `--engine <NAME>` | `claude`, `codex` or `git`, for this run only. |
| `--engine-bin <PATH>` | A binary under another name — a nightly, or one outside `PATH`. |
| `--all` | Every tracked repo. This is also the default. |
| `--pattern <GLOB>` | Match `owner/repo` labels, e.g. `quangdang46/*`. |
| `--filter <EXPR>` | `tag:<substr>`. |
| `--tag <T>` | Shorthand for `--filter tag:<T>`. |
| `--include-archived` | Also act on archived and switched-off rows. |
| `--format <FMT>` | `text`, `json`, `ndjson`. |

Repos are named **positionally** — `ro ship cass voice-ai-agent` — so a name
you typed outranks a flag left in a shell profile. `--pattern` is the glob
form of the same selection.

### Safety

- **A bare `ro ship` commits and pushes.** It is not a dry run. `--dry-run`
  is the flag that makes it one, and it prints `would commit` per repo and
  `(dry run — nothing was written)` underneath.
- **Protected branches are refused**, not skipped: `main`, `master`,
  `production`, `staging`, and `release/*`, case-insensitively. The row reads
  `refused: <branch> is a protected branch` and there is no flag to override
  it — `--onto` naming a protected branch is refused too. Make a branch and
  ship that.
- **A conflict is a hand-over, not a failure.** The row reads `needs you:
  …`, the footer counts it as `skipped (need a human)`, and the exit code
  stays 0. `--resolve` is the one opt-in that lets the engine edit files
  mid-rebase.
- **A denylist is applied** before staging: `**/.env`, `**/.env.*`, `*.pem`,
  `*.key`, `**/id_rsa`, `**/id_ed25519`, `**/.git/**`, `**/target/**`,
  `**/node_modules/**`. A denied file is never in the index. The four
  credential patterns carry a `**/` prefix so they match at any depth, not
  just at the repo root.
- **A secret scan runs on the dirty files** and blocks the commit. It is not
  configurable — see "Safety net" below.

---

## Inventory

### `ro init`
Creates the config directory and the SQLite state database. Safe to re-run.

### `ro add <SPEC>`
Track one repo. A spec is `owner/repo`, `github.com/owner/repo`, a clone URL,
or **a path to a checkout you already have** — the two are told apart
structurally, and anything else is refused by name. Records the owner, name,
clone URL and the local path it will live at. `--name` sets the alias,
`--clone-to` and `--branch` shape the clone, `--credential` takes a
*reference* (`env:VAR` or `keychain:ENTRY`) and errors on a pasted token,
`--engine` pins a per-repo engine, `--author` names an `[identity.*]`
profile, `--tag` tags at enrolment.

### `ro remove <KEY>`
Stop tracking a repo. Cascades through dependent rows so the foreign keys stay
consistent. `--delete` also removes the working copy on disk; that is gated
five ways — the path is printed in full, it must be a git checkout whose
`origin` is the URL on the row, a directory that is another registered repo's
working copy is refused outright, the registry is consulted again immediately
before the removal, and an interactive run asks.

### `ro list [--owner <OWNER>] [--tag <T>] [--paths]`
Print the inventory. `--paths` adds each repo's local path, which is the
thing you paste into `cd`. Use `--format json` to read it from a script or an
agent. Output is one JSON object per line under both `json` and `ndjson`,
which makes it streamable and append-friendly.

### `ro tag <REPO> <TAG>…` · `ro untag <REPO> <TAG>…` · `ro tags [<REPO>]`

Group repositories. A tag is a row in `repo_tags`; `--tag <T>` on a fleet
verb selects on it.

```bash
ro tag cass work personal       # both tags
ro untag cass personal          # one comes off
ro tags                         # every tag, with how many repos carry it
ro tags cass                    # one repo's tags
```

Tagging twice is a no-op and removing a tag that was never there is a no-op,
both at exit 0. "Make it so" is idempotent; the row is already in the state
you asked for, and an error would only make shell loops awkward. The count is
printed either way, so a run that changed nothing still says so.

`ro add --tag` sets tags at enrolment. These three verbs are everything
after that — re-tagging a repo, and taking a tag off, which is the half that
is easy to leave out: a table with a writer and no remover grows forever and
the only remedy is editing SQLite by hand.

---

## Working copies

### `ro sync`
Bring every tracked repo in line with its remote.

| Flag | Effect |
|---|---|
| `--strategy ff-only\|rebase\|merge` | How to integrate. Default `ff-only`. |
| `--clone-only` / `--pull-only` | Do one half of the job. |
| `--autostash` | Stash before pulling, pop afterwards. |
| `--dry-run` | Show what would happen. |
| `--timeout <SECONDS>` | Per-operation network timeout. |
| `--prune` | `git remote prune` — drop local remote-tracking refs for branches gone upstream. |
| `--all` | Every tracked repo. This is also the default. |
| `--include-archived` | Also sync archived and switched-off rows. |
| `--filter <EXPR>` / `--tag <T>` | Narrow the same way the fleet verbs do. |
| `--format <FMT>` | `text`, `json`, `ndjson`. |

There is no `--resume`. There is no daemon to resume into: a run is a
process, and a process that was killed is gone. The flag survived a while
because removing it was nobody's urgent work, and it read as a capability
the tool had. A `ro sync` that had been interrupted is re-run, and it is
idempotent by construction — which is a better answer than resuming.

### `ro status [REPO]…`
Per-repo state: current branch, whether the worktree is dirty, and how far
ahead or behind the remote it is. Omit the repos to get every tracked repo.

**`ahead` and `behind` are measured against the local remote-tracking ref, not
the remote.** `ro status` does not fetch — `measured_after_fetch` is `false`
in the JSON — so the number answers "how far is HEAD from `origin/<branch>`
*as this checkout last saw it*". In the documented loop, `ro status` runs
*before* `ro sync`, so that ref is whatever yesterday's fetch left behind, and
`behind=0` there means nobody has looked since. The row names its base rather
than hiding the staleness inside a zero:

| JSON field | What it tells you |
|---|---|
| `measured_against` | The ref the counts were taken against, e.g. `origin/main`. |
| `measured_against_updated_at` | When that ref last moved, Unix seconds. `null` when there is no reflog to read. |
| `measured_after_fetch` | Whether *this* call fetched first. Always `false` today. |
| `unmeasurable_reason` | Why the counts are `null` — e.g. the branch has no upstream ref. |

To refresh the base, run `ro sync`, or fetch in the working copy, then read
`ro status` again. `--filter <EXPR>` (`tag:<substr>`), `--tag <T>`, `--dirty`,
`--ahead` and `--behind` all narrow the same list.

### `ro prune` — removed
There is no `ro prune` verb. It was cut, and nothing replaced it: forgetting a
repo is `ro remove`, and reclaiming its disk is `ro remove --delete`, which
asks first and prints the path it is about to remove. The two kinds of
cleanup that lived here were the same operation spelled twice, and the
destructive one had no gate at all — see "What is deliberately absent"
below. What survived is the narrower `ro sync --prune` flag, which prunes
local bookkeeping and never touches the remote.

---

## Safety net

The two checks below run inside `ro commit`, `ro push` and `ro ship`, before
the engine is handed the worktree.

### Denylist
Paths that are never committed: `**/.env`, `**/.env.*`, `*.pem`, `*.key`,
`**/id_rsa`, `**/id_ed25519`, `**/.git/**`, `**/target/**`,
`**/node_modules/**`. Checked **before** staging, so a denied file is never
even in the index. A repo carrying one is refused with the paths named:
`blocked: denylisted path(s): .env, notes.pem`.

### Secret scan
Scans the dirty files for credential-shaped text. A file containing
something that looks like a GitHub token **blocks** the commit, names the
file and the rule, and redacts the matched value — printing what matched
would repeat the leak in the message reporting it. The rules are
`github_token` (`ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_` + 36 chars),
`github_pat`, `aws_access_key`, `aws_secret_key`, `private_key_block` and
`google_api_key`.

**There is no setting to downgrade it.** The preflight is unconditional and
always blocks. `ro config set checkpoint.secret_scan="warn"` is refused with
`[checkpoint] is no longer read — the preflight is not configurable; it
always blocks`, and the same is true of `safety.secret_scan`. A table ro does
not read is not a setting; it is a promise the tool does not keep. The way
past a finding is to remove the file from the diff, not to lower the gate.

### Quality gates
There are no quality gates. The `ro-sweep` crate still carries a
`quality_gates` module, but nothing in the CLI calls it: there is no flag, no
config key, and no `[review]` table. A commit cannot be blocked by a broken
build, because nothing runs the build.

---

## Conflict recovery

### There is no `ro conflict` verb
A conflict is a **stage inside `ro ship` and `ro push`**, not a namespace.
`ro commit` does not touch the remote and so does not rebase. Both verbs
rebase onto the remote's base before the engine runs, and when that rebase
conflicts the run stops there: the row reads `needs you: …`, the files are
named, and nothing was committed or pushed. It does not resolve the conflict
for you, because the file a conflict lands in is a decision only you can
make.

`ro ship --resolve` / `ro push --resolve` is the one exception and it is
opt-in for the same reason: a model editing a file mid-rebase is the one step
where ro would be rewriting work rather than organising it. The engine edits
the conflicted files and stages them, ro checks the index is free of unmerged
entries and runs `git rebase --continue`, then pushes. An agent that did all
of that on its own would be a tool where every identity guarantee is advice
given to a process free to ignore it.

To bail out by hand, the underlying git commands are the interface:
`git rebase --abort`, `git merge --abort`, `git rebase --continue`.

---

---

## The per-repo summary

`ro ship` prints one line per repository — label, branch, engine, what
happened — and a footer with the counts:

```text
roexp/other                      feat/z                   git        pushed 1c54034

1 committed, 1 pushed, 0 failed.
```

The `outcome` column is the whole story of that repo: `committed <oid>`,
`pushed <oid>`, `nothing to commit`, `would commit` (a dry run),
`skipped: mid-conflict`, `blocked: <reason>`, `refused: <reason>`,
`needs you: <detail>`, or `failed: <error>`. A repo that needs a human is
counted in the footer as `skipped (need a human)` and does not move the exit
code — a signal that fires for a known, already-diagnosed condition is a
signal people learn to ignore.

**There is no account column.** The JSON shape has an `account` field, but
nothing in the binary ever sets it: it is `null` on every row, and the text
renderer never prints it. The line does not tell you which account the
credential resolved to. If you need that, `gh auth status` in the working
copy is the answer.

There is no `ro run list` and no run-history command. The `runs` and
`run_events` tables exist and are written during a sync, but nothing reads
them: with no command to query them, a table nobody queries is not an audit
trail, it is a second copy of the truth that can disagree with the first.

**What this answers:** *what did ro do to this repository?* **What it does
not answer:** *what did ro do last Tuesday?* That gap is real and it is
deliberate — the alternative is a queryable history of an event stream that
nothing is allowed to trust, because a half-populated table answers with
confidence.

The exit code carries the run-level verdict: `0` all succeeded, `1` partial,
`2` all failed, `64` bad usage, `70` fatal. `ro doctor` keeps its own `0`/`1`
— a failed environment check is not a partial fleet run.

---

## Diagnostics and upkeep

### `ro doctor`
Checks the environment: `git` presence and version, GitHub auth, config and
state directory health, the registry, and whether the configured engines are
on `PATH` (`provider:claude`, `provider:codex`). `--fix` applies the repairs
it knows about. A required check failing exits `1`; an optional one warns and
does not move the exit code.

### `ro config`
`ro config` prints every setting; `ro config set KEY=VALUE` changes one.
`ro config print` is the same read, spelled as a subcommand.

**The value is parsed as TOML, not as a string.** `ro config set
agent.engine=codex` fails with `"codex" is not a valid TOML value`, because an
unquoted bare word is not a TOML value. Quote anything that is meant to be a
string:

```bash
ro config set 'agent.engine="codex"'   # a string
ro config set core.parallel=4          # an integer
ro config set 'identity.work.name="Alice"'
```

A key that ro does not read is refused rather than silently written, and the
error names the valid keys — `core.paralel` gets `Did you mean
core.parallel?`. A table from a newer ro is written with a warning, so a
script provisioning a box does not have to know this version's key space.
Per-repo settings go to the registry, not the file: `ro config set
repos.<name>.<key>=<value>` writes the row in `state.db`.

**`[identity]` and `[agent]` are the tables that do something today.**
`[core]`'s `layout`, `parallel`, `projects_dir` and `timeout_secs`, and
`[github]`'s `host` and `auth`, are parsed, validated, printed by
`ro config`, and accepted by `ro config set` — but nothing in the binary reads
them. A clone lands under `$STATE_DIR/projects/<owner>/<name>` no matter what
`core.projects_dir` says, `ro ship` fans out four ways regardless of
`core.parallel`, and the network timeout comes from `ro sync --timeout`, not
from `core.timeout_secs`. Setting them succeeds and changes nothing.

### `ro schema`
Machine-readable CLI reference as JSON, for driving `ro` from an agent instead of
parsing human-readable text. Generated by walking the live clap command tree, so it
cannot drift from the binary: a command added to the CLI appears here
automatically, and one that is removed disappears. Emits the program name and
version, and for each command its `about`, its arguments (`name`, `long`,
`short`, `required`, `help`, possible `values`) and any nested `subcommands`.

This replaces `ro robot-docs <TOPIC>`, which was a hand-written JSON literal. It
had already drifted in four places before being cut: it omitted `commit-sweep`
from the sweep subcommands, omitted `--orphans/--archive/--delete` from the
(since-removed) `prune`, listed only two output formats, and recommended the
`ro health` command that had been renamed to `ro list`. A mirror of a clap
tree that is written by hand is wrong the moment it is written, and nothing in
the build or the test suite can see it.

**Compatibility break.** `ro schema` is a live command-tree dump, which is a
different artifact from the old topic-addressed summary. Anything parsing
removed `ro robot-docs commands` or `ro robot-docs quickstart` needs to move
to `ro schema` and the new shape.

---

## Output formats

`--format` accepts three values on every command that exposes it — `ro list`,
`ro sync`, `ro status`, `ro commit`, `ro push`, `ro ship` and `ro doctor`.

| Format | Use it for |
|---|---|
| `text` | Reading. The default. |
| `json` | Scripting and agents. Stable, schema-backed. |
| `ndjson` | One JSON object per line, for a consumer that reads a stream. |

There is no `toon` renderer, and no other fourth format: `--format toon` is
rejected by clap before ro sees it.

`json` and `ndjson` carry the same objects; the only difference is the
framing. `ro list` emits one object per line under **both** — it streams
rather than emitting a single array, so a consumer can read the first repo
without waiting for the last. The fleet verbs and `ro status` differ:
`ro ship --format json` prints `{"repos": [...], "summary": {...}}`, while
`--format ndjson` prints one row per line and a final line carrying
`"summary": true` — the same counts, framed so twenty repos do not have to
be held in memory at once. `ro doctor` emits one object under both, since a
report is a single document.

---

## Global flags

| Flag | Effect |
|---|---|
| `--config-dir <DIR>` | Override the config location |
| `--state-dir <DIR>` | Override the state location |
| `--non-interactive` | Never prompt. Use this in automation and CI. |

There is no `--quiet` or `--verbose`. `--verbose` was read nowhere, and
`--quiet`'s only real read sat inside the `ro import` branch that Phase 1
deleted. A verbosity flag that changes nothing is a promise the tool does
not keep, and the fix is removal rather than wiring it up later.

There is no `--jobs` / `-j` either. `ro ship` fans out four ways, and the
number is not configurable — `core.parallel` is parsed and validated but read
by nothing.

---

## The engine is in the loop

Worth stating plainly, because the alternative is worse than a surprise.

`ro commit`, `ro push` and `ro ship` hand the worktree to an **agent** before
anything is written. The default is `claude`; `codex` and `git` are the other
two. `git` is the raw backend — it stages everything and writes
`wip on <branch>` — and it is a deliberate choice, not a fallback for when the
model is missing.

```bash
ro commit                          # engine from [agent] engine — claude by default
ro commit --engine codex           # this run only; never written to a row
ro commit --engine-bin ~/bin/claude-nightly
```

`ro add --engine <NAME>` pins a per-repo engine onto the row, and
`ro doctor` reports `provider:claude` / `provider:codex` so a missing binary
is a check you can see rather than a run that fails halfway.

The engine's contract is narrow: read the diff, group the changes into
commits, output the plan. ro stages and commits each group itself, with the
author identity it resolved, and it is the only thing that pushes. A commit
the agent makes itself is one ro cannot attribute and will not push.

---

## What is deliberately absent

So you do not go looking for it:

- **There is no `ro import`.** Bulk-loading a cloud org or a stars list is
  orthogonal to managing a local fleet. Register what you want: `ro add
  owner/repo` to clone, `ro add <path>` to adopt a checkout you already have.
- **There is no `ro prune` verb.** Forgetting a repo is `ro remove`;
  reclaiming its disk is `ro remove --delete`, which prints the path and asks
  first. There is a `ro sync --prune` *flag*, and it is the narrower thing: it
  runs `git remote prune`, dropping local remote-tracking refs for branches
  that no longer exist upstream, and never touches the remote.
- **There is no `ro health` with health scores.** `ro list` is what it was
  renamed to; `ro health` still resolves as a hidden alias for one release. It
  prints the inventory, not a ranking. There are no health scores in the
  registry.
- **There is no `ro review`, no plan/apply/rollback.** The preflight is the two
  checks above, and it is not a separate command.
- **There is no `ro run list`, no run history.** See "The per-repo summary".
- **No `--force` alias for `remove --delete`.** Deleting working copies is
  spelled out in full.
- **No `--resume` on `ro sync`.** There is no daemon to resume into: a run is
  a process, and a process that was killed is gone. A sync is idempotent by
  construction, which is a better answer than resuming.
- **No `rfo` shim.** This project was renamed from `rfo`. If you relied on the
  old binary name, it is gone.
