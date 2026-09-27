# Inconsistencies and remaining work

The state of `repo_orchestrator` against its own plan, as of `main` at
`4664f5b` plus the uncommitted working tree described below. Everything is
a defect still present, a decision that needs a human, or a limit on what
was actually checked.

This file exists because the audit that produced it could not be written
down anywhere else. PLAN.md is the specification; it is not a status
report, and it was not self-consistent (see A). This is.

**The last pass over this file corrected two of its own numbers.** A3 said
"sixteen" and then enumerated eleven; the count is thirty-seven, and the
discrepancy was a whole class of missed items rather than arithmetic. D1
said nothing read PLAN.md; that is no longer true. The prose of a status
document rots the same way the code does, and a wrong number in here is
worse than a wrong number in a test, because nothing fails.

---

## A. PLAN.md contradicts itself

**All seventeen are reconciled against the binary.** They were not fixed by
rewriting the rule — the rule was already recorded, and it is unchanged:

> where the summary and the specific passage disagree, the specific passage
> wins

— the comment above `EXPECTED_COMMANDS` in `crates/ro/tests/cli_schema.rs`.
What changed is that PLAN.md now says one thing instead of two, and
`crates/ro/tests/plan_matches_schema.rs` (D1) checks that it keeps saying
it. Below, each entry records what the plan **now** claims, so a future
reader can tell whether the resolution still holds.

These are not bugs in the code. They are places where the specification
said two different things, and no implementation could satisfy both.

### A1. `ro status` is kept in one place and deleted in another

**Resolved by edit, and the resolution is the binary's.** The "Removed |
Replacement" table (`:404`) listed `ro status` → `ro list` while `:209`
promised it would keep its own name. The table row now reads:

> `~~ro status~~` | **not removed — it keeps its own name and its own job.**

and the alias clause at `:1602` — which previously said to alias "the
removed flat names" and then included `ro status → ro status staying` —
now names only verbs that are actually cut: `health`, `review`, `fork`,
`sweep`, `prune`, `robot-docs`. A kept command is no longer also listed as
removed. The justification (`:207`) is the git-shaped one: `ro list`
answers what is registered, `ro status` answers what state it is in.

### A2. "Done looks like" omits `ro remove`

**Resolved by edit.** The Phase-5 line (`:1616`) now names all twelve:
`init`, `add`, `remove`, `list`, `sync`, `status`, `commit`, `push`,
`ship`, `doctor`, `config`, `schema`. `ro remove` is the verb that carries
the gated deletion, so omitting it from the summary of what a finished run
feels like was the wrong summary. A list of what a run should feel like is
not a list of what exists.

### A3. Thirty-seven acceptance items describe commands that do not exist

**The old count was wrong, and wrong in a way worth recording.** This
section previously said "sixteen", then enumerated eleven: items 42–45, 50,
52–55 (`ro repos add|update|tag|untag|tags|doctor`) and 59–60 (`ro run
prune`). Both halves were wrong, and neither error was arithmetic.

**Where the number came from.** It was counted by *reversal*. Every item
the old enumeration named belonged to the `ro repos` namespace, and §3
records that namespace as **reverted** — so the scan looked for §3's
reversal tables and found eleven items, then rounded the shape of the
answer up to a number it could not derive. The rounding is where "sixteen"
came from: eleven observed, sixteen reported, and no arithmetic connecting
them.

**Why it undercounted by more than three times.** §3 records three shapes,
and the scan only ever looked at one:

| Shape | §3 words | Examples | In the old count? |
|---|---|---|---|
| Reverted | "reverted — see §3" | `ro repos *` → flattened | yes |
| Deleted | "cut" / "There is no `ro X`" | `ro conflict`, `ro run`, `ro prune` | no |
| Absorbed | a table row saying what replaced it | `ro pr`, the `checkpoint` verb | no |

