# Changelog

All notable changes to `ro` are recorded here. The format is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Every entry below was found by **running the tool**, not by reading it. The
recurring lesson of this project: individually-correct code, a green suite, and
a real defect — all at once.

---

## [0.4.1] — 2026-09-30

Two defects, both found by running the tool rather than reading it. One
changes what the summary line claims; the other is a hang on Windows.

### Fixed

- **A push was reported as a commit.** `Summary::committed` counted every
  `Pushed` row, on the reasoning that a push implies a commit. That holds for
  the ordinary path — the engine commits and `ro` pushes what it wrote — and
  fails for three ordinary ones: a clean worktree, where the engine had nothing
  to group; a worktree whose every change the engine judged ephemeral, so it
  declined all of them and left the work uncommitted; and a branch the remote
  did not have, pushed at a commit that was already there. All three push
  something real, so `pushed()` was never wrong. A run against a single
  uncommitted scratch note printed `1 committed, 1 pushed, 0 failed` with the
  note still untracked in the worktree and the branch on the remote
  byte-identical to its base — a green summary over work that did not land,
  which is the one thing a summary line must never be. `Pushed` now carries
  `engine_committed`, set only where the engine actually reported a commit.
- **`ro` hung on Windows when an engine left a background process.** The
  timeout path reached the engine's descendants with `taskkill /T /F /PID
  <child>`, and `taskkill /T` walks the *live* process list — so once the
  direct child had been reaped, which is exactly the case the code path is
  for, it found nothing, reported success and killed nothing. The descendant
  kept the inherited pipe write ends open and the run blocked on a join that
  could not return: an agent that leaves a dev server, a watcher or a build
  running hung `ro` until that process finished on its own. A Windows Job
  Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is now created before the
  spawn and holds the tree, which is the only Windows construct that binds a
  tree to a handle this process holds; `taskkill` remains as the fallback for
  a host that will not nest jobs.

### Changed

- **The release pipeline now gates on CI.** It is tag-triggered, which meant it
  ran on whatever commit the tag pointed at — including a red one. v0.4.0 was
  cut from a commit whose Format, Clippy and all three Test jobs were failing,
  and every job built an artifact from it anyway. A tag is a claim about a
  commit and nothing was checking the claim. The gate reads CI's own conclusion
  for the tagged SHA rather than re-running the suite, so it cannot disagree
  with the run a developer already watched go green.

### Tests

- Six fixtures that could not pass on Windows or macOS, each of which had been
  reporting a platform fact as a defect: two shims written as `#!/bin/sh` that
  the platform refuses to execute at all, a `cmd.exe` batch script holding a
  value in single quotes, two tests comparing a registry row against a raw
  fixture path where `ro add` stores the canonicalized one, and a pull fixture
  inheriting `core.autocrlf=true` from the machine and failing on a carriage
  return nobody wrote.

---

## [0.4.0] — 2026-09-30

### Security

- **A per-repo credential no longer reaches the engine.** The strip was a
  six-name list, and `credential_ref = "env:WORK_GH_TOKEN"` names a variable no
  list can contain — the user chose the name, and the variable is a secret by
  construction. It walked straight into the agent's environment: the exact leak
  the module exists to prevent, arriving through the door the per-repo
  credential feature opened. Two tiers now: a recognised shape (`ghp_`,
  `github_pat_`, `sk-`, a JWT) is conclusive on its own; a bare long token is
  stripped only when the name says it holds a secret, so `PATH` and a prose
  value in `API_KEY` both survive.
- **`ro remove --delete` no longer deletes a checkout containing an
  unregistered repository.** The nested-clone check consulted the registry, so
  it only fired when the nested repository happened to be a row too. A stray
  `git clone` inside a checkout is not a row — it is exactly the thing nobody
  registered — and it sailed straight through, destroying a user's uncommitted
  work with exit 0 and "Deleted working copy" in the log. The registry cannot
  answer this question, because the thing at risk is the thing nobody
  registered; the directory is where it has to be read from.

### Fixed

- **No git behind `ro` could open a password dialog.** `GIT_TERMINAL_PROMPT=0`
  was the only credential-suppressing variable in the workspace, and it
  suppresses a *terminal*, not a credential *helper*. A helper is a separate
  program git runs before it ever considers prompting, and it is read from the
  machine's own git config — a file `env_clear()` does not touch. On Windows
  that helper is Git Credential Manager, whose prompt is a GUI window. Every
  `ro sync` and `ro ship` against a remote `ro` had no credential for opened
  one on the user's screen, mid-fleet, with no terminal attached to explain it;
  and a helper that *can* answer is worse, because that is a fetch as the wrong
  account. The helper list is now emptied rather than the prompt muted:
  `-c credential.helper=` on every git `ro` runs, in the engine's own
  environment, and in the test fixtures, which is where the dialog kept coming
  back from.
- **`ro ship` fetched anonymously, so it failed on every private repo.** Only
  the push carried the resolved credential. The fetch now carries it too,
  scoped to the row's actual scheme+host, for the invocation only.
