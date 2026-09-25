# `ro` — feature reference

What each command actually does, and when it is worth reaching for it.

`ro` tracks many GitHub repos from one local SQLite database, keeps working
copies in sync, and automates the boring parts of working across a fleet.

- **Binary**: `ro`
- **State**: `$XDG_STATE_HOME/ro` (override with `--state-dir`)
- **Config**: `$XDG_CONFIG_HOME/ro` (override with `--config-dir`)

---

## The one command you will use most

```bash
ro sweep commit-sweep --all              # dry run: show the plan
ro sweep commit-sweep --all --execute    # create the commits
ro sweep commit-sweep --all --execute --push
```

Groups every dirty file across every tracked repo into **logical buckets** and
makes one conventional commit per bucket, instead of one catch-all commit per
repo:

| Bucket | Prefix | Example message |
|---|---|---|
| source, new file | `feat` | `feat(src): update src source` |
| source, modified | `fix` | `fix(core): update core source` |
| source, renamed | `refactor` | `refactor(lib): update lib source` |
| test files | `test` | `test(core): update core test` |
| docs | `docs` | `docs(root): update root doc` |
| config / CI | `chore` | `chore(ci): update ci config` |

The scope comes from the top-level directory, and a ticket id on the branch is
appended — `feature/bd-123` produces `feat(src): update src source (bd-123)`.

### Flags

| Flag | Effect |
|---|---|
| *(none)* | Dry run. Nothing is written. This is the default. |
| `--execute` | Create the commits. |
| `--push` | Push each branch after a successful commit. |
| `--push-remote <NAME>` | Remote to push to. Default `origin`. |
| `--force-with-lease` | Safer force-push. |
| `--all` | Every tracked repo. |
| `--repos <GLOB>` | Match `owner/repo` labels, e.g. `quangdang46/*`. |
| `--filter <EXPR>` | `tag:<substr>` or `health:<N>`. |
| `--path <DIR>` | One working copy, bypassing the inventory. |
| `--allow-protected` | Also commit on `main`, `master`, `release/*`. |
| `--respect-staging` | Keep manually staged files as a separate `wip:` commit. |

### Safety

- **Dry run is the default.** You must pass `--execute` to change anything.
- **Protected branches are skipped** by default: `main`, `master`,
  `production`, `staging`, `release/*`. `--allow-protected` overrides this.
- **Push is gated three ways** — the `--push` flag must be set, the branch must
  not be protected, and the remote must exist. Any gate failing means no push.
- **Nested repositories and worktrees are skipped**, with a warning, rather
  than committed as a single opaque gitlink. `git status -uall` still reports a
  nested repo as one `dir/` entry, so this is checked explicitly.
- **A denylist is applied** before staging: `.env`, `.env.*`, `*.pem`, `*.key`,
  `id_rsa`, `id_ed25519`, `**/.git/**`, `**/target/**`, `**/node_modules/**`.

---

## Inventory

### `ro init`
Creates the config directory and the SQLite state database. Safe to re-run.

### `ro add <SPEC>`
Track one repo. Accepts `owner/repo`, `github.com/owner/repo`, or a clone URL.
Records the owner, name, clone URL and the local path it will live at.

### `ro remove <KEY>`
Stop tracking a repo. Cascades through dependent rows so the foreign keys stay
consistent.

### `ro list [--owner <OWNER>]`
Print the inventory. Use `--format json` or `--format toon` to read it from a
script or an agent. Output is one JSON object per line, which makes it
streamable and append-friendly.

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
| `--resume` | Continue an interrupted run. |
| `--timeout <SECONDS>` | Per-operation network timeout. |
| `--format <FMT>` | `text`, `json`, `toon`. |

### `ro status [REPO]`
Per-repo state: current branch, whether the worktree is dirty, and how far
ahead or behind the remote it is. Omit the repo to get every tracked repo.

### `ro health [REPO]`
A 0–100 score per repo with a class of `excellent`, `healthy`, `attention`,
`risky` or `critical`. Use `--filter health:<N>` elsewhere to find the bad ones
in bulk — for example feeding `ro sweep commit-sweep --filter`.

### `ro prune`
Two independent kinds of cleanup.

**Database-level** — forgets repos from the inventory:

| Flag | Removes |
|---|---|
| `--archived` | Repos marked archived |
| `--missing` | Repos whose local path no longer exists on disk |

**Disk-level** — finds working copies that are in the projects directory but
*not* in the inventory:

| Flag | Effect |
|---|---|
| `--orphans` | Report only. |
| `--archive` | Move to `<state-dir>/archived/<name>_<timestamp>` |
| `--delete` | Delete permanently. |

`--delete` refuses to run without an interactive terminal unless
`--non-interactive` is passed, and `--archive` and `--delete` are mutually
exclusive. The scan descends at most 4 levels and never enters `.git`,
`node_modules` or `target`; a working copy is treated as a leaf, so its own
subdirectories are not mistaken for separate repos.