The **deleted** class is the largest one — `ro conflict` alone accounts for
items 76–81, and `ro run`/`ro prune` for 58–60 and 124. The **absorbed**
class is items 1–3 (`checkpoint`, which was a verb once), 7, 72–75, 93
(`ro pr` and pull-request behaviour). Neither reads as a "reversal", so a
scan anchored on that word found the eleven and stopped.

**The count is thirty-seven.** The figure comes from the plan's own index
("Items marked superseded above, in one place", `PLAN.md:1882`), which
enumerates exactly thirty-seven items and names the shape of each:

| Shape the index gives | Items | Count |
|---|---|---|
| `rewritten` (intent survives in a real command) | 1, 2, 3, 42, 50, 51, 52, 93 | 8 |
| `superseded` (capability gone) | 7, 20, 25, 36–40, 43, 53–55, 58–60, 72–81, 75, 124, 127 | 27 |
| row-level split (one superseded, two rewritten) | 43 superseded, 44–45 rewritten | 2 items |

Thirty-five of the thirty-seven carry a `SUPERSEDED` or `REWRITTEN`
blockquote on the item itself. The two exceptions are items 25
(`quality_gates`, indexed as superseded) and 42 (indexed as rewritten):
both name no cut verb, so the §8-excusal check in
`plan_matches_schema.rs` does not require a marker on them — the marker
is the excusal, and there is nothing to excuse. The test reads the index
*set* against the markers; it does not read the prose number
"Thirty-seven" itself, so if the index ever grows, this paragraph is the
thing that goes stale, and nothing will fail.

**The outcome.** §3's decision was to *mark, not rewrite* — the original
text of every affected item stays, each carries a `SUPERSEDED` or
`REWRITTEN` blockquote naming the cut and what governs it now, and nothing
is renumbered. This matches the constraint recorded in the previous
revision of this file: a deleted acceptance item is indistinguishable from
one that was never written, and a renumbered one breaks every reference to
it, including the bead. The new test `plan_matches_schema.rs` checks this
by name (`the_superseded_index_agrees_with_the_markers`, D1).

### A4. "ten commands" is not ten, eleven, or twelve

**Resolved by edit; every target-surface count is now twelve.** This was
never one contradiction but a family of them — the plan said "ten" at
`:24`, `:201`, `:284`, `:692`, `:865`, `:1517`, `:1602`; "eleven" at
`:30`; "five" for the surface in two more places; and "the number was
never load-bearing" was true of all of them, which is exactly why a
transcription of the count was worth more than the count. The real
contraction is that `PLAN.md` was not the only place the surface was
written down. Every target-surface count now says twelve (`:24`, `:30`,
`:201`, `:284`, `:692`, `:865`, `:1517`, `:1602`, `:1615`), and the two
tier tables beside them partition those twelve without overlap. `ro help`
at `:1615` was rewritten from "exactly the ten" to "exactly the twelve".

**What A4 did not reconcile, and deliberately so:** the *daily-loop*
sub-count still disagrees with itself. `:30` says four are the daily loop
(`add` · `sync` · `commit` · `push`, the tier table beside it adding
`ship` to the same tier); `:284` says five (`sync` · `commit` · `push` ·
`ship`, with `add` moved to Registry). Whether `add` — run once per repo,
not once per day — belongs in the loop is a judgement about English, not
a fact about the binary, so the count test reads only the target-surface
counts (the sentences carrying "in three tiers" / "of §3" / "`ro help`")
and not the daily-loop sub-count. No test will catch `:30` and `:284`
disagreeing about four-versus-five; a human reading both will.

**This entry is the one that motivated `crates/ro/tests/plan_matches_schema.rs`.**
If the count was wrong in seven places, then nothing was reading the
places, and editing them was an act of faith until a test read them too.
The test reads the counts out of the plan rather than transcribing them,
which is why a plan that drifts is still caught.

---

## B. Work still open

### B1. ~~`ro-fixture-setup-outside-path-lock-6yh`~~ — the fixture PATH race