- **`--filter health:999` selected the whole fleet and ran a real sync against
  every repo.** The comparison was `score < N` with N unbounded, so any N above
  the maximum score of 100 was always true and never checked. Now `score <= N`,
  inclusive, and both impossible ends are refused. The inclusive end is the
  other half of the decision: under the old strict `<`, a repo driven to
  exactly 0 was reachable by no threshold a person types, so the sickest repos
  in the fleet were the only ones the sick-repo filter could never return.
- **`--pattern` + `--filter` discarded the filter.** `ro sync "proj/*" --filter
  health:999` synced every repo the glob matched. The filter branch was an
  `else if` behind the token branch — the widening bug arriving through the one
  combination nobody had tested, and the one a user is most likely to type.
- **A repo with no `origin` rendered as `ahead=0 behind=0`**, indistinguishable
  from a healthy in-sync repo. `ro sync` called the same repo out correctly;
  `ro status` rendered it clean. `RepoStatus` now carries `has_upstream`.
- **`ro sync --dry-run` said "in line" about a ref that may be weeks old.** Every
  "looks fine" branch now carries a staleness caveat built from the ref's own
  reflog age. `plans_match` — the function whose doc comment states the
  invariant that was violated — was called only from `#[cfg(test)]` code; it now
  runs in production.
- **`--timeout` reached exactly one git call.** It now reaches every one, with a
  process-group kill, so a hanging remote returns at the deadline and the next
  repo still runs.
- **`ro sync --prune` passed no remote name and was a usage error.** `git remote
  prune` requires the remote; `run_in` returns `Ok` for a non-zero exit and the
  caller discarded it with `let _ =`. A flag that existed in `--help`, had a
  full help paragraph, and changed nothing.
- **`.env` at any depth below the repo root was committed and pushed.** The
  denylist's `.env` patterns lacked the `**/` prefix its three siblings have.
- **One NUL byte in the first 8000 bytes disabled the secret scan for a whole
  file**, so a `ghp_`-shaped token in a real-looking text file committed and
  pushed as a clean run. `looks_binary` was a presence test; it is now a density
  test, which keeps genuinely binary blobs skipped while scanning text with a
  stray byte.
- **A failed fetch was reported as success.** `fetch` returns `Ok` for a non-zero
  exit, so the check that matched only `Err` could never fire. `ro ship` committed
  and pushed onto a base it never fetched.
- **`ro sync` never set its process exit code.** `run_exit_code` was computed,
  written to the `runs` table, and dropped before any script could see it. A
  fleet-wide failure exited 0.
- **A `HandedOver` row was counted as nothing at all** — a fleet of three wedged
  repos printed "0 committed, 0 pushed, 0 failed" and exited 0.
- **`--amend` on a HEAD already on the remote was not refused** despite the help
  promising it; it amended a published commit, reported `committed`, exited 0,
  and left the branch diverged.
- **`--onto` was a total bypass of the protected-branch rule.** The guard tested
  the base branch and was skipped whenever `--onto` was present, so `--onto main`
  pushed straight onto `main`. The destination is now checked too.
- **`--amend` silently replaced the subject the user wrote** with the engine's
  own placeholder, because it read the subject of the commit the engine had just
  created rather than the one underneath.
- **`ro ship a b` — the documented plural form — selected nothing.** The caller
  joined the names with a comma and the resolver splits on whitespace, so the
  two-name form was unusable on three of five verbs.
- **`--all` discarded a narrower `--pattern`** and ran the whole fleet.
- **`ro status <name>` matching nothing was a silent success** (0 bytes, exit 0)
  while `ro sync <name>` exits 64, `ro tag <name>` exits 70, and `ro remove
  <name>` succeeds. Four verbs, four answers to "what does the name alpha mean".
- **`ro config set "core.parallel 4=1"` panicked.** A config tool that panics on
  a typo is worse than one that rejects it.
- **`[auth]` never got the "did you mean"** every other table had, because
  `deny_unknown_fields` made the suggestion branch unreachable.
- **Six config keys shipped documented and were read by nothing**
  (`core.layout`, `core.parallel`, `core.projects_dir`, `core.timeout_secs`,
  `github.auth`, `github.host`). They now have readers, verified as real call
  sites rather than a weakened test.
- **`ro status` reported only the first line of an agent's stderr**, so a codex
  failure named its banner instead of the 401 thirteen lines down.
- **The engine test suite was not hermetic.** `resolve_program` searched `.exe`
  across the whole PATH before `.cmd`, so a real `claude.exe` shadowed the test
  shim. PATH entries are now iterated before extensions.
- **`--resolve` was dead code.** The conflict `Err` hard-returned before the
  resolve stage, so the second engine dispatch was unreachable for exactly the
  case it exists to handle.
- **`--onto` did not change the rebase base**, and pushed a bare branch name that
  git read as a source with an empty destination.
