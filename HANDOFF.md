# HANDOFF

State of `ro` as of this handoff. Written so the next session does not have
to rediscover what was tested, what was only read, and what is still broken.

**Read the "Not verified" and "Known broken" sections before trusting any of
this.** Several things here were found by *running* the fleet, and the pattern
was consistent: every real bug was in code that was individually correct, and
the test suite was green through all of it.

---

## What this tool is

`ro` — a multi-repo Git fleet CLI. 15 commands, 11 crates. Register a set of
local repos, then commit and push them in one invocation with a per-repo
author and per-repo credential, optionally through a coding agent
(`claude` / `codex` / `git`).

Full design in `PLAN.md`. The plan is the spec; where the plan and the binary
disagreed, this document says which one is wrong.

---

## Verified working (real runs, not reading)

Against real bare remotes, a real `claude` install, and a clean-machine
`ro init`:

- The daily loop: `list` · `status` · `sync` · `commit` · `push` · `ship` ·
  `doctor` · `config` · `schema` · `remove`
- **Per-repo identity** — two repos, two `[identity.*]` profiles, one command,
  two real authors. Applied per-invocation with `-c user.name/user.email`, so
  nothing is written to `.git/config`.
- **Multi-commit split** — a real `claude` run over two files produced two
  separate commits. This required changing the engine contract to a plan
  naming files per group.
- **Tags** — `tag` / `untag` / `tags` and `--tag` / `--group` selection.
- `--format json|ndjson` on the fleet verbs, `--message`, `--amend`,
  `--prompt`, `--prune`, `--paths`, `--dirty` / `--ahead` / `--behind`.
- Config: `ro init` writes four tables and every one is read; a config
  carrying a cut table gets a migration warning rather than going quiet.

---

## Bugs found by running, and fixed

| Bug | Consequence |
|---|---|
| `git pull` given a **branch where the remote goes** | **Every `ro sync` in the fleet failed.** "does not appear to be a git repository" |
| A test asserting the pull was refused, passing because of the bug above | Pinned the bug in place |
| `extraheader` scoped to `http.https://{host}/` — scheme hardcoded | Credential silently dropped on any plain-HTTP remote; git falls back to the machine credential; **possible wrong-account push** |
| `host_for_extraheader` returned `localhost` for every non-HTTP remote | Every SSH repo's credential was a silent no-op |
| 10 of 14 config tables had **no reader**; two were *validated* | `ro doctor` would refuse to start over a bad value in a key that changed nothing |
| `ro doctor --fix` added the two cut tables, omitted three live ones | The repair command wrote config nobody could use |
| `claude -p --output-format=stream-json` without `--verbose` | `ro commit` failed on first use, on the only machine with `claude` installed |
| Engine "several commits" committed the whole index per subject | First commit took everything, the rest failed |
| A clean tree read as "nothing to commit" when the agent had committed | False report; `ro ship` would skip the push |
| `AppConfig` had no `identity` field | `global_identity` hardcoded `None`; per-repo author was advertised and inert |
| `author_ref` read as a literal name | `ro add --author work` → commit authored `work@localhost`, reported as success |
| `--all` tested before the filter | `--status --all --tag work` selected the whole fleet, dropping the tag |
| `ro add` wrote `.ro/config.local.toml` | Made the repo dirty on enrolment; second copy of the registry |
| A freshly-created branch recorded as a sync **failure** | Healthy fleet full of red |
| Summary line said `0 failed` over a wedged fleet | A run that handed work back, reported as one that had not |
| 11 flags the plan promised did not exist | — |

---

## Known broken — found, not yet fixed

Reported by a peer session that ran each for real. **None are fixed; all are
in files that were mid-edit when it reported.**

1. **`--resolve` is dead code.** `rebase_onto_remote_base` returns `Err` on
   conflict, and that `Err` hard-returns `RepoOutcome::Failed` *before* the
   resolve stage below it. The second engine dispatch is unreachable. Real
   run: `ro push --resolve` printed "the rebase conflicted" and dispatched
   nothing. The intent was right — the tree is left mid-rebase on purpose —
   and the control flow lost it.
   *Also:* `resolve.rs` discards the engine's outcome, so an engine that
   fails to spawn is reported as a conflict.

2. **`--onto` is broken two ways.** It does not change the rebase base (the
   rebase always uses `origin/HEAD`), and its refspec pushes a bare branch
   name, so git reads it as a *source* with an empty destination. The case
   the flag exists for — work on A, land on B — is the case that fails, and
   it commits locally first, leaving a commit and no push.