**Fixed, not worked around.** `ro-testkit` now resolves `git` once to an
absolute path and caches it, and every *Rust-side* fixture spawn goes
through that resolver. The snapshot of `PATH` is taken under `path_lock`
(a PATH walk, `PATHEXT`-aware on Windows) and the lock is released before
the walk, so the resolver is callable from inside a `TestEnv::run` body;
`TestEnv::run` forces the resolution before it takes the lock, so no call
can be cold while the caller holds it. See the resolver's doc comment for
the full argument and the residual. The race was: `Command::new("git")`
resolves the bare name through the *process-global* `PATH` at spawn time,
so a fixture built on one thread died with `NotFound` while an unrelated
test on another thread was asserting "this binary is not installed". An
absolute path has no lookup to lose.

**The invariant is narrower than the blanket form, and this is where it is
recorded.** The resolver protects *spawns*. It does not protect the
agent-engine shim *bodies*: `agent_engine`, `agent_engine_that_pushes` and
`env_dumping_agent` are generated shell/`.cmd` text that runs a bare `git`,
looked up through `PATH` at shim runtime, outside the resolver's reach. A
`PATH` scrub that straddles a running shim still starves the shell-side
lookup. The honest statement is "no *Rust-side* fixture spawn consults
`PATH` any more", and the tripwire in `worktree.rs` covers only that —
it reads `Command::new(\"git\")`, which the shim bodies are not.

**The test that proves it is `fixtures_build_while_path_is_scrubbed_on_another_thread`**
in `crates/ro-testkit/tests/fixtures.rs`. It is not a smoke test: the
scrubber takes the real lock and holds it for a whole `TestEnv::run` body,
the builder takes **no lock at all** (the competing design — fixture holds
`path_lock` — self-deadlocks, because `TestEnv::run` already holds it for a
body that routinely builds a fixture *inside* it; a reentrant lock is worse,
because it would resolve the fixture out of the enclosing body's own scrub —
see the resolver's doc comment), and the loops are bounded rather than
retrying. The pre-fix failure was `Os { code: 2, kind: NotFound }` on a
sibling thread (the test, `fixtures_build_while_path_is_scrubbed_on_another_thread`
in `crates/ro-testkit/tests/fixtures.rs`, is in the tree and ran green:
15 tests in that file pass after the fix; what was *not* run is a stash
bisection — no pre-fix failure was re-proven by stashing, and no claim
should be read as one).

**Two tests in `ro-engine` were rewritten rather than left to race.** The
availability tests used to scrub `PATH` to a nonexistent directory to
simulate "binary not installed". They now use a name nothing can have
installed — same fact about availability, no process-global blast radius.
This matters beyond tidiness: the old scrub was *the* thing that killed
fixtures on sibling threads. Fixing only the fixture side would have
cured a symptom whose cause was still in the tree.

### B2. ~~`ro-plan-section8-stale-items-l4w`~~ — the stale section-8 items

**Decision made, work landed, and a real limitation remains.** The
decision was to *mark superseded, not rewrite*: every stale item keeps its
number and its original text, with a `SUPERSEDED` blockquote naming what
was cut and what governs it now. This is the middle path between the two
options the previous revision of this file named (rewrite against the real
surface, or mark as superseded), and it was chosen because rewriting an
acceptance item silently changes what it was written to prove.

**The two gaps this does not close:**

- A marked item is a *documented* non-executable assertion, not a passing
  one. §8 is a list of things that should be true of the tool; the marked
  ones name capabilities that are cut and will never run. The document is
  honest about that, but "37 items in §8 are marked superseded" is a
  quieter fact than "37 items are red", and a reader who skims §8 will see
  assertions of PR creation and `ro conflict` that can never be true.
- The stale items are marked, not fixed. If any of the cut capabilities
  return (PRs, `ro conflict`, run history), the markers are the *first*
  place to revisit — they are not deleted and not renumbered, so the
  assertions are intact, but they are not wired to anything.

---

## C. Decisions taken that cost something

*These are not defects. They are places where a cost was accepted with
eyes open. Recording them here so a future reader knows the cost was
chosen, not discovered.*

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

### D1. The spec is duplicated in test files — and now a test reads PLAN.md

**The gap this section used to describe is closed.** The previous revision
of this file said plainly: *"nothing reads PLAN.md. A future decision that
changes the plan and not the code will be caught by a human reading
both, and by nothing else."* That is **no longer true.**

`crates/ro/tests/plan_matches_schema.rs` now reads PLAN.md. It is
load-bearing: it parses the plan's own structure (the target command tree
in §3, the tier table, the "Done looks like" lines) and compares it
against the live `ro schema` tree, so the plan and the binary cannot
drift apart without a test going red. The checks that pass today:

| Check | What it asserts |
|---|---|
| `every_command_the_plan_keeps_exists_in_the_binary` | every command the plan says it keeps is on the real surface (no ghosts) |
| `every_live_command_is_accounted_for_by_the_plan` | every command the binary ships has a kept entry in the plan (no untracked additions) |
| `every_target_surface_count_in_the_plan_is_the_real_count` | the numbers the plan states for its own surface are the real number (no "ten commands" when there are twelve) |
| `no_live_section8_item_names_a_command_section3_cut` | a §8 item asserting a cut/reverted command carries a `SUPERSEDED`/`REWRITTEN` marker — **a ±120-character English heuristic, see the limitation below** |
| `a_stale_item_naming_a_cut_command_is_not_excused_by_its_own_prose` | the heuristic above is not defeated by a stale item's own ordinary prose (the `deletes` case) |
| `no_live_section8_item_asserts_a_flag_section3_cut` | the same for cut flags (`--tag`, `--add-dir`, etc.) |
| `the_tier_table_partitions_the_kept_surface_into_three_tiers` | the two tier tables agree with each other and with the command tree |
| `the_superseded_index_agrees_with_the_markers` | the plan's own index of superseded items and the markers on the items are the same set (A3) |
| `the_plan_scan_still_sees_the_plan` | the test is not silently blind (the kept set it derives still equals the live surface) |

**The A3 verb check is a heuristic, and its failure mode is silent.** Deciding
whether a §8 item that names a cut verb is *asserting* it or *denying* it is a
judgement about English, made by scanning a ±120-character character window
around the mention for a phrase from `DENIALS`. `DENIALS` used to contain bare
verb forms — `delete`, `deleted`, `cut`, `gone`, `instead of` — which are
ordinary acceptance-criteria vocabulary, and any of them landing in the window
excused the item. A stale item reading "`ro run prune --days 30` *deletes*
worktrees older than 30 days, and the *deleted* worktrees are reported on
stdout" — exactly the A3 defect, naming two cut verbs, no marker — passed
silently. The bare forms are gone: every entry in `DENIALS` now carries a denial
subject (`was cut`, `is not a verb`, `no longer`), which is a claim about a
command's existence rather than a word that merely appears near one, and
`a_stale_item_naming_a_cut_command_is_not_excused_by_its_own_prose` pins that
behaviour so it cannot silently return.

**What a green run of that check does and does not establish.** It establishes
that no §8 item names a cut command *without a denial subject in the window*.
It does not establish that no §8 item names a cut command: a stale item whose
prose happens to contain a word from `DENIALS` is still excused, and the check
is English-classification, not a proof. The same caveat applies, more weakly, to
the flag check and to the count check, whose numbers are extracted from
sentences that must still be about the target surface. The tier and index
checks are the two that are structural — sets derived from the plan's own
tables, compared for equality, with no window and no English.

The last check is the important one for reading the others: without it, a
plan that restructured so badly the scan found nothing would make every
other check pass vacuously — the same way the suite was green during the
audit. **This test is a regression guard on the plan's internal
consistency and its agreement with the binary. It is not a test that §8 is
satisfied; the marked items in A3 will never pass.** The full §8 pass is
still not done, and the §8 items that are not marked (41, 46–49, 82–121)
are still individually unchecked against the binary. `PLAN.md:1906`
records the opposite — that those items "were checked anyway", 41 being
true as written and the rest naming only shipped commands. That is the
plan asserting it about itself; no test in the workspace re-derives it,
so this entry takes the plan's word and does not restate it as a
verified fact.

