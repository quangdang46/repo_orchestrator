# Inconsistencies and remaining work

The state of `repo_orchestrator` against its own plan, as of `main` at
`5b6001f`. Everything below is either a defect still present, a decision
that needs a human, or a limit on what was actually checked.

This file exists because the audit that produced it could not be written
down anywhere else. PLAN.md is the specification; it is not a status
report, and it is not self-consistent (see A). This is.

---

## A. PLAN.md contradicts itself

These are not bugs in the code. They are places where the specification
says two different things, and no implementation can satisfy both. Each
one was resolved by a stated rule — **the more specific statement wins
over the summary table** — and the rule is recorded in the comment above
`EXPECTED_COMMANDS` in `crates/ro/tests/cli_schema.rs` so the next
person does not have to re-derive it.

### A1. `ro status` is kept in one place and deleted in another

| Line | Says |
|---|---|
| `:209` | "**stays `ro status`** … Kept as its own command" |
| `:1616` | listed in "Done looks like" |
| `:70` | "`ro status` shows the raw facts" |
| `:407` | `ro status` → `ro list` (removed, replaced) |

**Resolved: kept.** Three statements to one, and `:209` answers `:407`
explicitly with "see the note below". The summary table is the outlier.

### A2. "Done looks like" omits `ro remove`

`:1616` lists eleven names. `ro remove` is not among them — but `:201`
and `:208` both keep it ("Three commands keep their names — `ro add`,
`ro list`, `ro remove`"), and it is the verb that now carries the gated
deletion. The actual surface is twelve commands.

**Resolved: `ro remove` kept.** A list of what a run should feel like
is not a list of what exists.

### A3. Sixteen acceptance items describe commands that do not exist

`§8` has 182 numbered acceptance items. Sixteen of them are written
against commands `§3` explicitly cut or reverted:

- items 42–45, 50, 52–55: `ro repos add|update|tag|untag|tags|doctor`
  — `§3` and `:82` both say the `ro repos` namespace is **reverted**
- items 59–60: `ro run prune` — both `ro run` and `ro prune` are cut

**Unresolved, and unresolvable in code.** These items cannot pass and
were never meant to survive the reversal that `§3` records. Tracked as
`ro-plan-section8-stale-items`.

### A4. "ten commands" is not ten, eleven, or twelve

`:1615` says `ro help` lists "exactly the ten commands". The explicit
list beside it has eleven names, and the true surface is twelve. The
number is prose, not a requirement; the list next to it is the
requirement.

---

## B. Work still open

### B1. `ro-fixture-setup-outside-path-lock-6yh` — a flaky test

`Worktree::with_one_commit()` runs `git` **outside** `path_lock()`.
A test that scrubs `PATH` concurrently — `a_missing_binary_is_unavailable_not_a_process_failure`
and `the_availability_probe_distinguishes_present_from_missing` both set
it to a nonexistent directory — can make an unrelated fixture fail to
find `git`:

```
git runs: Os { code: 2, kind: NotFound }
```

Passes in isolation, failed once under load. Fixing it is not free: the
lock is a plain `Mutex`, so making fixture setup take it would deadlock
against `TestEnv::run`, which already holds it. The fix needs a
reentrancy story, not a one-line `lock()`.

### B2. `ro-plan-section8-stale-items-l4w` — the sixteen items

See A3. Needs a decision: rewrite the items against the real surface, or
mark them superseded. Neither is a code change.

---

## C. Decisions taken that cost something

### C1. Linux release binaries have no keychain

`cargo-dist` cannot set features per target
([axodotdev/cargo-dist#762](https://github.com/axodotdev/cargo-dist/issues/772)
is open), and a statically linked musl binary cannot link a glibc
`libdbus`. So `default-features = false` applies to the whole release,
and `ro-core`'s `keychain` is compiled out of every shipped binary,
macOS and Windows included.

**A released binary resolves credentials through `env:VAR_NAME`.** That
is the stated design of the feature — "Turning it off must still leave a
working tool" — and a static binary is what lands on headless machines
and CI runners, where no keyring is running for `keyring` to find
anyway. The alternative was switching the Linux targets to gnu, which
trades the capability for a glibc version floor.

Recorded in `Cargo.toml` under `[workspace.metadata.dist]` and in the
v0.3.0 release notes. **Reversible** — a target change and one
`--features` line.

### C2. `ro sync --dry-run` reports intent, not a preview

It answers `would_pull` or `would_clone`. It does not predict a
conflict or a failed fetch. `ro ship --dry-run` is the fuller preview
and shares its guards with the real run. Left alone deliberately: it is
honest about what it does, and rewriting it is a design question.

---

## D. Known limits of the audit itself

Stated so the green suite is not read as more than it is.

### D1. The spec is duplicated in test files, and nothing checks PLAN.md

The reason 118 beads could be closed while the CLI carried six cut
commands and one missing flag: `EXPECTED_COMMANDS` and `COMMANDS` are
hand-written transcriptions of the plan, living in test files. When the
code and the transcription drifted the same way, every test was green.

`crates/ro/tests/docs_match_schema.rs` was rewritten to scan the
**documentation prose** rather than its own list, and it found seven
ghosts on the first run. It cannot do the same for PLAN.md.

**The gap that remains:** nothing reads PLAN.md. A future decision that
changes the plan and not the code will be caught by a human reading
both, and by nothing else.

### D2. The docs scan uses a denial heuristic

`DENIALS` decides whether a mention of a removed command is a denial
("There is no `ro run`") or an advertisement, using a ±120-character
window. It is a judgement about English, and it can be wrong in both
directions. It was already wrong once during this work: judging the
*line* rather than the window called five correct passages on Windows,
and the offset arithmetic assumed LF, so a CRLF checkout shifted every
window by a byte.

Verified against a real CRLF checkout. Still a heuristic.

### D3. What was checked, and what was not

Checked against the real binary:

- the full command surface, against `§3` and "Done looks like"
- the exit-code table (0 / 1 / 2 / 64 / 70) on real invocations
- every `--filter` selector, including the spellings the docs use
- every documented command invocation, in both docs
- `ro schema`, the `repos` schema, and the credential/engine columns
- `ro doctor`'s per-repo write-access probe
- both installers, on a real release, driving the installed binary

**Not checked:**

- `PLAN.md` line by line. The audit is by area — surface, exit codes,
  selectors, docs — not a 182-item pass.
- The `§8` acceptance items. Sixteen cannot run; the rest were not
  each executed against the binary.
- `install.ps1` by hand. No `pwsh` on this machine; it runs in CI on a
  real Windows runner against a real release, which is evidence, but not
  the same as having run it.
- Behaviour under a real `gh`, a real credential, and a real fleet.
  Everything fleet-shaped was tested against local bare remotes.