- **`ro doctor` probed every row with one ambient token**, so a per-repo
  credential was a permanent false alarm.
- **`--autoshash` never reached git**, and once wired, a conflicting pop exits 0
  — so a green sync meant the user's work was sitting in an untold-about stash.
  The tree is now read rather than the exit code, and a conflicting pop is a
  per-repo failure naming `git stash list`.
- **A leftover autostash wedged the repo forever.** `autostash_still_held`
  substring-matched the word in `git stash list`, so a stash from a previous
  conflict made every later sync report a false conflict. It now compares stash
  ids snapshotted before the pull.
- **`.ro/config.local.toml`'s credential was resolved and thrown away** —
  `RepoPlan` was built from the *unmerged* row value, so an operator believed the
  local file pinned the credential while the row's other token left the machine.
- **`NothingToCommit` returned before the push**, so a run whose commit came from
  the rebase reported "nothing to commit" and never pushed.
- **`rebase --continue` leaves `REBASE_HEAD` behind**, so `guards` skipped the
  repo forever after.
- **`ro status` was blind to a conflicting autostash pop**, which writes no
  `MERGE_HEAD`/`REBASE_HEAD`. The unmerged index is now a first-class conflict
  signal, surfaced in text, json and ndjson.
- **`ro sync --timeout` was documented but did not fire** on a hanging remote.
- **`--message` did not reach the agent path**, so a user who asked for one commit
  got N.
- **The agent's own commit carried the agent's identity**, not ro's — the exact
  outcome the prompt's "Do NOT run `git commit`" line exists to prevent.
- **`.git/config` tampering was not reported**, though the module doc promised a
  hash-and-report control that did not exist.
- **The 600-second hang.** `run_with_deadline` polled `try_wait` without draining
  the pipes, so any engine writing more than a 64 KiB pipe buffer blocked and
  never exited. The real `claude` emits 73–86 KB, so the flagship path sat right
  at the edge. The readers are now joined under the same deadline, and the
  process group is killed when a descendant holds the pipes open.
- **`ro sync` now runs repos concurrently**, bounded by `core.parallel`, with
  results in registry order and the `runs` row written exactly once.
- **The summary row for a `--onto` run names the branch that was actually
  written**, not the checkout branch.
- **`ro schema` now publishes `takes_value`**, so a consumer can tell an arg that
  consumes an operand from a `SetTrue` flag without guessing from the presence of
  a `values` list — and `--message <MSG>` consumes an operand with no list at all.

### Documentation

- **README no longer claims `ro sync` is parallel** — it is not, and
  `core.parallel` bounds the fleet verbs instead.
- **README no longer promises `ro plan` / `ro apply` / `ro rollback`** — all three
  exit 2 as unrecognized subcommands.
- **README no longer lists `ro-testkit` among the crates the CLI depends on** —
  it is a dev-dependency, compiled into the test suite and never linked into the
  shipped binary.
- **README no longer says "64 = bad usage"** — clap usage errors are 2; 64 is a
  ro usage error, produced by a different layer for a different reason.
- **FEATURES.md no longer documents a `toon` format or eight flags that do not
  exist**, and its "Dry run is the default" headline is no longer inverted.

### Tests

- **911 tests pass, 0 fail** (was 572 at the start of this work).
- A test that lost its `#[test]` in an edit and sat complete and unrun was
  restored; a duplicated `#[test]` that ran another twice was removed.
- `schema_publishes_values_only_for_args_that_take_a_value` recursed through
  `subcommands` while the root hangs its children off `commands`, so it visited
  3 of 89 args and passed. A green suite that is not running the test it claims
  to run is worse than no test, because it counts as coverage it did not provide.
- `delete_refuses_a_parent_of_the_real_checkout` asserted the parent's origin
  matched the row's `clone_url`; it did not, so the origin gate fired first and
  the nested check was never reached. It passed, and its comment claimed
  otherwise.
- Four tests now pin the README corrections, so the docs cannot drift back.

---

## [0.3.0] — 2026-09-28

The first wave: the six items HANDOFF.md listed as known-broken, plus sixteen
more found by driving the real binary over real bare remotes.

- `--resolve` dead code, `--onto` broken two ways, `doctor` ignoring
  `credential_ref`, first-line-only stderr, non-hermetic engine tests,
  `--autostash` never reaching git.
- `ro remove --delete` destroying a registered repo nested in its target.
- `.env` at any depth committed and pushed; one NUL byte disabling the secret
  scan.
- A failed fetch reported as success; `ro sync` never setting its exit code;
  `--prune` passing no remote name.
- `--all` discarding a narrower request; a bare name selecting nothing.
- A config typo silently written; six config keys read by nothing.
- The delete gate running `git remote get-url` with the CWD set to the target, so
  it read the target's own config.

---

## [0.2.0] — 2026-09-27

Per-repo identity, tag commands, the config tables, and the first end-to-end
verification of the daily loop against real bare remotes.