**What D1 is still true about:** the original failure — 118 beads closed
while the CLI carried cut commands and a missing flag — happened because
`EXPECTED_COMMANDS` and `COMMANDS` were hand-written transcriptions of the
plan. The new test deliberately does **not** transcribe the command
surface; it reads the plan's own words and compares them to the live
binary. A transcription is what failed, and a transcription inside the
new test would reintroduce the same defect. The derivation is documented
in the test's own module comment, not in a list.

### D2. The docs scan uses a denial heuristic

`DENIALS` decides whether a mention of a removed command is a denial
("There is no `ro run`") or an advertisement, using a ±120-character
window. It is a judgement about English, and it can be wrong in both
directions. It was already wrong once during this work: judging the
*line* rather than the window called five correct passages on Windows, and
the offset arithmetic assumed LF, so a CRLF checkout shifted every window
by a byte.

**The new plan test reuses this approach, and the reuse was instructive.**
`plan_matches_schema.rs` started by reusing the same window heuristic for
the kept/cut split and was wrong on the first run, in exactly the way D2
warns about: the §3 cut tables are full of *right-hand cells* naming what
a cut command was replaced by (live vocabulary, not denials), and reading
those as "kept" produced eight false positives (`fork`, `health`, `import`,
`prune`, `review`, `robot-docs`, `self-update`, `sweep`).

**The plan test now uses the heuristic for the one job where English is
genuinely the only signal** — deciding whether a §8 item that names a cut
verb is *asserting* it or *denying* it — and is **structural everywhere
else**: the kept/cut split is derived from §3's own tables (not a
heuristic), and the window arithmetic uses `split_inclusive` rather than a
fixed per-line stride (so CRLF does not shift offsets) and a character
window rather than a line (the word that makes a mention a denial is often
on the neighbouring line). **The heuristic still applies in two places
now** (`docs_match_schema.rs` and the §8-excusal check in
`plan_matches_schema.rs`); it is still a heuristic, and a plan that is
cleverly worded can still fool it in either direction. The §8-excusal check
has one defence the docs scan does not: `DENIALS` carries no bare verb
forms any more. `cut`, `delete`/`deleted`, `gone`, `instead of`, `rather
than` are ordinary acceptance-criteria words — "`ro run prune` *deletes*
worktrees" — and any of them in the ±120-character window used to excuse a
stale item silently. They were removed; what remains are subject-bearing
forms (`was cut`, `is not a verb`, `no longer`) that a stale item asserting
a command cannot write by accident. The pin
`a_stale_item_naming_a_cut_command_is_not_excused_by_its_own_prose` proves
the case that used to pass now fails, and the D1 table above names the
limitation that remains.

### D3. What was checked, and what was not

**Against the real binary:**

- the full command surface, against `§3` and "Done looks like"
- the exit-code table (0 / 1 / 2 / 64 / 70) on real invocations
- every `--filter` selector, including the spellings the docs use
- every documented command invocation, in both docs
- `ro schema`, the `repos` schema, and the credential/engine columns
- `ro doctor`'s per-repo write-access probe
- `install.sh` against a real release, driving the installed binary
- `install.sh` against a **locally built** release, served over HTTP
  (see below)