3. **`ro doctor`'s write check ignores `repo.credential_ref`.** It probes
   every repo with one ambient token. The push path *does* honour the row, so
   a per-repo credential makes doctor a permanent false alarm — and doctor
   never checks the credential the push will actually use.
   *The check itself is real and works*: with a token in the environment it
   reports `write: NO` correctly, which is the user's own motivating case
   reproduced.

4. **`--autostash` is not implemented on `ro sync`.** It only bypasses ro's
   dirty-skip. `PullOpts` has no autostash field. Git itself then refuses
   the merge. Worse, once wired: `git pull --autostash` **exits 0 when the
   pop conflicts**, leaving the user's work in a kept stash and the tree
   holding conflict markers. A green sync would be a lie.

5. **`agent.rs` reports only the first line of stderr.** Its own comment
   says to read the whole stream. `codex` writes a banner to stderr, so every
   codex failure reports a status line instead of the actual error 13 lines
   down. A red test exists.

6. **The engine test suite is not hermetic.** `resolve_program` searches
   `.exe` across the whole PATH before considering `.cmd`, so on a machine
   with real `claude.exe` / `codex.exe` installed the **real binary** runs
   instead of the test shim. This is the cause of the 4 engine test
   failures below. `ro-testkit` already fixed this class for `gh`.

---

## Not verified

Never run. Each is a real gap, not a claim of success.

- **`codex` engine end to end.** Dispatched once by a peer; every failure it
  produced was unreportable (see item 5 above). It has no credentials on
  this machine and its `~/.codex/config.toml` is invalid TOML, so every
  invocation dies at startup.
- **`env:` / `keychain:` credential end to end.** The resolver and the
  extraheader are unit-tested, and the no-silent-fallback refusal is
  verified (it correctly refuses and names the missing variable). But the
  fix to the hardcoded `https` scheme is **written and not yet verified** —
  it needs a re-run against the auth server.
- **`ro doctor` write check with a per-repo credential** (see item 3).
- **`--onto`** and **`--autostash`** — see items 2 and 4.
- **`ro remove --delete`** — the gated destructive path. Deliberately not run.

---

## The 4 failing tests

`ro-engine/tests/agent_engines.rs`:

```
the_prompt_arrives_as_exactly_one_argument
a_custom_prompt_replaces_the_builtin
a_non_zero_agent_exit_is_classified
a_hanging_engine_is_killed_and_reported_as_timed_out
```

**Pre-existing and environmental, and the cause is now known** (item 6
above): the shims are `.cmd`, the resolver prefers `.exe`, and this machine
has real agent binaries installed — so the real `claude` runs instead of the
shim. Proven pre-existing by stashing every change and re-running on
`1970dad`, which reproduces them identically. They pass on CI, where no
agent is installed.

---

## Environment gotchas on this machine

- **Git Bash / MINGW64** cannot hand a Windows child process a PATH entry or
  its argv. Any test that spawns a `.cmd` shim and reads what it recorded
  will fail here and pass on CI.
- **Memory is tight.** The linker dies with `LNK1102 out of memory` and
  `LNK1318`, and rustc once crashed with `STATUS_STACK_BUFFER_OVERRUN`.
  Build with `CARGO_PROFILE_DEV_DEBUG=0` and `-j 1`. If a build dies that
  way, it is the machine, not the code.
- **Never run a test that can pop a credential dialog.** `ro`'s own git
  calls are hardened (`GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=Never`,
  set after the caller's env so they cannot be relaxed). But a *hand-typed*
  `git clone` over HTTP is not, and it opens a modal on the user's screen.
  Export both vars before any ad-hoc git.
- **CRLF.** Line-number-based scripts to edit source land on the wrong line
  and have twice deleted core functions. Use the Edit tool.

---

## Token hygiene

A classic PAT was pasted into this session's transcript and used for
`git push`. It has been in the log the whole time, which is the exact leak
the design exists to prevent — a credential should never sit somewhere a
model can read it. **Revoke it.** Next time, `gh auth login` once and the
token lives in the keyring where nothing has to paste it.

---

## Suggested order for the next session

1. Re-verify the `https`-scheme fix against the auth server (it is written,
   unverified, and it is a wrong-account-push risk).
2. `--resolve` — the conflict `Err` hard-returns before the resolve stage.
   One control-flow change, and a red test already exists.
3. `--onto` — two independent bugs; both need a test that the work lands on
   the named branch.
4. `sync --autostash`, and handle the pop-conflicts-exit-0 case while
   doing it, or the feature ships a green lie.
5. The engine stderr first-line bug, then make the shims win over a real
   install so the 4 tests prove something.
6. `doctor` honouring `repo.credential_ref`.