---

## Sweep

### `ro sweep commit-sweep`
See the top of this document — the multi-repo conventional-commit sweep.

### `ro sweep commit --path <DIR> --message <MSG>`
Single repo, single commit. Runs denylist, then quality gates, then a secret
scan on the dirty files, and only stages if all three pass. Useful when you want
to control the message yourself.

### `ro sweep agent`
Runs quality gates, a secret scan and a denylist check across one or many repos
and reports whether each is sweepable. It creates **no commits** — it is a
readiness check.

| Flag | Effect |
|---|---|
| `--all` | Every tracked repo |
| `--repos <GLOB>` | Match `owner/repo` labels |
| `--filter <EXPR>` | `tag:<substr>`, `health:<N>` |
| `--dry-run` | Preview only |
| `--auto-approve <LEVEL>` | `none` (default) or `low` |
| `--output json` | Stream NDJSON events |

---

## Conflict recovery

### `ro conflict list` / `explain <ID>` / `abort <ID>` / `mark-resolved <ID>`
Surfaces repos with an in-progress merge or rebase, explains what state each
one is in, and lets you bail out or record that you have handled it by hand.

---

## Review

Plan-then-apply automation with a rollback path.

| Command | Effect |
|---|---|
| `ro review plan` | Build a review plan. Creates nothing. |
| `ro review approve` | Mark a plan as approved |
| `ro review reject` | Discard a plan |
| `ro review apply` | Execute an approved plan |
| `ro review rollback` | Undo a previously applied plan |
| `ro review list-plans` | Show known plans |

---

## Run history

Every mutating operation records a run, so you can answer "what did we change
last Tuesday?".

| Command | Effect |
|---|---|
| `ro run list` | Recent runs |
| `ro run show <ID>` | One run in detail |
| `ro run timeline` | Its events in order |

---

## Diagnostics and upkeep

### `ro doctor`
Checks the environment: `git` presence and version, GitHub auth, config and
state directory health. `--fix` applies the repairs it knows about.

### `ro config`
`ro config` prints every setting; `ro config set KEY=VALUE` changes one.

### `ro schema`
Machine-readable CLI reference as JSON, for driving `ro` from an agent instead of
parsing human-readable text. Generated by walking the live clap command tree, so it
cannot drift from the binary: a command added to the CLI appears here
automatically, and one that is removed disappears. Emits the program name and
version, and for each command its `about`, its arguments (`name`, `long`,
`short`, `required`, `help`, possible `values`) and any nested `subcommands`.

This replaces `ro robot-docs <TOPIC>`, which was a hand-written JSON literal. It
had already drifted in four places before being cut: it omitted `commit-sweep`
from the sweep subcommands, omitted `--orphans/--archive/--delete` from `prune`,
listed only two output formats, and recommended `ro health` in its quickstart. A
mirror of a clap tree that is written by hand is wrong the moment it is written,
and nothing in the build or the test suite can see it.

**Compatibility break.** `ro schema` is a live command-tree dump, which is a
different artifact from the old topic-addressed summary. Anything parsing
`ro robot-docs commands` or `ro robot-docs quickstart` needs to move to
`ro schema` and the new shape.

---

## Output formats

`--format` accepts three values across the commands that expose it.

| Format | Use it for |
|---|---|
| `text` | Reading. The default. |
| `json` | Scripting and agents. Stable, schema-backed. |
| `toon` | Token efficiency. Compact tabular output. |

`toon` renders a uniform array of objects as a tab-separated table — header row,
then one row per element — which is markedly cheaper to feed a model than
repeating the key names on every record. Anything without a single uniform key
set falls back to the text renderer. Note that `ro list` deliberately streams one
object per line rather than a single array, so it does not take the tabular
path; that is the trade for keeping the output append-friendly.

`ro sync --output json` on `sweep agent` emits NDJSON instead, for streaming
progress across many repos.

---

## Global flags

| Flag | Effect |
|---|---|
| `--config-dir <DIR>` | Override the config location |
| `--state-dir <DIR>` | Override the state location |
| `--non-interactive` | Never prompt. Use this in automation and CI. |
| `--quiet` / `--verbose` | Less or more output |

---

## Quality gates

`ro sweep commit` and `ro sweep agent` run the ecosystem's own checks before
touching anything, so a commit cannot land on top of a broken build. Detected
automatically from the repo: **Rust**, **Node**, **Python**, **Go**.

---

## What is deliberately absent

So you do not go looking for it:

- **No AI or LLM integration.** The NTM wrapper and its `ai-sync` consumer were
  removed. Commit messages here are rule-based and deterministic, not generated.
- **No `inbox` command.** It never existed — only in the README, where it also
  queried a table the schema had already dropped. Removed rather than reimplemented.
- **No `--force` alias for `prune --delete`.** Deleting working copies is spelled
  out in full.
- **No `rfo` shim.** This project was renamed from `rfo`. If you relied on the
  old binary name, it is gone.