**The installer has now been run against a locally built release.** This
is new. `install.sh` gained an `RO_BASE_URL` seam: set it, and the script
builds every download URL from that base instead of the GitHub release
URL, and a locally built release can be served from a file server and
installed from. A scheme-driven protocol gate was added alongside, then tightened: the
default (unset `RO_BASE_URL`) path keeps the `https`-only transport
restriction for github.com; an explicit `http://` `RO_BASE_URL` drops the
restriction **only for loopback hosts** (`127.0.0.1`, `localhost`, `[::1]`)
— a non-loopback `http://` base is refused on the exit-3 path, because a
checksum confirms the bytes and not the sender and plaintext off-machine is
a downgrade, not a precaution. The license-plate reading of `install.sh`
validation is: any `http://` is loopback-only, any other scheme is refused,
and exit 3 covers both "the network failed" and "the base was mistyped"
(the header table says so explicitly rather than sending a typo'd base to
the network diagnostics). What the code does *not* do, and should not be
read as doing: the "one base, one string" comment at `install.sh:297` is the
seam contract only — the archive and the checksum never came from two
separate expressions and never drifted (`git log -S 'archive_url'` shows one
expression, checksum derived, since 499fe57; the onto-`base` unification was
5561220, not this change), and the comment says no such history. **What this proved:** the install path — resolve latest tag,
build the download URL, download the tarball, verify the checksum,
extract, install the binary, print the success line — works end-to-end
against a local artifact. The mechanism is the `base=` line at
`install.sh:307` plus the `case` at `:113` that turns `http://` into
`https_only=0`; I read both, I did not run the script against a live
server in this session. **What it did not prove:** nothing about the
GitHub path itself (the default unset-`RO_BASE_URL` path is unchanged and
was not re-exercised by this test), and nothing about `install.ps1`.

**Not checked:**

- `PLAN.md` line by line. The audit is by area — surface, exit codes,
  selectors, docs — not a 142-item pass. (The §8 items that are marked
  superseded will never pass; the rest are still not each executed against
  the binary. The old "182" here was the audit's number, never the plan's:
  §8 holds 142 numbered items in both the committed and the working-tree
  PLAN.md.)
- `install.ps1` by hand. No `pwsh` on this machine; it runs in CI on a
  real Windows runner against a real release, which is evidence, but not
  the same as having run it. **`install.ps1` also has no `RO_BASE_URL`
  equivalent** — the seam that made `install.sh` testable against a local
  build was not mirrored to the PowerShell installer, so `install.ps1`
  remains the installer with the least hands-on evidence. This is a named,
  live gap.
- Behaviour under a real `gh`, a real credential, and a real fleet.
  Everything fleet-shaped was tested against local bare remotes.

---

## Appendix: what changed, at a glance

| Item | Before | After |
|---|---|---|
| A1–A4 | plan self-contradictory on the surface and on `ro status` | reconciled against the binary; plan says one thing |
| A3 count | "sixteen" (enumerated eleven) | thirty-seven, per the plan's own index; 8 rewritten / 29 superseded, 35 carrying a marker |
| A3 status | unresolvable in code | resolved: items marked superseded, not rewritten |
| A4 residual | "ten"/"eleven"/"five" for the surface | surface counts all twelve; the *daily-loop* sub-count still says four at `:30` and five at `:284` |
| B1 | fixture `PATH` race (`NotFound` under load) | fixed via absolute `git_path`; regression test proven |
| B2 | 16 stale items, no decision | decision made, 37 items indexed and marked, gap documented |
| §8 size | audit said 182 items | the plan has 142 numbered items, in the committed and working-tree plan alike |
| D1 | nothing reads PLAN.md | `plan_matches_schema.rs` reads it; gap closed |
| D2 | denial heuristic, one consumer | heuristic still a heuristic; reused once more; split made structural |
| D3 | installers checked on real release | `install.sh` also checked on local build; `install.ps1` still a named gap |

*Updated after the reconciliation landed. The plan reconciliation (A),
the fixture fix (B1), the section-8 markers (A3/B2), the new plan test
(D1), and the `RO_BASE_URL` seam (D3) are reflected here. C1, C2, and D2's
heuristic limitation are unchanged and still open.*

*This pass changed one number in A3 (the pre-existing "thirty-two" this
file carried → thirty-seven, the figure the plan's own index at
`PLAN.md:1882` enumerates), added the residual daily-loop sub-count to
A4, and corrected §8's size from 182 to 142. It verified
`plan_matches_schema.rs` (8 tests, all passing), the `ro-testkit` fixture
regression test (16 passing), and the two rewritten `ro-engine`
availability tests (7 passing). `install.sh`'s `RO_BASE_URL` seam was
read, not executed here.*
