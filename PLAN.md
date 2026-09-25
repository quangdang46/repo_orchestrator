# ro — Repositioning Plan

`ro` today is a 12-crate Rust workspace whose entire user-facing surface is 18 top-level clap subcommands crammed into one 1473-line file (`crates/ro/src/main.rs`), of which roughly a third is on the vision's cut list and none of the three things that matter most — `ro repos`, `ro checkpoint`, engine dispatch — exist. The goal is to strip the GitHub-first "orchestrator with a review lifecycle and a health score" down to an opinionated, agent-first fleet tool: one command (`ro checkpoint`) that scans a set of opted-in local repos, hands each dirty one to a pluggable commit engine (claude / codex / git), and commits + pushes either straight onto the current branch or onto a fresh WIP branch with a PR — with per-repo failure isolation, a real auth/identity policy, and a JSON surface an agent can drive headlessly.

## Where we are today

```
crates/ro          the binary: Cli + Commands(18) + one 861-line run() match
crates/ro-core     repo_spec + redaction (redact_secret has 0 callers)
crates/ro-config   single-file XDG TOML loader, 8 sections, ~90% dead keys
crates/ro-output   text/json/ndjson/toon renderers; 9 symbols called, 5 source files mostly dead
crates/ro-state    SQLite open_db + 3 migrations; queries.rs is 100% health score
crates/ro-git      shell-out git mutation + gix reads; no branch/stage-all/diff/env-injection
crates/ro-github   octocrab wrapper; only auth::discover_token is live (via ro doctor)
crates/ro-jobs     runs/run_events/failures accessors; every writer has 0 callers
crates/ro-sync     inventory CRUD (manage) + status + prune + the only sync loop
crates/ro-review   541-LOC crate whose apply_plan is a UPDATE-only no-op  -> CUT
crates/ro-sweep    deterministic conventional-commit splitter + safety scanners
crates/ro-dep-update 344 LOC, 0 callers, 0 dependents                     -> CUT
```

Hard ground truth that shapes everything below. `cargo check --workspace --all-targets` is green. **`cargo clippy --all-targets --all-features` is also red today** — one warning at `crates/ro-sync/src/prune.rs:135` (`if let Ok(p) = r` where only the `Ok` variant is used → `.flatten()`), and CI runs `-D warnings` on a 3-OS matrix, so the clippy job fails on all three platforms right now. **Tests: 2 Windows failures in `ro-sync` (`prune::tests::find_orphans_returns_empty_when_all_tracked`, `prune::tests::find_orphans_reports_untracked_working_copies`) plus 1 in `ro-github` (`import::tests::fetch_without_source_errors`) — the ro-github one is *not* a Windows bug, it is a live production panic (see 1f).**

The plan lands **five modules** (Registry, Git, Agent, Auth, GitHub — see §5), ten top-level commands, and 4 migrations. Every new crate and every new manifest entry is named in the phase that creates it; the twelve crates above collapse to five without adding a single new concern, and the moves happen inside PR 2 and PR 3 rather than in a behaviour-free structural PR nobody reviews.

---

## 1. The plan at a glance

| Phase | Outcome | Rough size |
|---|---|---|
| **1. Baseline + safe cuts** | Green clippy + green tests; 3 crates and 7 command surfaces deleted; V4 drops the `plans` table and the `repos.default_branch` column; `ro schema` replaces the hand-written `robot-docs` | ~1 PR, mostly deletions |
| **2. Identity, config, onboarding** | nested `ro.local.toml` (`[identity]`/`[auth]`/`[agent]`) + 3-tier precedence; three-mode `ro init`; `ro repos update`/`doctor`/`tag`/`untag`/`tags`; `ro repos add` remote-only + the drive-letter fix; `ro run prune --keep`; `toml_edit` `config set` | ~1100 LOC new, ~300 cut |
| **3. Git, credential, safety** | branch/stage-all/diff primitives; **per-repo author via `git -c user.name/email`**; `RunOpts` env seam with a real timeout; fixed `RepoLock`; the **keychain/env credential resolver** + `SecretString`; the extraheader push path; `AuthPolicy` in `ro-core` | ~1100 LOC new |
| **4. Engine abstraction + sweep cut** | `ro-engine` crate: `enum EngineKind` + `trait Engine` + claude/codex/git built-ins; the `Bucket` classifier and the whole `ro sweep` namespace die atomically | ~700 LOC new, ~900 cut |
| **5. Fleet verbs + surface** | `ro checkpoint` (positional repos, per-repo identity, WIP/direct), **`ro pr`**, **`ro conflict` as the resolve assistant**, run history, the `ro repos` regroup, docs, the full test suite | ~2600 LOC new, ~2500 cut |

### The five phases are the five PRs

The phases are not ten small steps — they are five PRs, each one mergeable and each leaving the tree green. §2's `1`, `1a`…`1j` are checklist items *inside* PR 1, not separate units of work. Review this as a PR series, not a phase list:

| PR | Contains | Gate before merge |
|---|---|---|
| **PR 1** — baseline + cuts | Everything in §2. Fix the Windows separator bug, the clippy failure, the `ro import` panic; delete 3 crates, toon, fork, import, self-update, the health *display* and the whole review lifecycle; `V4_DROP_PLANS` (drops `plans` and `repos.default_branch`); land `ro schema` alongside the `robot-docs` removal | `cargo test` and `cargo clippy -D warnings` green on all three OSes; V4 applied to a copy of a pre-existing `state.db` without error, and `ro pr` still works against that upgraded database |
| **PR 2** — identity, config, onboarding | nested `ro.local.toml` (`[identity]`/`[auth]`/`[agent]`) + 3-tier precedence + `deny_unknown_fields`; three-mode `ro init`; `ro repos update`/`doctor`/`tag`/`untag`/`tags`; `ro repos add` remote-only + the drive-letter fix; global-only `allow_fallback`; `toml_edit` `config set`; compat shim for `[providers]`; `ro run prune --keep`; fold `ro-jobs` into Registry and `ro-config` into Registry | unit tests for `init_mode` and `resolve_effective`; no existing user config becomes unloadable; `token = …` in a `ro.local.toml` is an **error**, not a silently-ignored key |
| **PR 3** — git, credential, identity | branch/stage-all/diff primitives; **per-repo author applied via `git -c user.name=… -c user.email=…`**; `RunOpts` env seam with a real timeout; `RepoLock` relocated and its TOCTOU fixed; the **keychain/env credential resolver** behind a `SecretString` that cannot be `Debug`-printed; the `extraheader` push path; `AuthPolicy` + `CommitIdentity` in `ro-core` | the credential test asserts the extraheader reaches git, not merely that a push was attempted; a `SecretString` appears as `***` in a captured log line; a repo with `identity.email` commits as that author while the *other* repo in the same run commits as a different one |
| **PR 4** — `ro-engine` | the new crate with `enum EngineKind` + `trait Engine` + claude/codex/git; the `Bucket` classifier and the `ro sweep` namespace die **in the same commit**; per-engine timeout and child-tree kill | `ro sweep commit-sweep` is never alive-but-hollow at any commit in the series |
| **PR 5** — fleet verbs + surface | `ro checkpoint` (positional repos, per-repo identity, WIP/direct/`off`); **`ro pr`**; **`ro conflict` as the resolve assistant**; run history; the `ro repos` regroup with aliases; docs rewrite; the full test suite | dry-run and execute both green; partial failure exits 1; protected-branch, author, and credential cases covered; a conflicted repo round-trips through `ro conflict` → manual edit → `--continue` → pushed |

**Do not merge PR 4 and PR 5.** PR 4 leaves the tool without a fleet-commit command, which is unusable but honest; merging them hides the atomic sweep cut inside a large diff and makes the riskiest deletion in the series unreviewable.

### What we are NOT doing

- **No daemon, no background sync, no cloud resume, no watch mode.** `ro sync --resume` gets deleted, not implemented. This was cut twice in review and stays cut: a background process is the single thing that most makes a CLI hard to reason about, and every run being explicit with dry-run as the default is load-bearing for the rest of the design. A `watch:` block that monitors a subset of repos is the same idea wearing a config file — it needs a lifecycle, a log destination, and an answer to what happens when it disagrees with a manual run. Revisit only if the fleet passes ~50 repos and the per-run cost is actually the bottleneck.
- **No workspace or group abstraction.** There is no `Workspace` entity, no `ro workspace`, and no `group`. Selection is a flat set of repo names, aliases, and `owner/name` positionals, plus `--all`. `repo_tags` already carries a tag, and `--tag <T>` filters on it; a "group" is a tag with a different word in front of it, and adding a second mechanism for the same selection produces two answers to "which repos does this run touch?". If groups become load-bearing later, they are a named tag — not a new table, not a new command.
- **No `ro clone`.** Cloning a remote repo and registering it is `ro repos add <REMOTE-SPEC>`, which is clone-then-register. A second verb for the same act is a second thing to document and a second thing to get wrong.
- **No plaintext token in any config file, including `ro.local.toml`.** The per-repo file holds `credential = "keychain:<name>"` or `credential = "env:<VAR>"` and never the secret. This is the one place where following an obvious-looking example would be actively harmful: a `token = "ghp_…"` key in a file ro writes and the user commits-adjacent to is a live credential in plaintext, and the research already found `AuthToken` deriving `Debug`, so it is one `tracing` field away from a log file.
- **No AI commit-message *generator* inside ro — but the engine does write the messages.** To be unambiguous, because this reads as a contradiction otherwise: `ro checkpoint` with the default `claude` or `codex` engine reads the diff and produces conventional commit messages, and that is the flagship behaviour. What is deleted is ro's *own* deterministic bucketing (`Bucket::classify` / `scope_of` / `task_id_of` and the message template), which existed to split commits **without** a model. The `git` engine is the only one that falls back to a fixed message, because it has no reasoning step. The boundary is: ro transacts and configures, the engine reasons.
- **No `ro review` lifecycle.** plan/approve/reject/apply/rollback/list-plans all go.
- **No health score in the UI.** The 0-100 number and the Excellent/Healthy/Attention/Risky/Critical classes never render — `ro status` shows the raw facts instead (dirty, ahead, behind, conflict, protected). The *computation* stays: `--filter health:<N>` remains a real selector, so a 20-repo fleet run can target only the repos that actually need attention. A score is a filter, not a display. This is cheaper than it sounds: the scorer already exists and already has tests.
- **No TOON.** Text + JSON + NDJSON.
- **No hand-maintained machine docs.** `ro robot-docs` dies in Phase 1 and `ro schema` (generated from the clap tree) lands in the **same commit** — never a four-phase absence of the surface the vision wants.
- **No `ro fork`.** It is already a stub: `fork clean` only *prints* `cleaned branch {b}` and deletes nothing, so `--dry-run` is a meaningless distinction; `fork sync` only runs `git fetch upstream` and never pulls or pushes. `FEATURES.md:205` documents `--strategy ff-only|rebase|merge` and `--push` flags that do not exist. Not "cheap" to keep.
- **No `ro import` from GitHub stars/orgs.** Bulk-loading a cloud org list is orthogonal to managing a local fleet.
- **No TUI, no interactive wizard, no engine discovery at init.** `ro init` never scans PATH for coding agents and never asks a question. With no arguments it finishes in about a second and prints two lines. (It *does* read the filesystem when you point it at a repo or a directory — that is the onboarding half, and it is opt-in by construction. See the three modes in section 3.)
- **No async runtime in the git mutation layer.** The git layer is synchronous today and stays that way — do not introduce a runtime for it. This is *not* a workspace-wide prohibition: the GitHub layer is async today, and section 5 resolves that by removing the need for it in `ro checkpoint` (shell out to `gh`).
- **No free-form plugin registry.** Exactly three engines ship, registered in a fixed three-entry table. The vision forbids a plugin registry; a config-driven one over a user-editable `[engines.*]` table is the same thing wearing a costume.
- **No *silent* auto-management.** No repo ever enters the inventory without you asking for it. `ro repos scan` is read-only and writes nothing; the only ways in are an explicit `ro init` inside a repo, `ro init --add-dir <DIR>`, or `ro repos add`. What is dropped is the idea that "ro owns everything under a projects directory" — the boundary is the inventory table, and you cross it on purpose.
- **No per-command config file in a repo dotfolder.** `ro.local.toml` sits at the repo root, single file.

---

## 2. Cut list (do first)

Order is by safety. Every item is independently revertable. Do not reorder 1h before 1g — they share the same V4 migration and the same `delete_repo_cascade` string-literal list.

### 1. Baseline repairs (no behavior change; blocks everything else)

- [ ] **Fix the Windows orphan-detection bug.** `ro-sync/src/manage.rs:286` (`resolve_local_path`) builds stored paths with `format!("{}/{}/{}", projects_dir.display(), owner, name)` — forward slashes on Windows. `ro-sync/src/prune.rs:183` walks with `PathBuf::join` — backslashes. `prune.rs:138-142` compares them as raw `String`s. On Windows **every tracked repo is reported as an orphan**. Frame the exposure accurately: `handle_orphans` with `Delete` is gated at `main.rs:803-812` (requires `--non-interactive` or a TTY) and then `confirm()` returns false on a non-TTY, so the realistic harm is a TTY user being shown a list that wrongly includes their tracked repos and typing `y` — not silent deletion. Still fix it first.
  - **Acceptance criterion, not just a regression test:** the fix has two halves and both are required. (i) `resolve_local_path` uses `PathBuf::join`, which fixes rows written *after* the change; (ii) the comparison side canonicalizes-with-fallback, which is the part that rescues every **already-persisted** forward-slash `local_path` in existing `state.db` files. Without (ii), existing users' repos become orphans the moment the fix lands. Add a mixed-separator regression test that asserts a tracked repo with a forward-slash stored path is not reported as an orphan.
- [ ] **Fix the clippy failure at `ro-sync/src/prune.rs:135`.** `if let Ok(p) = r` where only the `Ok` variant is used → `.flatten()`. It sits inside the function the prune fix touches, so it will be fixed incidentally — but it belongs in the green-baseline definition of done, not discovered at Phase 3.
- [ ] **Fix or delete `ro-github` `import::tests::fetch_without_source_errors`.** This is **not** test hygiene — it is a live production crash. `ro-github/src/import.rs:28` calls `crate::auth::build_client` (which calls `Octocrab::builder().build()`) *before* `tokio::runtime::Runtime::new()` on line 31. `Octocrab::builder().build()` panics with `there is no reactor running` outside a runtime, so **`ro import --stars` panics on any machine where a token is discoverable.** It passes on CI only because `gh` is off `PATH` there, which is exactly why it went unnoticed. Fix: move the `build_client` call inside `rt.block_on`, and move the "no import source specified" bail *before* the client is constructed. Adding `#[tokio::test]` would **not** fix it — `Runtime::new()` inside a runtime panics with "cannot start a runtime from within a runtime". Then decide separately whether the file is cut in 1f.
- [ ] **Verify green:** `cargo clippy --all-targets --all-features -- -D warnings` and `cargo test --workspace --no-fail-fast` on Windows, then confirm ubuntu + macos are also green.

### 1a. `ro-dep-update` — SAFE, 2 manifest lines

- **What:** delete `crates/ro-dep-update/` (344 LOC in one `lib.rs`, zero `#[cfg(test)]`, zero call sites: `detect_package_managers`, `check_outdated`, `update_single`, `run_tests`, `detect_test_command`, `commit_update`, `update_and_test`, `UpdateResult`, `DetectedDeps` are all unreferenced). Its `commit_update` hand-rolls an unaudited `git push origin <b>` at `lib.rs:249`.
- **Why:** nothing depends on it — not in `crates/ro/Cargo.toml`, not in any other manifest. (It does pull in `ro-core, ro-config, ro-git, ro-jobs, tokio` transitively, all of which are live elsewhere, so the graph saving is smaller than "2 lines" suggests — but the dead code is the point.)
- **Touch:** root `Cargo.toml:14` (members) and `:41` (workspace dep). Cargo.lock regenerates.
- **Verdict: SAFE-TO-DELETE. Do this one first.**

### 1b. `ro fork` — SAFE, self-contained

- **What:** `Commands::Fork` + the `ForkCommands` enum + the ~79-line handler arm in `main.rs` (lines 1391-1469).
- **Also delete by hand** (`pub` in a lib crate, so no `dead_code` warning fires): `ro_git::read::merged_branches` (`read.rs:145`) and `ro_git::mutation::fetch_remote` (`mutation.rs:208`).
- **Exception — do NOT delete `ro_git::read::has_remote` (`read.rs:124`).** It is exactly the primitive `ro checkpoint` needs for the "no GitHub remote → local commit only" rule. It happens to be called only from the fork path today, but it survives into Phase 3.
- **Verdict: SAFE-TO-DELETE.**

### 1c. `ro self-update` — SAFE, ~25 LOC

- **What:** `Commands::SelfUpdate` + handler (`main.rs:1364-1383`).
- **Why:** it does not self-update. Without `--check` it echoes a curl/irm install line; with `--check` it prints `CARGO_PKG_VERSION` and a releases URL without checking anything. `FEATURES.md:232-234` claims it "Replace[s] the installed binary".
- **Verdict: SAFE-TO-DELETE.** The install instructions it prints belong in README, where they already partly live.

### 1d. `ro robot-docs` → `ro schema` — SAFE, and the replacement lands **here**

- **What:** `Commands::RobotDocs`, `fn generate_robot_docs` (`main.rs:487-551`, 65 lines of `json!` literals), the dispatch arm.
- **Why it goes:** it has already drifted from the clap tree in four places — `commands` omits `commit-sweep` from the sweep subcommands; `prune` omits `--orphans/--archive/--delete`; `formats` lists only text/json (contradicting FEATURES.md's toon section); `quickstart` still recommends `ro health`. `FEATURES.md:236-239` promises an `all` topic that does not exist.
- **Replacement, same PR:** `ro schema`, built from `clap::CommandFactory` (already a dependency) so it cannot drift again. ~30 lines.
- **Compatibility break, state it in the release notes:** `ro schema` is a live clap-tree dump — a genuinely different thing from a hand-written JSON summary. Any agent parsing `ro robot-docs commands` breaks.
- **Verdict: SAFE-TO-DELETE, with the replacement atomic.**

### 1e. TOON + the whole `ro-output` crate — SAFE, and the plan deletes the crate, not just the format

- **What:** `OutputFormat::Toon` (`main.rs:26`), `fn print_toon` (`main.rs:30-35`), 6 dispatch arms (`main.rs:663, 758, 790, 896, 1322, 1419-1425`); `ro_output::text::write_toon` + `uniform_object_keys` (text.rs:29-72) and 4 tests; the `Toon` arms in `ro-output/src/lib.rs` (`OutputFormat`, `Display`, `parse_output_format`, `render`).
- **Why the whole crate goes:** `ro_output::{OutputFormat, parse_output_format, render}` have **zero** external callers — `write_value` is reached only from inside the dead `render()` at `ro-output/src/lib.rs:68`. `json::{write_pretty, write_compact}`, all of `format::*`, `text::header`, and `NdjsonWriter` are the same. Cutting toon orphans the entire `ro_output::text` module, and what remains is a crate the binary barely uses.
- **What moves, and where:** the **8 live `NdjsonEvent` constructors** are called at `main.rs:1087, 1099, 1107, 1124, 1130, 1116, 1145, 1152` inside the `sweep agent` handler, and `text::write_toon` at `main.rs:32`. Under the new architecture the orchestrator lives in the `ro` binary (see section 5), so both move into `crates/ro/src/ndjson.rs` (the `NdjsonEvent { kind, ts, #[serde(flatten)] payload }` envelope, kept verbatim) and `crates/ro/src/render.rs`. `ro-output` is deleted from `[workspace] members`.
- **Test accounting:** `ro-output` has 25 tests. TOON kills 4, the dead `OutputFormat`/`parse_output_format`/`render` kill 5 more in `lib.rs`, `json.rs` kills 2, `format.rs` kills 6 — **~17 die, ~5 survive** (the `ndjson.rs` ones). Do not let "only 2 symbols are called" understate the live NDJSON surface: it is 9 call sites.
- **Text rendering, stated explicitly so nobody hunts for a renderer that does not exist:** text output is **hand-rolled `println!` per command handler**, which is what happens today. `--format text` stays the default on `ro repos list`, `ro repos scan`, `ro status`, `ro sync`, and `ro checkpoint`; `json` and `ndjson` are the machine surfaces. There is no shared text renderer and the plan does not build one.
- **Verdict: SAFE-TO-DELETE.**

### 1f. `ro import` + `ro-github/src/import.rs` — SAFE, ~300 LOC

- **What:** `Commands::Import` + handler (`main.rs:669-718`), `ro_sync::manage::import`, `ro_github::import::fetch_import_specs` (its single call site is `main.rs:693`).
- **Why:** off-vision. Bulk-loading stars/orgs is orthogonal to a local fleet.
- **Side effect:** removes the panicking test fixed in 1. Does **not** make `ro-github` dead: `crates/ro/src/doctor.rs:25` imports `ro_github::auth::discover_token`, called at `doctor.rs:239` in `check_github_auth`. **The `ro-github` dependency MUST STAY in `crates/ro/Cargo.toml` after this cut.** The crate is no longer "only live because of import" — it is live because of doctor.
- **Verdict: SAFE-TO-DELETE.**

### 1g + 1h. The health *display* and `ro review` — ONE PR, one V4 migration

**Only `ro review` drops a table.** The health scorer survives — see 1g below. The two cuts still land together because both are referenced by the same `delete_repo_cascade` literal lists, and the `plans` table is the only one going.

- [ ] **Health UI only — keep the computation.** Delete `Commands::Health`, its handler arm, the `HealthClass` display enum, and the health entries in the (now-deleted) robot-docs literals. **Keep `crates/ro-state/src/queries.rs` in full** (275 LOC, 7 tests: `HealthClass`, `HealthSnapshot`, `score_repo_health`, `latest_health`, `score_all_health`, `row_to_health`), **keep `pub mod queries;`** in `ro-state/src/lib.rs`, and **keep the `health:<N>` branch in `resolve_multi_repo_targets`** so `--filter health:<N>` keeps working on `ro checkpoint`. The score is a selector, not a display: nothing renders it, but `ro checkpoint --filter health:critical` still narrows a 20-repo fleet to the repos that need a human. `HealthClass` keeps its `Debug`/`Serialize` derives because the filter now consumes it; delete only the `Display` impl if it exists to feed the removed table output.
- [ ] **Review:** delete `crates/ro-review/` (541 LOC, 13 tests), `Commands::Review`, the `ReviewCommands` enum, `fn parse_risk`, its handler arm, `AutoApproveLevel`, `ReviewConfig`, the matching `validate.rs:50-61` review blocks, the `[review]` block in `DEFAULT_CONFIG_TOML`, and the `paths.rs` test assertion on `review`.
- [ ] **Good news:** `ro-review::apply_plan` (`apply.rs:22-45`) only runs `UPDATE plans SET status='applied'` — `ro review apply` never touched a repo. Deletion carries zero behavioral or data-loss risk.
- [ ] **Do NOT confuse** `ro_sweep::agent::SweepSummary.plan_created: bool` (in-memory, live, `main.rs:1113`) and `ro_output::ndjson::NdjsonEvent::plan_created` with the `plans` table. They are unrelated to ro-review.
- [ ] **Append `V4_DROP_PLANS` in `ro-state/src/migrate.rs`** with exactly two statements:
  ```sql
  DROP TABLE IF EXISTS plans;
  ALTER TABLE repos DROP COLUMN default_branch;
  ```
  **`repo_health_snapshots` is NOT dropped** — the health scorer survives as the `--filter health:` selector — so the migration does not touch the health FK chain at all. **Do NOT edit `V1_INITIAL_SCHEMA`** — `migrate::run` records applied versions in `_meta.version` and only applies migrations with `version > current`, so a V1 edit never reaches an already-initialized DB. Update the hard-coded table list in `all_tables_exist` and the index list in `all_indexes_exist`, and drop **only the `plans` assertions** from the `remove_clears_dependent_rows` test at `manage.rs:437` — the `repo_health_snapshots` assertions and the `score_repo_health` calls at `ro-sync/prune.rs:369,381` all stay live. The `ALTER TABLE` is SQLite 3.35+ (2021); the workspace pins `rusqlite` with `bundled`, so check the bundled version rather than assuming.
- [ ] **`default_branch` has readers that must be rewritten, not just a column to drop.** `ro pr --base` and the empty-repo base inference both read it today; both move to `git symbolic-ref` and `gh repo view --json defaultBranchRef` (see §3). `ro repos update --default-branch` is deleted with the column. Grep for the name before running the migration — a `SELECT` left behind is a runtime failure on an upgraded database, and the migration itself will succeed.
- [ ] **⚠ Fix `ro-sync/src/manage.rs` in the same commit — one entry, not two.** `NULLABLE_FK_TABLES` contains `"plans"` as a **raw string literal**; remove it. `CHILD_TABLES` contains `"repo_health_snapshots"` and that entry **stays**. Getting this backwards is the single highest-risk item in the migration: dropping a table without editing this file leaves `cargo check`, `cargo clippy`, **and `cargo test` green** and then breaks `ro remove` / `ro repos prune` at runtime with a SQLite no-such-table error. Invisible to every static check.
- **Verdict: DELETE-WITH-FOLLOWUP, one PR.**

### 1i. Already-dead helpers, lib.rs edits, and unused dependencies

**Every module deletion below requires its `lib.rs` edited in the same commit.** Each is an instant E0583/E0432 otherwise. This is the largest cluster of unlisted mechanical edits in Phase 1:

| File | Edit |
|---|---|
| `crates/ro-sweep/src/lib.rs` | drop `pub mod policy_check`, `pub mod risk`, and `pub use risk::{RiskLevel, RiskReason, classify, to_json}` |
| `crates/ro-config/src/lib.rs` | drop `pub mod policy` and `pub use policy::{FileViolation, Policy, PolicyReport, check_policy}` |
| `crates/ro-core/src/lib.rs` | drop `pub mod redaction` |
| `crates/ro-git/src/lib.rs` | drop `GitErrorKind` from the `pub use mutation::{…}` list |

Then delete: `ro_output::*` (whole crate, 1e); `ro_git::mutation::reset_hard` (destructive, 0 callers, no rollback requirement in the vision); `ro_git::read::{discover, status}` (0 callers; `ro-sync/status.rs` re-derives status inline); `ro_git::mutation::GitErrorKind` + `classify` (0 callers — and it is **not** collapsible later, because once this deletion lands there is nothing left to collapse with `ro-jobs::failure::classify`; the two taxonomies get unified in Phase 3 by keeping `ro-jobs` as the survivor); the `AheadBehind::ZERO`-on-any-error conflation in `read.rs:111-112` (it returns "up to date" for a typo'd upstream); `ro_config::policy` + `ConfigPaths::policies_yaml()` (`Policy::from_file` has 0 production callers); `ro-sweep/src/{risk, policy_check}.rs` (0 CLI callers).

**Six ro-config test sites break and are not optional** (each is a hard compile/test failure, not a cosmetic update): `schema.rs` test `defaults_match_plan_example` (13 assertions on `jobs`/`mcp`/`review`/`safety`); `schema.rs::round_trip_through_toml`; `loader.rs::round_trip_load_then_load` (asserts `cfg.jobs.max_attempts == 3`); `validate.rs` production rules reading `cfg.jobs.*` and `cfg.safety.max_auto_apply_risk`; `validate.rs::invalid_layout_rejected` (writes `cfg.core.layout`); and `paths.rs::default_config_parses_as_toml`.

**Manifest cleanup, including the ones that break workspace resolution if missed:**

- `crates/ro-output/Cargo.toml` declares `ro-core`, `comfy-table`, `owo-colors`, and `console` — **zero references to any of them** in the crate's sources. Removing the corresponding `[workspace.dependencies]` entries without editing this manifest is an immediate workspace-resolution error.
- `crates/ro-sweep/Cargo.toml` declares `tempfile.workspace = true` in **both** `[dependencies]` (line 26) and `[dev-dependencies]` (line 33). Fix the duplicate.
- After `policy_check.rs` goes, `ro-config` and `serde_yaml` also become dead in `ro-sweep/Cargo.toml`. Neither was on the original list.
- Unused workspace deps: `comfy_table`, `console`, `sha2`, `owo_colors`. Unused per-crate deps: `ro-git{tokio, serde_json, ro-core, regex}`, `ro-sweep{ro-core, ro-jobs, ro-output, tokio, comfy-table, ro-config, serde_yaml}`, `ro-sync{tokio, indicatif, console}`, `ro-github{reqwest, time}`, and `ro-config`'s unused `ro-core`. Drop `gix`'s `max-performance` feature (3 gix calls total).
- Consolidate the three redaction implementations onto one: `ro_core::redaction::redact_secret` (0 callers, and it byte-slices `&secret[..visible]` which panics on a multi-byte boundary), `ro_github::auth::AuthToken::redact` (first-4/last-4), `ro_sweep::secret_scan::redact` (first-4, correct). Pick one and delete the other two. The identity guard in section 5 needs a single rule.
- The broken `xtask` alias in `.cargo/config.toml` — there is no `crates/xtask`; the alias errors if run.
- **`deny.toml` is stale.** Its `[advisories] ignore` list references `RUSTSEC-2024-0436` (via `rmcp`, a crate deleted in commit `5fdece9`) and `RUSTSEC-2025-0119` (via `indicatif`, which this phase removes from `ro-sync`). `.github/workflows/audit.yml` mirrors the same three ignores. Stale ignores are at best noise and at worst a hard failure depending on cargo-deny version. Clean both.
- **Correct a wrong comment while you are in the file:** `crates/ro/Cargo.toml:39` says "tempfile is used by doctor tests in src/ (behind `#[cfg(test)]`)". That is **wrong** — `doctor.rs:187` calls `tempfile::tempdir()` in **production** code inside `run()` (the `#[cfg(test)] mod tests` starts at line 443), in the `ConfigPaths::discover()` fallback. `tempfile` is a genuine runtime dependency and must **not** be moved to `[dev-dependencies]`.
- **Stale doc strings in files this phase touches:** `ro-output/src/lib.rs:11` says TOON is "retained for `ru` compatibility" and the crate description lists all four renderers.

### 1j. Held back for Phase 4 / Phase 5 — do NOT cut yet

- **`ro sweep commit-sweep`** is the only working fleet-commit path. It stays until `ro checkpoint` ships. **But** the `Bucket` classifier deletion moves *into the same commit* that cuts the `sweep` namespace (Phase 4, see AA) — never leave the flagship command compiled-but-gutted.
- **`ro sync --parallel/-j`, `--resume`, `--timeout`:** delete the *flags* in Phase 5. Do not delete them now — the real bounded-executor work needs the fleet loop from Phase 5. Meanwhile **stop advertising them** in README/FEATURES: a silently-accepted no-op flag is worse than an error. Note `--timeout` is worse than advertised — `SyncOptions.timeout_secs` is set at `main.rs:739` and **never read anywhere in `ro-sync/src/sync.rs`** (only the struct field at `:40` and the `Default` impl at `:54`). It is a no-op flag today, and it is **not** a config-precedence instance: `timeout.unwrap_or(30)` is a hardcoded literal that never consults `core.timeout_secs`.
- **`--quiet` and `--verbose`:** delete **both** in Phase 5. `--verbose` is never read anywhere. `--quiet` is a dead binding at `main.rs:623` (`let _quiet = cli.quiet;`) with exactly one real read at `main.rs:704` — inside the `ro import` branch that 1f deletes, so it has zero readers afterwards. This resolves the contradiction between the section-3 command tree and the cut list: the target tree below carries neither flag.

---

## 3. Renames & command surface

| Before | After | Kind |
|---|---|---|
| `ro sweep commit-sweep` | `ro checkpoint` | **Promote + reshape** (not a rename) |
| `ro sweep commit --path P --message M` | `ro checkpoint NAME --message MSG` | Fold into checkpoint |
| `ro sweep agent --output json` | `ro checkpoint --format ndjson` | Move + unify flag namespace |
| `ro add <SPEC>` | `ro repos add <REMOTE-SPEC>` | **Regroup** (alias kept one release) + **narrowed**: remote spec only, no local paths |
| `ro remove <KEY>` | `ro repos remove <KEY>` | **Regroup** (alias kept one release) |
| `ro list` | `ro repos list` | **Regroup** (alias kept one release) |
| `ro prune` | `ro repos prune` | **Regroup** (alias kept one release) |
| (new) | `ro repos scan <DIR>` | New, read-only |
| (new) | `ro repos tag <REPO> <TAG>` | New, needed by `--tag` |
| `ro robot-docs <TOPIC>` | `ro schema` | **Replace** with a clap-derived JSON surface (Phase 1, same PR) |
| `ro health [REPO]` | (command gone) — `ro status` already shows the raw facts. `--filter health:<N>` survives as a selector | **Delete UI, keep compute** |
| `ro review *` | (gone) | Delete |
| `ro fork *`, `ro import`, `ro self-update` | (gone) | Delete |
| `ro sweep *` | (gone) | Delete in Phase 4, atomically with the classifier |
| `ro sync --resume` | (gone) | Delete (no daemon to resume into) |
| `ro --config-dir D` alone | actually honoured | **Bug fix** |
| exit `0/1/64` (claimed) / `2` (clap) / `3` (prune) | `0/1/2` fleet outcomes, `64` usage, distinct fatal code | Reconcile |

### Target command tree

```
ro [<GLOBAL FLAGS>] [<REPO>…]            # bare = checkpoint the cwd repo, dry-run
ro [<GLOBAL FLAGS>] <COMMAND> …

  init        # three modes, disambiguated by argv + cwd — see below
             [--add-dir <PATH>] [--add-repo <PATH>] [--engine <NAME>]
             [--auth gh|git] [--non-interactive] [--force]
  repos       <SUBCOMMAND>
               add <REMOTE-SPEC>    # clone + register. REMOTE SPEC ONLY — no local paths
               remove <KEY>
               list     [--owner <OWNER>] [--format text|json]
               scan <DIR> [--depth <N>] [--format text|json]   # read-only, adds nothing
               update   <KEY> [--name <NEW>] [--owner <NEW>] [--alias <NEW>]
                       [--branch <NAME>] [--archive | --unarchive]
               prune    [--archived] [--missing] [--orphans] [--archive | --delete]
               doctor   [--format text|json]                   # read-only inventory audit
               tag      <REPO> <TAG>...
               untag    <REPO> <TAG>...
               tags     [<REPO>] [--format text|json]
  status      [<REPO>…] [--format text|json]
  sync        [<REPO>…] [--strategy ff-only|rebase|merge] [--format text|json]
             [--dry-run] [--clone-only] [--pull-only] [--autostash]
             [-j <N>] [--timeout <SECS>]                    # PULL: reconcile with remote
  checkpoint  [<REPO>…] (flag table lives with the command itself)
             # PUSH: agent commits, ro pushes. direct | wip | off
  pr          [<REPO>] [--base <BRANCH>] [--title <T>] [--body <B>]
             [--draft | --ready] [--format text|json]
  conflict    list [--format text|json]                     # every repo mid-merge/rebase
             <REPO> [--strategy rebase|merge]               # fetch + integrate + resolve
             <REPO> --continue                              # finish a manual resolution
             explain <REPO> | abort <REPO>
  run         list [--limit <N>] | show <RUN_ID> | timeline <RUN_ID>
             prune --keep <N>                               # retention, never automatic
  doctor      [--fix] [--format text|json]
  config      [print | set <KEY=VALUE>]
  schema                                    # generated from the clap tree
```

Ten top-level commands, plus a bare form. `--quiet` and `--verbose` are gone (1j); do not re-add them to this tree.

**Bare `ro` is `ro checkpoint` on the current repo — and it is safe because it is dry-run.** The single most common action in the tool's whole life is "I'm in a repo with uncommitted work, what would you do with it", and that should not require remembering a subcommand. `ro` with no subcommand resolves cwd to one repo, resolves that repo's `ro.local.toml`, and prints the checkpoint plan. **It does not commit, does not push, and does not spawn an agent**, because `checkpoint`'s default is `--dry-run` and the bare form inherits that default rather than overriding it. `ro` with positionals and no subcommand is the same thing over a named set: `ro cass backend` previews both.

This is the one place where a magic default is worth it, and the reason is that the default it picks is read-only. A bare `ro` that committed would be indefensible; a bare `ro` that tells you what it would do is the whole tool in one word.

**One more case, and it is the one that removes the last reason to `cd`: bare `ro` outside any repo previews the whole managed inventory.**

```
ro                    # inside a git repo  -> that one repo, dry-run
ro                    # not in a repo      -> every MANAGED repo, dry-run
ro cass backend       # named set          -> those, dry-run
```

That third-to-second transition is the whole multi-repo use case in one word, and it is scoped to the **inventory** — a set you put there deliberately with `ro init` or `ro repos add`. It is not a filesystem scan. The difference is the entire safety argument:

| | inventory (what this does) | scanning parent folders (what was proposed) |
|---|---|---|
| Which repos | the ones you registered | every `.git` it can reach |
| A repo you forgot about | invisible, safe | silently committed and pushed |
| A clone someone left in `~/tmp` | invisible | pushed with your identity |
| New repo you just made | not managed until you say so | picked up on the next run |
| What "managed" means | a row you added | filesystem layout |

Auto-discovery means the set of repos ro will **push to with your credential** is decided by where you happen to be standing. A directory scan that reaches one repo too far is not a bug report, it is a disclosure. The inventory is a slower way to build the same list, and the speed-up is the exact thing that is dangerous here.

`--all` remains for scripts, and it means the same thing the bare form outside a repo means: the managed set. Nothing in the tool ever guesses.

**Repo selection is positional, everywhere — and cwd is the no-argument case.** `ro checkpoint cass voice-ai-agent`, `ro sync cass`, `ro status backend`, and bare `ro` inside a repo. Not `--repo NAME`: it is what people actually type and it reads like a git command. `--all` remains the explicit "every managed repo" escape for scripts, because a bare `ro checkpoint --all` on a 20-repo fleet is a decision you want to be visible about. Accept a bare name, an alias, or `owner/name` in one positional and resolve it the same way in all three verbs — one resolver, one set of errors.

Note what this is **not**: `cd repo-a && ro` in a loop. The positional form exists so you can stay in one directory and name the fleet. An earlier proposal to make the loop the interface — `cd a && ro ship; cd b && ro ship` — is precisely the workflow this tool was built to remove, and it is the reason the positional form and the inventory exist at all.

**`ro clone` does not exist.** Cloning a remote repo and registering it is `ro repos add <REMOTE-SPEC>`, which is clone-then-register. A second verb for the same act is a second thing to document and a second thing to get wrong. If you want it to read better at the top level, the honest form is an alias, not a subcommand.

### The three `ro init` modes — one verb, disambiguated by argv and cwd

`ro init` is the onboarding verb. It is dumb about *engines* and competent about *repos*, and the two never mix: it never scans PATH for `claude`/`codex`, never asks a question, never validates a binary. Availability is still checked lazily at `ro checkpoint` time.

| Invocation | What it does | Cost |
|---|---|---|
| `ro init` (no args, not in a git repo) | **Global bootstrap.** Write `~/.config/ro/config.toml` with the hardcoded `engine = "claude"` + `auth = "gh"`, create the config dir, print the path and the one-line override hint. Nothing else. | ~1s |
| `ro init` (no args, **inside** a git repo) | **Onboard that repo.** The global bootstrap, plus: write `ro.local.toml` at the repo root, append it to `.gitignore` idempotently, and insert the repo into the inventory. This is the common case — `cd my-repo && ro init`. | ~1s |
| `ro init --add-dir <DIR>` | **Onboard a workspace.** Scan `<DIR>` for git repos, print what it found, and register each one **with an explicit summary and a confirmation in a TTY** (`--non-interactive` registers all and skips the prompt). Each registered repo gets its `ro.local.toml` + `.gitignore` entry. | seconds, scales with repo count |
| `ro init --add-repo <PATH>` | **Onboard one specific repo** without scanning a parent. Equivalent to `cd <PATH> && ro init`. | ~1s |

Two rules keep this from becoming the magic the plan rejected:

- **Mode selection is by argv and cwd, never a guess.** No args + `.git` in cwd → onboard that repo. No args + no `.git` → global only. `--add-dir` is always explicit. There is no recursive walk of the current directory, ever — that is what `ro init --add-dir .` is for, and the user types the dot.
- **`--add-dir` is still opt-in.** It is a flag, not a default. `ro repos scan` remains read-only and writes nothing. The point is that you can onboard 20 repos in one command *because you asked for 20 repos*, not because ro decided your projects directory is a fleet.

### `ro init` vs `ro repos add` — the split, and why it matters

Today these overlap and neither is right. `ro repos add` records a repo and remembers where it "will live at" without cloning; the actual clone happens later in `ro sync --clone-only`. The result is a state you cannot get out of: an inventory row pointing at a directory that does not exist. That is the "tracked but not cloned" row the checkpoint edge-case table in section 6 has to handle at all.

| Situation | Command | Result |
|---|---|---|
| The repo is already on disk, and I am standing in it | `ro init` | onboard: `ro.local.toml` + `.gitignore` + inventory row, working copy untouched |
| I want a repo I do not have yet | `ro repos add github.com/owner/repo` | **clone and register** — a working copy exists when the command returns, or it failed and there is no inventory row |

`ro repos add` becomes clone-then-register, and it is **all-or-nothing per repo**: on clone failure it writes no row. That deletes the "tracked but not cloned" state from the data model instead of teaching the rest of the system to tolerate it. `ro sync --clone-only` stays for the repos that were added by older versions or by `ro import`-era tooling, but nothing new produces that state.

### `ro repos update` — the missing CRUD verb

`repos` has add, remove, list, scan, prune, tag and **no way to change a row**. Today, renaming a repo or flipping `archived` means `ro remove` followed by a re-`add` — and `remove` cascades through `CHILD_TABLES`/`NULLABLE_FK_TABLES`, so the cycle really does drop that repo's `repo_tags` rows and its `repo_health_snapshots` history. For a fleet tool whose whole point is a stable inventory, that is the wrong shape.

`ro repos update <KEY>` is a targeted, idempotent column edit against the existing `repos` table. It never touches a working copy and never renames a directory — it changes what the inventory *believes*, and reconciling disk stays the user's job.

**The key is `(host, owner, name)`, not `(owner, name)`** — that is the table's `UNIQUE` constraint (`migrate.rs:81`). Parse `<KEY>` as the existing `owner/name` form with an optional `host/` prefix defaulting to `github.com`, so the common case stays short.

The columns that exist today (`migrate.rs:66-82`) and are worth exposing:

- `--name <NEW>` / `--owner <NEW>` — change the inventory identity. Refuses if the new `(host, owner, name)` already exists: that is a merge, not an update.
- `--alias <NEW>` — the `alias` column exists and has no writer anywhere in the workspace. A short human label (`backend`, `oss`, `work`) is what `--tag` wants to be and is cheaper to type.
- `--default-branch <NAME>` — **removed.** The `repos.default_branch` column is dropped in V4; `ro pr --base` resolves from `git symbolic-ref` and `gh repo view` instead. Correcting a branch name by hand was only ever patching a cache of a value two live sources already answer.
- `--branch <NAME>` — the tracked `branch TEXT` column, currently the checkout ro assumes. Correcting it by hand is the escape hatch when the inventory and disk disagree. This one stays, because it is the branch *ro* is tracking locally, not a fact about the remote.
- `--archive` / `--unarchive` — toggle `archived`, which exists and is **never filtered** today (see the fleet-correctness note below).

**No `--remote` flag, and this is deliberate.** There is no remote column and no remotes table — the full table list is `repos, runs, run_events, sync_results, jobs, job_events, plans, failures, repo_health_snapshots, context_cache, audit_log, repo_tags`. Remotes are read live from the working copy through git, and mirroring them into the inventory would create a second source of truth that drifts the moment anyone runs `git remote set-url`. Fixing a remote is `git remote set-url` in the repo, which is one command ro does not need to wrap. Adding a remotes table in v1 would be the same over-engineering this plan exists to remove.

Everything not named is untouched. No `--force`, no bulk mode, no dry-run in v1: a single-row edit is cheap to undo by re-running with the old value, and the surface stays scriptable without a confirmation prompt. `ro repos update --archive`, combined with the `archived = 0` filter, is also the *supported* way to park a repo — which the plan otherwise has no mechanism for.

### `ro repos add` is remote-only, and must reject a local path out loud

Two verbs, two meanings, no overlap: `ro init` registers a repo **you already have, on disk**; `ro repos add` fetches a repo **you do not have**. The second is a network act, so its argument has to be a remote spec and nothing else. A user who types `ro repos add .` or `ro repos add C:\work\backend` must get an error that names the right command, not a row in the inventory.

`RepoSpec::parse` already rejects most local paths by accident — it requires an `owner/name` split on `/`, so `.` and `C:\work\backend` fail with "expected owner/repo". **But the Windows forward-slash form parses as a valid spec and that is a real bug:**

```
ro repos add C:/work/backend
  -> from_host_owner_name("github.com", "C:/work/backend", …)   repo_spec.rs:107
  -> splitn('/', 2) gives ["C:", "work/backend"]
  -> owner = "C:", name = "work/backend"
  -> clone_url = "https://github.com/C:/work/backend.git"
```

A row is inserted for an owner that does not exist, and the failure surfaces much later as a clone error against a nonsense URL. Fix it in `RepoSpec` rather than in the command handler, so every caller inherits it: reject a spec whose `owner` matches `^[A-Za-z]:$` (a Windows drive letter) and reject any spec containing a backslash. Add both as `RepoSpec` test cases — they are two lines and they are the difference between a clear error and a corrupt row.

`ro init` is the only verb that accepts a filesystem path, and it takes the path implicitly from cwd or from `--add-dir`/`--add-repo`. A user reaching for `ro repos add <path>` has the wrong command; the error message says so.

### `ro repos doctor` — read-only inventory audit

The inventory drifts. A folder gets renamed, a disk gets swapped, a clone gets deleted, the `origin` URL stops matching `clone_url`. Today every consumer discovers this independently and inconsistently: `status_repo` has a `<local_path>/.git` guard, the checkpoint edge-case table has a `NotCloned` reason, `ro repos prune --missing` can find them, and the bare-orphan half of `prune` does a third thing. Three partial answers to one question.

`ro repos doctor` is that question, asked once, changing nothing:

```
✓ backend                       present, clean
✗ dashboard (missing)           no directory at the recorded local_path
⚠ worker (remote mismatch)      origin is git@github.com:acme/worker, inventory says acme/worker.git
⚠ gateway (not a repo)          path exists but has no .git
○ legacy-api (not cloned)       recorded, never cloned — run: ro repos add acme/legacy-api
```

Four checks, all of which already exist as ad-hoc logic somewhere: **missing** (no directory), **not a repo** (directory but no `.git`), **remote mismatch** (live `origin` vs the recorded `clone_url`), **not cloned** (the legacy state from the init/add split). Read-only, always. No `--fix`: every remedy is destructive or opinionated — a missing directory means `ro repos remove`, a mismatch means the user should look at it — and a tool that guesses which of those you meant is a tool that deletes something.

This is deliberately **not** folded into `ro status`. `ro status` answers "what state is each repo in right now" and runs on every fleet operation; `ro repos doctor` answers "is my inventory still true", is a maintenance command you run when something looks wrong, and is the one you paste into an issue.

### `untag` and `tags` — the tag table needs to be able to shrink

The `repo_tags` table (`migrate.rs:212-218`) is `PRIMARY KEY (repo_id, tag)` with `ON DELETE CASCADE` and an index on `tag`. With only `tag`, the table can grow forever: the only way to remove a tag is to edit SQLite by hand, which is precisely the failure mode that makes people stop using a tool.

Three verbs complete the CRUD and all three are one statement each:

| Verb | SQL |
|---|---|
| `ro repos tag <REPO> <TAG>…` | `INSERT OR IGNORE INTO repo_tags (repo_id, tag) VALUES (?, ?)` |
| `ro repos untag <REPO> <TAG>…` | `DELETE FROM repo_tags WHERE repo_id = ? AND tag = ?` |
| `ro repos tags [<REPO>]` | `SELECT tag FROM repo_tags WHERE repo_id = ? ORDER BY tag` |

Both writers take **multiple** tags — `ro repos tag a backend oss work` is the shape people actually type, and looping in the handler is three lines. `tag` uses `INSERT OR IGNORE` so tagging twice is a no-op, not an error. `untag` on a tag that is not there is also a no-op with exit 0, because "make it so" is idempotent and an error would make shell loops awkward. `idx_repo_tags_tag` already makes `--tag <T>` selection indexed, so no schema work is needed.

This is the same hole the `update` verb fills for `repos`. Audit every writer in the plan for its missing remover; `tag`/`untag` is the only one.

### `ro run prune --keep <N>` — retention, never automatic

`runs`, `run_events`, `sync_results`, and `failures` all grow without bound, and a 20-repo fleet that checkpoints on every panic generates runs faster than anyone expects. There is no retention today and no plan for one, which means the first person to notice does it by deleting `state.db` and loses the inventory with it.

`ro run prune --keep <N>` keeps the **N most recent runs** and deletes their events, results, and failures by `run_id` cascade. Explicit, never automatic: no time-based expiry, no size cap, no "clean up on startup". An automatic policy would eventually delete the run that explains a bad push, and the failure mode is silent and discovered too late.

`--keep` is required. There is no default, because "prune to what?" has no safe answer and a default is how data disappears. Deleting a run also has to be announced in the output — `--format json` reports `{deleted: N, oldest_kept: "<run-id>"}` — so a script that calls it can log what it destroyed. One `DELETE … WHERE run_id IN (SELECT id FROM runs ORDER BY started_at DESC LIMIT -1 OFFSET ?)` over the four tables, with the run count printed before and after.

Note the interaction with the V4 migration: `prune` here deletes **rows**, never tables, and is unrelated to `V4_DROP_PLANS`.

### `ro pr` — push a branch and open the PR, without committing

The plan originally had no standalone PR verb: creating a PR was `ro checkpoint --wip`, which commits, branches, pushes, and opens the PR as one transaction. That is the right shape for *"I have WIP and want it somewhere safe."* It is the wrong shape for *"my branch is already committed and pushed, I just need the PR"* — which is most PRs, most days. Forcing those through `checkpoint` means either committing again for no reason or inventing a flag combination whose only job is to disable three of the four steps.

`ro pr [<REPO>]` does exactly two things: ensure the current branch is on the remote, then `gh pr create`. It does **not** commit, does not create a branch, and does not switch branches. If the tree is dirty it says so and stops — a PR is a statement about a specific commit, and silently including uncommitted work in a branch you did not just push is how a WIP file ends up in a reviewed PR.

**`--base` is resolved from git and GitHub, never from config or a database cache.** An earlier draft preferred the inventory's `default_branch` column, then a second revision kept it as a fallback. Both are gone: the column duplicated a fact two live sources already answer authoritatively, and a cache of a branch name is a field that goes stale on a rename.

```
1. --base, if given
2. `git symbolic-ref --short refs/remotes/origin/HEAD`  ->  origin/main
   (for a non-origin remote, try the remote actually configured in the row)
3. `gh repo view --json defaultBranchRef -q .defaultBranchRef.name`
4. error, naming all three
```

Two live sources, one cache, no fallback. A repo renamed `main` → `trunk` upstream is caught by step 2 or step 3 without the user editing anything. Steps 2 and 3 fail together only when there is no remote or `gh` is unavailable — in which case the honest answer is to ask rather than guess, and `--base` is right there in the error.

The `repos.default_branch` column is **dropped in V4** alongside `plans`, and `ro repos update --default-branch` goes with it. Nothing reads the column after this change, and leaving an unread column is exactly the dead-schema problem this plan has been cutting everywhere else.

`--title` and `--body` default to nothing, which is correct: `gh pr create` derives a title from the commits and ro has no opinion. `--draft` and `--ready` are the same thing at different times; `--draft` is the default because a PR opened seconds after a push is almost never ready for review.

**Output is a result, not a browser launch.** Print the number, the title, and the URL, and stop:

```
✓ PR created
#142  feat: retry extraction
     https://github.com/quangdang46/cass/pull/142
```

No browser is opened. That is partly a portability decision — `xdg-open`/`open`/`start` are three different calls with three different failure modes, and a CLI that hangs waiting on a browser is a CLI someone runs in a script — and partly the simpler one: the URL on screen is clickable in every terminal worth using.

Duplicate handling is the one behaviour worth spelling out, and it matches `checkpoint --wip`: **if an open PR already exists for this head, update it and print the existing URL rather than opening a second one.** A second PR for the same branch is the failure everyone hits when they re-run a script.

The branch naming, the push path, and the `gh pr create` call are shared with `checkpoint --wip`. If they are not shared, the two will drift and one of them will eventually push a branch the other cannot find.

### `ro conflict` is the resolve assistant, not just a detector

The plan's original `ro conflict` was `list | explain | abort | mark-resolved` — detection, explanation, and bailing out. That is half a feature: knowing a repo is wedged without help getting it unwedged just moves the work back to the terminal. The verb stays `conflict` (not a new `ro resolve`); the subcommands are the question and the action.

```
ro conflict list                          every repo mid-merge or mid-rebase
ro conflict <REPO>                        fetch, integrate, help finish
ro conflict <REPO> --continue             finish after a manual resolution
ro conflict explain <REPO>                what state is it in and why
ro conflict abort <REPO>                  bail out, restoring the pre-operation ref
```

`ro conflict <REPO>` is the flow, and it is the one place where opening a tool for the user is the right default:

```
fetch origin
   ↓
rebase onto <base>            (--strategy merge for the other mode)
   ↓
clean?  ── yes ──→ continue, push, done
   │
   no
   ↓
show the conflicted paths, grouped by conflict type
offer: open mergetool  |  list the files  |  abort
   ↓
(user resolves in their own editor)
   ↓
ro conflict <REPO> --continue
   → stage what they resolved, run rebase --continue, push if the ref moved
```

Two rules keep this from becoming a merge tool that half-knows what it is doing:

- **`--continue` verifies before it continues.** It checks the index is actually free of unmerged entries before running `rebase --continue`. Running it with unresolved conflicts produces a second, more confusing failure, and the user learns to avoid the command.
- **`abort` restores, it does not reset.** Record the pre-operation `HEAD` in the run row first, then `git rebase --abort` / `git merge --abort`, then verify the ref matches what you recorded. A tool that aborts a rebase has already thrown away commits unless the ref was captured first.

This is **one repo at a time, never a fleet loop.** The same rule as checkpoint: cloning mid-transaction is how a typo in a local path becomes a network operation. Resolve is interactive by nature, and an interactive command over 20 repos is not a command.

#### Most rejections are not conflicts — rebase before you call an agent

A `git push` that is rejected is overwhelmingly **not** a merge conflict. It is a branch that fell behind, and the fix is mechanical: `git fetch` → `git rebase origin/<base>` → `git push --force-with-lease`. There is nothing for a language model to reason about, and dispatching one would be paying a model to run three git commands. The engine is for cases that actually need judgement.

So the flow inverts the naive one — **ro tries the mechanical fix first, and only involves the agent on a real conflict**:

```
push rejected
   │
   ├─ ask: rebase onto origin/<base> and retry?  (default yes; --yes for CI)
   │
   ├─ git fetch && git rebase origin/<base>
   │     │
   │     ├─ clean ──→ git push --force-with-lease     ← the common case, no agent
   │     │
   │     └─ CONFLICT
   │           │
   │           ├─ --resolve : dispatch the engine, second time, with a conflict
   │           │            prompt. It edits files and `git add`s the resolutions.
   │           │            It does NOT run `rebase --continue` and does NOT push —
   │           │            ro does both, after verifying the index is clean.
   │           └─ no flag  : show the conflicted paths, offer mergetool, hand over
   │
   └─ still rejected after rebase -> non-fast-forward persists; report and stop
```

`--force-with-lease`, never `--force`. The lease is the only thing standing between "my branch moved during the rebase" and silently overwriting someone else's push, and a tool that runs unattended across a fleet is exactly where that matters.

**The second engine dispatch is a different call, not a retry of the first.** It gets a different prompt (`resolve the conflicts in these paths; do not rebase, do not push`) and the same stripped environment. It edits working-tree files and stages them; ro runs `rebase --continue` and then pushes. The engine never touches the remote on this path either — the rule does not relax just because the situation got harder.

Two reasons this ordering is worth the extra branch: a model call costs money and latency on the path that happens most, and an agent that "resolves" a non-conflict has no way to know it was asked to fix something that was never broken.

### Aliases and deprecation — a first-class requirement, not a release-note line

The survey notes there are **zero aliases anywhere in the clap tree today**, so nothing is in the way. `ro add`, `ro list`, `ro remove`, and `ro prune` are the most-used commands in the tool and the regroup breaks all four without warning. **This is a bigger compatibility break than the exit-code redefinition and needs the same treatment, not less.**

- Add clap `alias` / `visible_alias` for `add`, `list`, `remove`, `prune` **on the `ro repos` subcommands** for one release. Note the alias must live on the *new* nested command to keep the old flat spelling working; the top-level `Commands::Add` etc. become hidden `#[command(alias = "…")]` pass-throughs in the same PR.
- Print a deprecation line on stderr naming the replacement.
- **State a removal version in the release notes**, and put it in the docs. An alias with no announced removal is an indefinite compatibility tax.

### Notes on the regroup

- **`ro repos` is a namespace over existing, working code — with two exceptions.** `ro_sync::manage::{remove, list}` do not change; only the clap path moves. `add` gains a clone step in front of the existing row insert (see the init/add split above), and `update` is new code in `ro_sync::manage` (`pub fn update(conn, &RepoKey, &RepoPatch) -> Result<Repo>`) — a targeted `UPDATE repos SET … WHERE host = ? AND owner = ? AND name = ?`, never `remove` + `add`. It needs **no schema change**: `alias`, `branch`, `archived` and `disabled` are all columns on `repos` today (`migrate.rs:66-82`), and there is no remotes storage anywhere in the schema, which is why `update` has no `--remote` flag.
- **`ro repos scan` needs a real extraction, not a "reuse".** `collect_git_dirs` is a **private** `fn` at `ro-sync/src/prune.rs:163` and `ro-sync/src/lib.rs` has no `pub use` for it. The only public entry point is `find_orphans`, which is coupled to the SQLite inventory and returns `Vec<Orphan>` after doing the tracked-path comparison — `scan` must not route through it. Make the walker `pub` (or move it to `ro-git::discover_dirs`) and make its `const MAX_DEPTH: usize = 4` (line 164) a parameter so `scan --depth <N>` is honest. Scan must **not** write: the vision is explicit that being in the SQLite `repos` table is what makes a repo managed, and that is an explicit `ro repos add`.
- **`ro repos tag` / `untag` / `tags` are new** because `ro checkpoint --tag <T>` is in the target spec and the `repo_tags` table (`migrate.rs:212-218`, added in V2) has **no writer and no reader anywhere in the workspace**. Today `--filter tag:X` is a fake: `main.rs:587-588` does `label.contains(rest)` against the literal string `"owner/name"`, so `--filter tag:orch` silently selects every repo whose owner or name contains "orch". The table is `PRIMARY KEY (repo_id, tag)` with an index on `tag`, so all three verbs are one statement each and `--tag` selection is already indexed — no schema work. Ship all three: with only `tag`, the table can grow but never shrink, and the only way to remove a tag is to hand-edit SQLite. See the CRUD table above.
- **Archived and disabled repos are never filtered today, and that becomes a fleet-correctness bug.** `manage::list` is `SELECT {REPO_COLUMNS} FROM repos ORDER BY owner, name` (`manage.rs:208-210`) with **no WHERE on `archived` or `disabled`**, and `resolve_multi_repo_targets` (`main.rs:554`) calls `manage::list(conn, None)`. The columns exist in the schema. Without a fix, `ro checkpoint --all` would commit and push repos the user explicitly archived or disabled. Add `WHERE archived = 0 AND disabled = 0` to the lifted target resolver **and** to `sync_all`, plus an explicit `--include-archived` escape hatch. This is the single most consequential gap for the flagship command.
- All 5 tests in `crates/ro/tests/cli_init.rs` hardcode the flat surface and break on the regroup. The test struct is still named `RfoTest` from the pre-rename era — rename it while you are there.

---

## 4. Config model

### Global — `~/.config/ro/config.toml`

On Windows this resolves to `%APPDATA%\ro\config.toml` via `dirs::config_dir()`, **not** `%LOCALAPPDATA%` — `install.ps1:138` claims the latter and is wrong. State (`state.db`) already falls back to `dirs::data_local_dir()` on Windows because `dirs::state_dir()` returns `None` there.

```toml
# ro configuration file
# Edit by hand. `ro config print` shows the effective, merged result.
# Precedence: CLI flag > <repo>/ro.local.toml > this file > built-in defaults.
# Per-repo engine + auth go in ro.local.toml at the repo root, NOT here.

[core]
# Parent directory for newly cloned repos (used by `ro repos add`).
projects_dir = "~/projects"
# Max repos processed concurrently in ONE fleet run. Owned by -j / --parallel.
parallel = 8
# Per-git-command timeout, seconds. Owned by --timeout.
timeout_secs = 60

[engine]
# Built-in engines: claude | codex | git. "git" is the raw backend / fallback,
# NOT the default: plain git cannot read the diff, split commits, or resolve
# conflicts, which is the whole reason the agent engines exist.
default = "claude"

[auth]
# CREDENTIAL SOURCE for the push, not a transport binary — see section 5.
# Same shape as the per-repo table: the key name IS the transport, the value
# is a *reference* to a secret ro reads at push time. Omit the table and ro
# uses the machine normal credential (SSH agent, credential manager, gh auth).
# https = "env:GH_PERSONAL_TOKEN"
# ssh   = "keychain:ssh-work"
#
# HARD RULE: there is NO silent fallback between credentials. If the named
# credential cannot be obtained or the push is rejected, ro STOPS and errors
# for that repo. Falling back to the machine default is allowed ONLY when
# allow_fallback is explicitly true, and then only with a loud warning naming
# the repo — a silent fallback can push via the wrong SSH key / personal
# account and leak the wrong commit author or org identity.
#
# GLOBAL-ONLY, deliberately. There is no per-repo allow_fallback key. A
# per-repo fallback posture means one fleet run can hold two different safety
# policies at once, and the question "which repo did we push as the wrong
# account?" then has no single answer afterwards. The per-run
# --allow-fallback flag is the only override, and it is visible in the
# command line.
allow_fallback = false
# Optional identity guard on the PUSHING identity (not the commit author).
# Compared against the login the credential reports; on mismatch the repo
# aborts before any push.
# expected_login = "quangdang46"

[checkpoint]
# Pre-commit safety net. Runs BEFORE the engine is dispatched.
secret_scan = "block"   # off | warn | block
denylist     = true      # block known-secret / build-artifact paths
# Off by default: runs cargo test / npm test / … over the WHOLE tree, so an
# unrelated pre-existing failure blocks a one-file checkpoint, and across a
# 20-repo fleet it is the dominant cost.
quality_gates = "off"   # off | on

# Engine binaries. Three FIXED slots, not a free-form table: a typo like
# `engines.cladue.bin` would otherwise parse cleanly, become a "registered"
# engine, and fail at dispatch. Unknown keys here are a hard error.
[engine.claude]
bin = "claude"
args = ["-p", "--output-format", "stream-json"]

[engine.codex]
bin = "codex"
args = ["exec"]

[engine.git]
bin = "git"
args = []
```

**One owner per knob.** `parallel` is owned by `-j`, `timeout_secs` is owned by `--timeout`; the CLI flag overrides the config key and there is no third source. The draft's self-contradiction on `--quiet` is resolved by deletion (1j) — the target tree above carries neither `--quiet` nor `--verbose`.

**Cut from the shipped `DEFAULT_CONFIG_TOML`:** `[mcp]` (no `ro mcp` subcommand exists; the `ro-mcp` crate was deleted in `5fdece9`), `[jobs]` (configures a job runner that does not exist), `[review]` (validated at `validate.rs:50-61`, read by nothing), `[safety].require_plan_for_ai_apply` and `[safety].max_auto_apply_risk` (the deleted ro-review lifecycle), `core.layout` (dead **and** misleading — `manage.rs::resolve_local_path` ignores it and always nests `owner/name`), `git.terminal_prompt` (no TTY/prompt code path exists anywhere).

**Fix the template header at `ro-config/src/paths.rs:127-128`** (not 123-124), which tells every user to run `ro config edit` and to "See `ro config show --json`". `ConfigCommands` is only `Print` and `Set { pair }` (`main.rs:381-389`). Neither `edit` nor `show` exists.

**Back-compat, stated deliberately.** The shipped file's `[providers.claude]` / `[providers.codex]` tables become the fixed `[engine.claude]` / `[engine.codex]` slots. An existing user's `[providers.*]` keys will be **silently ignored**, because `AppConfig` has no `deny_unknown_fields` and unknown tables are dropped by serde. That is acceptable *only* because the values are pure defaults that the engine registry supplies anyway — the user's customized `bin`/`args` are lost. Say so in the release notes. The alternative — adding `deny_unknown_fields` to `AppConfig` — would make every existing config file **unloadable** and turn `ro doctor` into "invalid config" for the entire user base. That omission is deliberate; it does not extend to the new `RepoConfig` (below), which is new-file-only and can afford to be strict.

### Per-repo — `ro.local.toml` at the repo root

Single file, **not** a `.ro/` dotfolder. Holds engine + auth identity/policy **only**. Explicitly not the inventory (that is SQLite), not jobs, not mcp, not safety.

```toml
# ro per-repo config. Not checked in (ro init adds it to .gitignore).
# Holds the commit identity, the credential reference, and the engine for
# THIS repo. Precedence: CLI flag > this file > global config.toml.

[identity]
# The commit AUTHOR ro uses for this repo. Applied at commit time as
#   git -c user.name=<name> -c user.email=<email> commit
# so nothing is written to .git/config and nothing leaks into your other
# repos. This is what makes "work repo commits as the company, personal repo
# commits as me" a one-file change instead of a git config ritual.
# Both optional — omit and git's own identity applies.
name  = "Quang Dang"
email = "me@gmail.com"

[auth]
# Named credentials, one key per transport. The key name IS the transport,
# so there is no separate `provider` setting to keep in sync with it.
#
# OMIT the whole table and ro pushes with the machine's normal credential —
# SSH agent, git credential manager, `gh auth` — which is the correct answer
# for a work repo whose SSH key is already right and needs no config at all.
#
#   https = "env:GH_PERSONAL_TOKEN"   -> resolve that env var, push with a
#                                       per-invocation extraheader
#   https = "keychain:gh-personal"    -> resolve from the OS keychain
#                                       (Windows Credential Manager /
#                                        macOS Keychain / Secret Service)
#   ssh   = "keychain:ssh-personal"   -> use that key explicitly
#
# There is no `token` key, ever. The value is a *reference*; the secret is
# read at push time and exists only in ro's memory and in the argv of the
# one git invocation that needs it. Switching accounts is editing one
# string, with no `gh auth login` and nothing to re-authenticate.
https = "keychain:gh-personal"

# Optional GUARD on the PUSHING identity. This is not the commit author —
# that is [identity] above, which ro sets and therefore cannot get wrong.
# This is the account the remote sees, which ro does NOT control, so it is
# worth checking: compared against the login the credential reports, and on
# mismatch this repo aborts before any push.
# expected_login = "quangdang46"
# expected_login = "quangdang46"

[agent]
# Which engine commits this repo: "claude" | "codex" | "git".
engine = "claude"

# Optional. Overrides the binary + args, and the instruction, for this repo.
# This is how Gemini / Amp / Kiro / a nightly build gets used without waiting
# for a ro release. `{prompt}` is substituted as ONE argv element — never
# through a shell, because the prompt is built from diff text and file paths.
# command = 'claude -p "{prompt}" --output-format stream-json'
# prompt  = "Read the diff, group into logical commits, do not push."
# NOTE: a custom prompt must not tell the agent to push. ro pushes, with the
# credential and identity it resolved. An engine that pushes makes every
# auth guarantee in this tool advisory.

# Default push mode when `ro checkpoint` runs without --wip / --direct:
#   "wip"    -> new branch + draft PR into the base
#   "direct" -> commit onto the current branch
#   "off"    -> commit locally, never push
# The CLI flag always wins. This exists so a 20-repo fleet run does not need
# the flag typed 20 times; it is not a third mode.
mode = "wip"
```

**No `name`, no `branch`, no `default_branch`, no `remote`, no `owner`/`repo` — and that is the whole point of the file.** This file lives at the root of the repo it describes. The repo's name is `git remote get-url origin`; its branch is `git rev-parse --abbrev-ref HEAD`; its default branch is `git symbolic-ref --short refs/remotes/origin/HEAD`. Every one of those is a question git answers correctly today, and every one duplicated into a config file is a field that goes stale on a rename and disagrees with disk until someone notices. A per-repo config should describe **how this repo behaves** — who commits it, what pushes it, which agent reasons about it — and nothing about **what it is**.

**No `[github]` block, and specifically no `owner` / `repo` in it.** The inventory row already *is* the `(host, owner, name)` key, and the file lives inside the repo it describes. Declaring the identity twice means a rename has two places to go out of sync, and the loser is silent. A per-repo file should say how this repo behaves, not what this repo is.

**`allow_fallback` is absent from all three tables, by design.** It is a global-only safety posture; see the rule in §4. A per-repo fallback means one fleet run can hold two policies at once, and "which repo did we push as the wrong account?" then has no single answer afterwards.

### Precedence rules

Highest first: **CLI flag > `ro.local.toml` > global `config.toml` > built-in defaults.**

There is currently **no** precedence instance anywhere in the workspace. Every flag uses a clap `default_value_t` and never consults config in either direction; `AppConfig` is roughly 90% dead config — `core.layout`, `core.parallel`, `core.timeout_secs`, `core.projects_dir`, `github.auth`, `git.*`, `jobs.*`, `mcp.*`, `review.*`, `safety.*` are parsed, validated, printed, and ignored. `core.projects_dir` is actively shadowed: `ro add` passes `paths.state_dir.join("projects")` (`main.rs:640`) and never the config value.

Implementation, in `ro-config/src/resolve.rs`:

```rust
pub struct CliOverrides {          // None = "not specified on the CLI"
    pub engine: Option<String>,
    pub auth: Option<AuthProvider>,          // ro_core  Https | Ssh | Machine
    pub allow_fallback: Option<bool>,
    pub timeout_secs: Option<u32>,
    pub parallel: Option<usize>,
    pub mode: Option<PushMode>,              // Wip | Direct | Off
}

pub struct EffectiveConfig {
    pub engine: String,                       // "claude" | "codex" | "git"
    pub auth: AuthPolicy,                     // ro_core — see section 5
    pub identity: CommitIdentity,             // author ro APPLIES, not a guard
    pub mode: PushMode,                       // Wip | Direct | Off
    pub checkpoint: SafetySettings,
    pub timeout_secs: u32,
    pub parallel: usize,
    // No `paths` field. Paths are where things live on disk, not what the user
    // configured; every consumer that wants them already holds a ConfigPaths.
}
```

Because the engine/auth fields are a small fixed set, do **not** attempt a general serde deep-merge. Load global → overlay only the engine/auth/identity fields from the repo file → apply CLI last.

**Six rules that must not be skipped:**

1. **`deny_unknown_fields`, not `#[serde(flatten)]`.** The two are mutually exclusive in serde, and flatten would contradict the guarantee below. A typo (`authent = "gh"`) parses cleanly, has zero effect, and `ro doctor` still reports "config valid" — a half-implemented per-repo file fails *silently*, which is the most dangerous property this migration can introduce. `ro-config/src/policy.rs`'s `#[serde(flatten)] extra` is a different semantic (warn, do not fail) and is not the precedent for this.
2. **No `token` field anywhere — and the credential is a *reference*, not a secret.** `ro-config` does not depend on `ro-github` (the edge runs the other way), which is why `doctor.rs:249`'s hint to "set `[github].token` in config.toml" points at a key that does not exist. Fix that hint string. A per-repo `token = "ghp_…"` is the failure this rule exists to prevent: it is the reason the research flagged `AuthToken`'s derived `Debug` as a leak waiting to happen, because a secret in a config file eventually reaches a `tracing` field or a panic message. `credential` holds `keychain:<name>` or `env:<VAR>` and never the value.
3. **The same fixed-slot discipline applies to `[engine.*]`.** Three `#[serde(deny_unknown_fields)]` sub-structs, not a free-form map.
4. **`allow_fallback` is the one auth field a repo may not set.** `RepoConfig` carries `identity.*`, `auth.provider`, `auth.credential`, `auth.expected_login`, `agent.engine`, `agent.mode` — and **not** `allow_fallback`. Resolve it from the global config and the `--allow-fallback` flag only, then inject it into `AuthPolicy` at the end. The reasoning is the audit trail: a fallback is a *safety posture*, and a per-repo posture means one fleet run can be holding two of them at once. The question "which repo did we just push as the wrong account?" has no single answer after that, which is the exact failure the whole auth design exists to prevent. A stale `allow_fallback` in someone's `ro.local.toml` now errors under rule 1 and names the right file.
5. **`identity.email` and `auth.expected_login` are different things and must not be merged.** `identity.*` is the commit **author**, which ro *sets* — via `git -c user.name=… -c user.email=…` on the commit invocation, writing nothing to `.git/config`. It therefore cannot be wrong, so it needs no guard. `expected_login` is the **pushing** identity, which is whatever the credential turns out to be and which ro *cannot* control — that is what a guard is for. Collapsing them produces a design where setting your author silently starts author-guarding your push, or worse, where a correct author suppresses a wrong-account push. Keep them separate in the struct, in the config, and in the docs.
4. **Consider typed enums for engine/auth.** Every field being a `String` means `auth = "gt"` survives deserialization and only fails at `validate` time. `AuthProvider` should be a real enum; `engine` can stay a `String` validated against the builtin table.

### `ro doctor --fix` does not upgrade an existing config — plan for it

`check_and_optionally_fix_config` writes `default_config_toml()` **only when the file is absent**. After Phase 2 rewrites `DEFAULT_CONFIG_TOML`, every existing user keeps their dead `[mcp]`/`[jobs]`/`[review]` config indefinitely, and `validate()` keeps checking keys the schema no longer models. Decide explicitly: either (a) make `--fix` detect a stale schema version stamp and offer an in-place upgrade, or (b) document that `--fix` is create-only and add a `ro config upgrade`-style path in Phase 2. Leaving it undecided means the reconciliation in section 4 silently never reaches anyone who already ran `ro init`.

Two adjacent doc bugs in the same function: `doctor.rs:200` discards the applied-fix count via `let _ = applied_fix_count;`, and the module doc's claim that every mutation "backs up the prior state to `<state_dir>/doctor/runs/<run-id>/`" is **false** — no backup code exists.

### `ro config set` must stop destroying the file

It currently loads, mutates one of 7 hardcoded keys, re-validates, and `fs::write`s the entire `AppConfig` back via `toml::to_string_pretty` — with **no backup, no merge, and no comment preservation**. On every invocation it silently drops all comments and every key not modelled by `AppConfig`. The first write after cutting `[mcp]`/`[jobs]`/`[review]` would destroy those blocks in every existing user's config.

**Use `toml_edit` for a surgical in-place edit.** Do not also add a `.bak` — a backup written on every call is noise, and it does not fix comment/key loss anyway. Surgical editing does. Pick the surgical fix, not both.

### `.gitignore` auto-add behaviour

`ro init` appends `ro.local.toml` to the repo's `.gitignore`, **idempotently**:

- If `.gitignore` does not exist, create it with `ro.local.toml` as the only line.
- If it exists, append only if no existing line matches (trim trailing whitespace; accept `ro.local.toml` and `/ro.local.toml`, with or without a trailing comment).
- Never duplicate, never reorder, never rewrite existing lines. Copy the exists-check pattern from `loader.rs::write_default` (create-if-absent, returns `Ok(false)` when present) and add the line-matching logic.

`ro init` stays deliberately dumb **about engines**: hardcode `engine = "claude"` + `auth = "gh"`, write the global config, print the path and the one-line override hint. **No PATH scan, no wizard, no questions, no validation of binary availability.** Missing-binary errors surface at `ro checkpoint` time, not at init.

It is *not* dumb about repos. `ro init` is the onboarding verb and has three modes, disambiguated by argv and cwd — the full table is in section 3. What must not regress while adding them: **no mode ever walks the current directory recursively on its own.** Mode selection reads `.git` in cwd and the flag list, nothing more. `ro init --add-dir .` is explicit, and so is `--non-interactive` for it.

The `.gitignore` rules above are the per-repo half and apply to every mode that touches a working copy (onboard-repo, `--add-repo`, `--add-dir`, and `ro repos add` when the clone lands in a directory the user names). Global-only mode touches no repo and therefore writes no `.gitignore`.

---

## 5. Engine + transport architecture

**One new crate: `crates/ro-engine`.** The orchestrator stays in the `ro` binary; `resolve_multi_repo_targets` (`main.rs:554-611`, 57 lines) moves to a `ro-sync::targets` module that `ro` already depends on. The draft's two-crate split mints a hard cross-crate boundary between `EngineOutcome` and `RepoOutcome` for very little — and its own trait sketch already showed the symptom, with `EngineOutcome::Failed { class: FailureClass }` forcing `ro-engine` to depend on `ro-jobs` just to name an error. `ro-engine` must be registered in root `Cargo.toml` `[workspace] members`, in `[workspace.dependencies]`, and as a path dep in `crates/ro/Cargo.toml`; all three are named in the Phase 4 file list. No async runtime, no plugin registry, no dynamic loading.

### The crate layout after this plan: five modules, not twelve crates

Twelve crates for a tool with ten commands is the other half of the over-engineering this plan exists to remove. The cuts in PR 1 already delete four of them, and the reshuffle below takes the rest to **five modules** without adding a single new concern:

| Module | What it owns | Where it lives |
|---|---|---|
| **Registry** | the `repos`/`run_events` tables, `add`/`remove`/`update`/`list`/`doctor`/`tag`, target resolution, the shared positional repo-name resolver | `ro-state` (schema + queries) and `ro-sync::manage` (operations), merged conceptually; `ro-jobs` folded in and its `runs` accessors kept |
| **Git** | every `git` invocation: status, stage, branch, commit, push, fetch, rebase, merge, conflict detect, `RepoLock` | `ro-git`, unchanged in responsibility. This is where `git -c user.name=…` and the credential extraheader go |
| **Agent** | `enum EngineKind` + `trait Engine` + the claude/codex/git built-ins + discovery + per-engine timeout | `ro-engine` — **the one new crate** |
| **Auth** | `AuthPolicy`, `CommitIdentity`, the keychain/env credential resolver, the author guard, the fallback policy | `ro-core` for the types; resolution lives with the Git module because a resolved credential is a `git -c` argument |
| **GitHub** | `gh pr create` / `gh pr list` / `gh api user` — **shelled out, no crate** | `ro-github` survives only for `auth::discover_token`, which `ro doctor` calls; PR creation does not go through octocrab at all (see the async note below) |

Crates that go away entirely: `ro-review` and `ro-dep-update` (PR 1), `ro-output` (PR 1), `ro-jobs` as a separate crate (folded into Registry), `ro-config`'s `policy` module (PR 1), and `ro-github` reduced to one function. That is **12 → 5**, with `ro-config` becoming a module inside Registry and `ro-sync` becoming the other half of it. Do this as a rename-and-move inside PR 2 and PR 3, not as a separate "reorganise crates" PR: a structural PR that moves code and touches no behaviour is a PR nobody reviews properly, and it would be the one PR where the two-crates-per-concern mistake could slip back in.

### The trait

```rust
// crates/ro-engine/src/engine.rs

pub struct EngineContext<'a> {
    pub repo_root: &'a Path,
    pub owner: &'a str,
    pub name: &'a str,
    /// Branch that was checked out BEFORE any WIP branch was created. Captured
    /// in preflight and immutable — an agent engine may switch branches itself,
    /// so this cannot be re-read later. It is the PR `base`.
    pub base_branch: Option<String>,
    /// Resolved per-repo, after CLI > ro.local.toml > global.
    pub auth: &'a AuthPolicy,                 // ro_core
    /// Merged into the child process env. See "env injection" below: this
    /// makes GH_TOKEN reach `gh`, it does NOT authenticate `git push`.
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    /// A supplied message replaces engine-generated messages and forces a
    /// single commit. None means the engine decides.
    pub message_override: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
pub enum Availability { Found(PathBuf), Missing }

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EngineOutcome {
    Committed { commits: Vec<CommitRecord> },  // empty vec == engine found nothing
    Failed { error: String, class: FailureClass },   // ro_core — see below
    TimedOut { after: Duration },
    Unavailable { binary: String, hint: String },   // detected lazily at dispatch
}

#[derive(Clone, Debug, Serialize)]
pub struct CommitRecord { pub sha: String, pub subject: String, pub files: Vec<PathBuf> }

pub trait Engine: Send + Sync {
    fn id(&self) -> &'static str;                    // "claude" | "codex" | "git"
    fn bin(&self) -> &str;
    fn default_args(&self) -> &[String];
    /// Cheap PATH probe. Called at DISPATCH time, never at init or doctor.
    fn availability(&self) -> Availability;
    /// Read the diff, split commits, write the messages, resolve conflicts.
    /// Returns a VALUE, never a Result — that is the structural guarantee that
    /// one failing repo cannot stop the fleet.
    fn checkpoint(&self, ctx: &EngineContext<'_>) -> EngineOutcome;
}
```

### The engine commits. `ro` pushes. This boundary is load-bearing.

An engine **must not push**. It reads the diff, decides how to group the work, writes the commits, resolves conflicts — that is the reasoning, and it is the reason an engine exists at all. Then it stops. `ro` performs the push, with the credential it resolved and the identity it applied.

This is not tidiness. Every guarantee in the auth design depends on it:

| If the engine pushes | What is lost |
|---|---|
| `git -c http.…extraheader=…` | The agent may push over whatever credential *it* finds — usually SSH |
| `expected_login` guard | A guard that cannot be enforced is a comment, not a control |
| `SecretString` | The secret has to be handed to a subprocess's environment to be useful, which is where it leaks |
| per-repo `[identity]` | The agent can commit as whoever `git config` says, ignoring what ro applied |
| the no-silent-fallback rule | Fallback becomes a property of an agent's behaviour, not of ro's policy |

**Concretely: the default prompt must not contain the words "and then push".** The built-in prompt ends at "write the commits" — ro pushes them. This was left ambiguous in an earlier draft of this document and it is worth being blunt about why, because the ambiguity is tempting: letting the agent do the push is one line shorter. It trades every identity guarantee in the tool for that one line.

**"Do not modify git config" is in the prompt, but the prompt is not the control.** An agent that runs `git config --local user.email other@example.com` writes to `.git/config` and that write persists — the next commit, and the next `ro` run, sees it. The line in the prompt is a request. The actual guarantee is that **ro re-asserts `-c user.name=… -c user.email=…` on every `git commit` it performs**, so the per-invocation value wins regardless of what is in the config file. ro should additionally record `.git/config`'s hash before dispatching an engine and compare it after, and report a config mutation as a per-repo warning — not because it can undo it, but because an agent writing git config is a behaviour the user needs to know happened.

The same boundary means an engine's outcome is *commits*, not *remote state*. `EngineOutcome::Committed { commits: Vec<CommitRecord> }` is the only success variant; there is no "pushed" because the engine never was.

`Availability` and `TimedOut` exist because a bare `Failed` cannot distinguish a normal error from a killed child, and the whole reason the trait is a trait rather than an `enum` is that adding a fourth engine must not mean editing every match arm. A `match` over a fixed 3-variant enum would force exactly that. The registry itself is a fixed three-entry array — the trait carries the extensibility, the registry does not pretend to be a plugin system.

### `AuthPolicy` lives in `ro-core` — once

The draft defined `AuthPolicy` in **two** crates: `ro-config/src/resolve.rs::EffectiveConfig { pub auth: AuthPolicy }` and `crates/ro-github/src/auth.rs`. That requires both `ro-config -> ro-github` **and** `ro-github -> ro-config`, and `crates/ro-github/Cargo.toml` declares no `ro-config` dependency (its only workspace sibling is `ro-core`). Defining it once in `ro-core` — already a leaf that `ro-config`, `ro-github`, and `ro-sweep` all depend on — kills the cycle, and it is also where the failure discriminant belongs:

```rust
// crates/ro-core/src/auth.rs
pub enum AuthProvider { Https, Ssh, Machine }

pub struct AuthPolicy {
    pub provider: AuthProvider,   // derived from WHICH [auth] key is present, or Machine if none
    pub allow_fallback: bool,
    pub expected_login: Option<String>,   // verified when provider == Https
    pub author_email: Option<String>,     // verified when provider == Git
}

// crates/ro-core/src/failure.rs  (moved out of ro-jobs; ro-jobs re-exports)
pub enum FailureClass { /* the survivors of the ro-jobs / ro-git merge */ }
```

This also removes the unwanted `ro-engine -> ro-jobs -> ro-state -> rusqlite` edge. `ro-jobs` keeps its SQLite accessors; it just no longer owns the taxonomy.

### The three built-ins

**`GitEngine`** — the raw execution backend and explicit fallback, **not** the default. It cannot read the diff, split commits, or resolve conflicts, which is exactly why the agent engines exist. It does `stage_all` → `commit_all` with a single message (the `--message` flag, or a generic fallback if none is given). It never spawns a second process to invent a message.

`GitEngine` needs new ro-git primitives, because `ro_git::mutation::commit` (`mutation.rs:270`) does `git add -- <explicit files>` and **bails on an empty file list** — WIP-style "commit everything" is impossible through the typed API today, and `ro-sweep/src/commit.rs:110-140` bypasses it with a hand-rolled `git add -A` that violates ro-git's own stated rule.

**`ClaudeEngine` / `CodexEngine`** — thin wrappers: spawn `bin` + `args` in `repo_root` with the injected env, capture stdout/stderr, parse the commit list out of the stream, enforce a hard `timeout` and kill the child tree on expiry. No prompt-engineering sophistication lives here; the contract is "hand the worktree to the agent, get commits back".

**The built-in prompt, and what it must not say.** The instruction handed to a `claude` or `codex` engine, with `{prompt}` substituted into `command`:

```
Read the full diff of this repository. Group the changes into a small
number of logically connected commits, in dependency order, and write
each one with a specific subject line describing what that commit
actually does.

Do not edit any source file. Do not reformat. Do not add obviously
ephemeral files (build output, lockfiles from an unrelated package,
scratch notes, editor backups).

Do not push, and do not run any command that contacts the remote. Do
not modify git config — not `git config --local`, not `git config
--global`. Your author identity is already set for you; the caller
handles the credential, the remote, and the push.
```

That last sentence is the important one, and it is there because the natural instinct — and the shape of most prompt examples floating around — is to end with "commit all changed files and then push". **Do not.** `ro` pushes, with the credential it resolved, under the identity guard it applied. An engine that pushes makes every auth guarantee in this document advisory.

`{prompt}` is substituted as a **single argv element**, never through a shell. `command = 'claude -p "{prompt}"'` is split on whitespace respecting quotes, the prompt goes in as one argument, and the whole thing is executed as an arg vector with no shell involved. This is not fastidiousness: the prompt contains file paths and commit text harvested from a diff, and passing any of it through a shell is a command-injection path into the user's own account.

### `[agent] command` and `[agent] prompt` — overriding without a release

The three engines ship, but the *configurable* part of an engine is exactly two strings: what to run, and what to tell it. Exposing both means Gemini, Amp, Kiro, or a nightly Claude build is a config change rather than a release:

```toml
[agent]
engine = "claude"                    # selects the built-in behaviour profile
command = 'claude -p "{prompt}" --output-format stream-json'   # optional
prompt  = "Read the diff, group into logical commits, do not push."  # optional
```

`command` overrides `bin` + `default_args` for that repo; `prompt` replaces the built-in instruction. Precedence per field: `ro.local.toml` → global `config.toml` → built-in. Because the boundary is "agent commits, ro pushes" and not "agent commits and pushes", a custom `command` is safe in a way a custom *workflow* would not be: whatever binary you name, ro still owns the push, so the credential and the identity guard still apply to it.

This is also the honest answer to "how do I add a new agent". It is not a plugin registry — it is two strings, and the fixed three-entry `EngineKind` still exists only to pick the behaviour profile (timeout, output parsing, availability probe). The `[engine_claude]`-style fixed slots in `config.toml` cover the "installed under an unusual name" case; `[agent] command` covers the "different agent entirely" case. Neither is a free-form table, so neither reintroduces the silent-failure class.

### Discovery and lazy validation

- The registry is a **fixed three-entry table** built from the three `[engine.*]` slots in `config.toml`, each a `#[serde(deny_unknown_fields)]` sub-struct. Not a free-form `[engines.*]` map — a typo there would parse cleanly, become a "registered" engine, and fail at dispatch, which is the exact silent-failure class this plan flags everywhere else. Override a binary name with the fixed slot or a CLI flag, not by inventing a table key.
- `availability()` is a PATH probe. **Move `which_in` out of the binary**: it currently lives at `crates/ro/src/doctor.rs:392` inside `mod doctor;` in the `ro` **binary** crate, so a new library crate cannot call it. It is a subprocess PATH probe — that is ro-git's job. Move it to `ro-git` and have both doctor and the engine call it there. It already handles the Windows `.exe` suffix.
- **Availability is checked at dispatch time only.** Not at `ro init`, not at `ro doctor`. This is a single rule with no exception: **do not add a `gh` probe to doctor**, even though gh is the preferred credential source. Doctor stays a fast environment check. (Its existing `check_provider("claude"|"codex")` probes already return `Severity::Optional` in *both* branches — `doctor.rs:372-386` — so they can only ever warn and never move the exit code. Leave them as informational; note that `doctor.rs:536 provider_check_handles_missing_binary` asserts `Severity::Optional` and `doctor.rs:562 run_returns_at_leat_six_checks` asserts `>= 6` with an inline comment listing the six checks, so any change here breaks two tests.)
- An `EngineOutcome::Unavailable` for one repo is a per-repo failure, not a fleet abort.

### Transport: credential source, not transport binary

**octocrab cannot push commits.** Its only git-data endpoint is `repos::create_ref` (`POST /{owner}/{repo}/git/refs`), which requires loose objects to have been uploaded first via the git blobs/trees API. There is no push. So "`gh` transport" vs "`git` transport" is not a real binary-level distinction — the plan's earlier framing of a no-fallback state machine around that distinction was unimplementable as written.

**The actual rule: always `git push`.** The config selects the **credential source**, not the executable. There is no `provider` setting any more: the key name in `[auth]` *is* the transport, so the two can never disagree.

- **`https = "<ref>"`** supplies an HTTPS credential. **A `GH_TOKEN` in the environment does not authenticate `git push` — git does not read it, full stop.** The concrete mechanisms, in order of preference:
  1. `git -c http.https://github.com/.extraheader="AUTHORIZATION: basic <base64(x-access-token:TOKEN)>" push` — a per-invocation header, nothing persisted, no global config mutated. This is the mechanism the plan specifies.
  2. A scoped `credential.helper` invocation.
  3. `gh auth setup-git` (mutates the user's global git config; least preferred for that reason).
  The env-injection seam is **necessary for `gh` and for nothing else.** An earlier revision of this document said the token also went to the agent engines; that was wrong, and it is reversed below.

- **`ssh = "<ref>"`** names an SSH key explicitly. **Omitting the whole `[auth]` table** uses the repo's existing SSH key or credential manager, unchanged — which is the right answer for a work repo whose SSH key is already correct, precisely because it needs no ro config at all.

**Where the token comes from — `credential`, never `token`.** A per-repo config that holds `token = "ghp_…"` puts a live credential in a plaintext file, which is a leak waiting for the first `tracing` field or panic message (the research found `AuthToken` already derives `Debug`). So `ro.local.toml` holds a **reference**, and the resolver is:

```
resolve_credential(repo_root) -> Result<SecretString, CredentialError>

  1. auth.credential in <repo>/ro.local.toml
       "keychain:<name>"  -> OS keychain via the `keyring` crate
                             (Windows Credential Manager / macOS Keychain / Secret Service)
       "env:<VAR>"        -> std::env::var(VAR)
  2. no reference set:
       GH_TOKEN -> GITHUB_TOKEN -> `gh auth token`
  3. nothing  -> per-repo failure "gh credential unavailable". NO git/SSH attempt.
```

Two properties this ordering buys, and they are the whole point:

- **No re-login, ever.** Switching a repo from your personal account to your work account is a one-word change in a file you already have open — `credential = "keychain:gh-work"`. There is no `gh auth login`, no `git config` edit, and no chance of the two drifting apart.
- **CI keeps working with no OS keychain.** A headless runner has no interactive keychain, so `credential = "env:CI_GH_TOKEN"` is the answer there. The keychain is the ergonomic default, not a hard requirement — which matters, because a hard keychain requirement would make `ro` unusable in exactly the automation it is meant to help.

The resolved secret is wrapped in a `SecretString` that does **not** implement `Debug` (or implements it as `"***"`), so it cannot be logged by accident. The `keyring` crate is the one new runtime dependency this plan adds; gate it behind a default-on `keychain` feature with an `env`-only fallback so a minimal build still compiles.

**`env:<VAR>` and `GH_TOKEN` are not the same thing, and the difference matters.** `env:WORK_GH_TOKEN` names a *specific* variable for a *specific* repo, so a fleet run can push repo A with a work token and repo B with a personal one in the same process. Plain `GH_TOKEN` is one value for the whole environment, which is exactly the "one account for everything" problem per-repo config exists to solve. The resolver prefers the reference precisely so that the per-repo case is expressible.

### The agent's environment is built by subtraction, not by addition

This is the correction that matters most in the whole auth design, and an earlier revision of this document got it wrong. The original text said the resolved token was injected into the engine's environment alongside `GH_TOKEN` for the agent's own use. **That is a leak with a long fuse**, and every step of it is outside ro's control:

```
1. ro spawns `claude` with GH_TOKEN in its environment
2. the agent runs `env`, or `printenv`, or a build script, or a crash reporter
3. the value lands in the agent's stdout
4. the agent's stdout lands in its transcript, which is written to disk
5. the transcript is uploaded, pasted into an issue, or read by a different
   model on the next session
```

There is no point in that chain where ro can intervene, and the destination is frequently a third party. A credential is exactly the kind of thing that must not be in an LLM's context: not because the model is careless, but because **the transcript is designed to be read and shared**, which is the opposite of a secret store. `SecretString` protects ro's own logs and panics; it cannot protect a file ro does not write.

So the engine's environment is assembled as:

```
child_env  =  parent_env
              .remove(GH_TOKEN)
              .remove(GITHUB_TOKEN)
              .remove(RO_CREDENTIAL)          // if such a var is ever set
              .set(GIT_TERMINAL_PROMPT, "0")  // ro's existing hardening block
              .set(GCM_INTERACTIVE, "Never")
              .set(GIT_PAGER, "cat")
              // author, if this repo has one — see below
```

**Stripping is mandatory, not merely adding-nothing.** A user who has `GH_TOKEN` exported in their shell for `gh` would otherwise hand it to every agent ro spawns, without either of them intending it. The child gets an environment the parent's token does not survive into.

**The author reaches git through `GIT_CONFIG_*`, not through `GIT_AUTHOR_*`.** If the agent runs `git commit`, git needs an identity from *somewhere* — its environment or its config, there being no third place — so the value is unavoidably in the child's environment. An earlier revision of this document used `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` / `GIT_COMMITTER_*`; switching to the `GIT_CONFIG_*` form is better for three reasons, none of which is secrecy:

```
GIT_CONFIG_COUNT=2
GIT_CONFIG_KEY_0=user.name      GIT_CONFIG_VALUE_0=Quang
GIT_CONFIG_KEY_1=user.email     GIT_CONFIG_VALUE_1=me@gmail.com
```

1. **It applies to every git operation in the tree, not just commit-ish.** `GIT_AUTHOR_*` only covers `commit`; `user.name`/`user.email` also cover `commit --amend`, `tag`, `merge`, and anything the agent runs that creates an object with an identity. Four variables become two.
2. **It is not an instruction the agent could misread.** `GIT_AUTHOR_EMAIL=…` announces "here is your identity"; `GIT_CONFIG_KEY_1=user.email` is config plumbing the agent has no reason to interpret or second-guess. The agent just runs `git commit` and git resolves the config.
3. **It is the same mechanism as the `-c` ro already uses**, so the author is expressed once in the codebase rather than as a parallel env-var convention.

Being straight about the limit: **the agent can still read the value with `printenv`.** There is no mechanism that puts an identity where a child process cannot see it, and pretending otherwise would be worse than saying it. What is achieved is that the value is *never a credential* — it is about to be in a public commit — and that the agent has no handle on it that looks like an override.

Two layers keep the agent from *choosing*:

- ro exports the `GIT_CONFIG_*` triple from `[identity]` on the agent's environment, so there is nothing for the agent to decide.
- **ro re-asserts `-c user.name=… -c user.email=…` on every `git commit` it performs itself**, so an agent that runs `git config user.email something-else` cannot change what a later ro-driven commit looks like.

The default prompt also says *do not modify git config*, but a prompt is a request, not a control. The two layers above are the control; the prompt line is just politeness.

**There is no remote rewrite, and therefore nothing to restore.** An alternative design would `git remote set-url` to a credential-bearing URL, run the push, then restore. This plan does not, and the reason is exactly the risk you raised: the restore is a second thing that can fail, it needs a `finally`, it can be interrupted between the two halves, and it puts a credential into a file on disk in the meantime where any process and any backup can read it. `git -c http.https://github.com/.extraheader=…` is **per-invocation and writes nothing** — there is no window in which a token exists on disk, and therefore no restore to forget. A run that is killed between commit and push leaves a clean local state and a token that never existed anywhere but memory.

**The no-silent-fallback hard rule, implemented against the credential source:**

```
provider == Https:                       # [auth] https = "env:X" | "keychain:X"
    token = resolve(ref)          # env:<VAR> | keychain:<name>
    if token.is_err()      -> per-repo failure "credential unavailable: <ref>".
                             NO SSH attempt. Move to the next repo.
    verify identity (expected_login) if set -> per-repo failure on mismatch
    push with the per-invocation extraheader credential
    if the push itself fails:
        if allow_fallback -> loud stderr warning naming the repo, the requested
                             credential and the reason, then retry over the
                             machine's default credential
        else             -> per-repo failure. No retry.
provider == Ssh:                          # [auth] ssh = "keychain:X"
    resolve the key; verify expected_login if set
    push via `git push` over that key
provider == Machine:                       # no [auth] table at all
    push via `git push` using the repo's existing SSH key / credential manager
    expected_login, if set, is verified against what that credential reports
```

The fallback warning must name the repo, the requested provider, and the reason, e.g. `warning: repo_orchestrator: gh credential push failed (403); falling back to git push over SSH — this may publish under a different identity`.

**Replace `discover_token` with `resolve_credential(ref)`.** Today's chain is `GITHUB_TOKEN -> config token -> gh auth token` with every error swallowed (`auth.rs:88-93`) — that is precisely the silent fallback the vision forbids, and it is what ships today. The new function takes the **reference** from `[auth]` and resolves it, rather than searching a list of ambient sources and using whatever it finds first: `env:<VAR>` reads that one variable, `keychain:<name>` reads the OS keychain, and there is no "try the next thing" branch. `GH_TOKEN`/`GITHUB_TOKEN`/`gh auth token` remain as the **`Machine` provider's** default chain, reachable only when `[auth]` is absent — so a repo that names a credential gets exactly that credential, and a repo that names none keeps today's behaviour.

- `from_env` checks **`GH_TOKEN` first, then `GITHUB_TOKEN`** (matching gh CLI precedence). Today only `GITHUB_TOKEN` is read (`auth.rs:52`); `grep GH_TOKEN` over `crates/` returns zero hits. Use `var_os`, not `var` — the latter fails on non-UTF8 values.
- **Delete the `"config-token"` strategy.** `GitHubConfig` has no `token` field and `main.rs:1309` hardcodes `config_token: None`, so `from_config` can never succeed.
- **Delete `"auto"`.** It is the silent chain.
- Wire `AppConfig.auth` into the strategy argument. It is currently validated against `env|gh|config-token|auto` and read by **nothing** — both real call sites hardcode the literal `"auto"` (`doctor.rs:239`, `import.rs:27`).
- `AuthToken` currently `#[derive(Clone, Debug)]` (`auth.rs:10`), so `{:?}` prints the **raw token**. `redact()` is opt-in and manual, and any `tracing` field or panic message leaks the secret once tokens start flowing into child processes. Give it a **manual `Debug` that delegates to `redact()`**. This is a prerequisite, not a follow-up.
- The GHE `base_uri` at `auth.rs:100` is `https://{h}` with no `/api/v3` suffix, so Enterprise hits the web UI instead of the API. Fix it if the client path survives at all — see PR creation below.

### PR creation and identity: shell out to `gh`

**Resolve the async contradiction.** octocrab 0.44 is 100% async — every function in `issues.rs`, `checks.rs`, and `client.rs` is `async fn` and every test uses `#[tokio::test]` or `rt.block_on`. The draft's sync `fn gh_login(client: &Octocrab) -> Result<String>` and `fn create_pr(...)` **will not compile**, and they contradict the plan's own "no async runtime" bullet. Also note `ro-github/src/import.rs:28` already proved empirically that `Octocrab::builder().build()` panics with `there is no reactor running` outside a runtime.

**Resolution: PR creation and identity lookup shell out to the `gh` CLI.** This is smaller, more faithful to the vision (gh is the named preferred source and requires no `gh auth login`), and in one move it kills the octocrab async problem, the GHE `/api/v3` base-URI bug, and the `list_pulls` pagination gap (`issues.rs:116` is read-only `.list()` with no pagination, `per_page` defaulting to 50, so head_ref matching would miss a PR on a busy repo). **It also drops the `ro-github` dependency from the checkpoint path entirely** — `ro-github` stays only because `ro doctor` calls `discover_token`.

```rust
// crates/ro/src/checkpoint/remote.rs
pub fn gh_login(path: &Path, env: &[(String, String)]) -> Result<String>;   // `gh api user --jq .login`
pub fn find_existing_pr(path: &Path, head_ref: &str, env: &[(String, String)]) -> Result<Option<u64>>;
pub fn create_pr(path: &Path, head: &str, base: &str, title: &str, body: &str, env: &[(String, String)]) -> Result<u64>;
```

The duplicate-PR rule is a `gh pr list --head <head_ref> --json number` lookup; a hit means "PR #N already open" and **no second PR is opened** — just report it. The base is `ctx.base_branch`, captured before the WIP branch was created (see the next section).

**Test seam:** a fake `gh` on `PATH` is the seam, not `wiremock`. Note that a shell-script fixture is not directly executable on Windows without a shim — the test fixture helper needs a `.cmd`/`.bat` variant on `cfg(windows)`.

### The author-identity guard

```rust
// ro-git
pub fn git_config_user_email(repo: &Path) -> Result<String>;   // git config user.email
```

The guard runs **before** the push, not after. It compares the acting identity to `expected_login` (via `gh api user`) or `author_email` (via `git config user.email`) and aborts that repo's transaction on mismatch, leaving the local commits in place and the branch unpushed.

**Cache the resolved login once per run, not once per repo** — a 20-repo fleet would otherwise make 20 identical calls. This is currently the single unguarded leak in the codebase: `mutation::push` is a bare `git push` with no token injection, no login check, and no email check.

### Env injection seam — the blocking edit

`ro_git::mutation::run_in` is a **private** `fn run_in(cwd: Option<&Path>, args: &[&str])` at `mutation.rs:159`, and the `Option` is **load-bearing**: `clone()` at `mutation.rs:262` calls `run_in(None, &argv)` because git must run outside the destination repo. The draft's proposed `run_in(repo: &Path, …)` deletes the only way to spawn git without a working directory.

**Keep the `Option`, add a third parameter:**

```rust
pub struct RunOpts<'a> {
    pub env: &'a [(String, String)],   // merged on top of the hardening block
    pub timeout: Option<Duration>,      // today: Command::output() blocks forever
}

fn run_in(cwd: Option<&Path>, args: &[&str], opts: &RunOpts<'_>) -> Result<GitCommandResult>;
```

Keep the arg-vector discipline exactly as-is: `--` separators, no shell interpolation, `--no-pager`, and the `GIT_TERMINAL_PROMPT=0` / `GCM_INTERACTIVE=Never` / `LC_ALL=C` hardening block.

**Two consequences that must be planned for:**

1. **Keep a 2-argument `run()` shim, or list the callers.** `ro-sweep/src/commit_sweep.rs` calls `mutation::run(...)` at lines **204, 214, 436, 505** (plus test call sites at 592-595, 610, 625, 640, 650-653, 671). Either retain `pub fn run(repo, args)` delegating to `run_in` with a default `RunOpts`, or add `commit_sweep.rs` to the Phase 3 file list. The shim is the lower-risk option.
2. **Add a real timeout.** `mutation.rs:17-18` explicitly defers timeout enforcement to "the daemon", which is being cut. `Command::output()` blocks forever, so a hung `git fetch` or an SSH credential prompt stalls an entire fleet run indefinitely. This gets worse once claude/codex subprocesses exist.

### `RepoLock` — the two fixes are individually correct but not composable as written

`ro_git::lock::RepoLock` (`lock.rs`, fs4, 30s default) has **zero production call sites**; `ro-sync/src/sync.rs`'s module doc claiming "acquire fs4 lock → …" is a lie. Both fixes are needed:

1. `Drop` calls `remove_file(&self.lock_path)` (`lock.rs:84`) — a textbook TOCTOU race where process B can create the path after A's unlink and before A's unlock, and both then believe they hold the lock. **Leave the file; only unlock.** This breaks the test `lock_acquire_and_release` (`lock.rs:95-105`), which asserts `!path.exists()` after drop — update it in the same commit.
2. `acquire` writes `.ro.lock` **inside the worktree** and calls `create_dir_all(repo_path)`. Wired naively, `git status --porcelain` reports `?? .ro.lock`, `is_dirty` returns true, and checkpoint sees its own lock as dirty work. It also creates directories for typo'd paths.

**But `RepoLock::acquire(repo_path, timeout_secs)` has no `state_dir` parameter and derives the path from `repo_path`**, so neither fix is implementable without new plumbing. `ro-git` currently depends on no workspace crate except `anyhow`/`gix`/`serde`/`fs4`, so pointing it at `<state_dir>/locks/` either inverts the dependency direction or requires a parameter threaded through every caller. **Phase 3 must state which.** Also drop the `create_dir_all` side effect so a typo'd path does not get created.

**SQLite is not shareable across threads.** `rusqlite::Connection` is `!Sync` (it contains a `RefCell<InnerConnection>`), so `std::thread::scope` cannot pass one into workers — it fails to compile. All DB reads and writes (`open_run`, `append_event`, `finalize_run`, the target query) happen on a **single coordinator thread**; worker threads touch only git. This keeps the one-writer invariant `delete_repo_cascade` already assumes. (WAL mode and `busy_timeout=5000` are already set in `ro-state/src/lib.rs:48-58` if a per-worker connection is ever preferred.)

---

## 6. The `ro checkpoint` command

### 6.1 Where the code lives

The vision forbids a plugin registry and says only `claude`, `codex`, `git` ship. That rules out minting two new workspace members for ~1.5k lines, and it rules out a registry over a fixed three-item table. **One new crate, and the orchestrator stays in the existing binary:**

| Piece | Home | Why there |
|---|---|---|
| `trait Engine`, `EngineKind`, 3 built-ins | **NEW `crates/ro-engine`** (lib, no clap, no tokio) | A real boundary: it owns child-process spawning and timeouts, which nothing else should do. |
| Orchestrator, per-repo transaction, summary, NDJSON emitter | **`crates/ro/src/checkpoint/`** (module tree inside the existing `[[bin]]`) | The 1473-line `main.rs` already owns the fleet loop; splitting 57 lines into a new member adds a workspace entry, a path dep, and a CI target for no navigability gain. |
| Target resolution + fleet scan | **`crates/ro-sync/src/targets.rs`** | `ro` already depends on `ro-sync`. Lifting `resolve_multi_repo_targets` (`crates/ro/src/main.rs:554-611`, 57 lines) here is a *move*, not a new edge — and it puts the scan where `sync`, `status`, and `checkpoint` can all share it. |

Adding `ro-engine` requires exactly three manifest edits, and all three must land in the same commit or the crate does not build:

1. root `Cargo.toml` → `[workspace] members`: add `"crates/ro-engine"`
2. root `Cargo.toml` → `[workspace.dependencies]`: add `ro-engine = { path = "crates/ro-engine" }`
3. `crates/ro/Cargo.toml` → `[dependencies]`: add `ro-engine.workspace = true`

`[workspace.metadata.dist]` already sets `precise-builds = true` (publish only the `ro` binary crate), so the release archive is unaffected by the new member. Add a CI assertion that the published artifact list is still exactly one binary, so a future `[[bin]]` slip cannot quietly ship `ro-engine`'s test fixtures.

### 6.2 Engine dispatch — a trait, an enum, and no table

```rust
// crates/ro-engine/src/engine.rs

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind { Claude, Codex, Git }
// FromStr / Display by hand. ro-engine does NOT depend on clap; main.rs
// maps `--engine <NAME>` onto it. A lib that pulls in clap to parse three
// strings is a lib that cannot be used from a plain unit test.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability { Present(PathBuf), Missing }

pub struct EngineContext<'a> {
    pub repo_root: &'a Path,
    /// Snapshot taken during the pre-dispatch scan, BEFORE any branch is
    /// created and before the engine runs. An agent engine may switch
    /// branches itself; the PR base must not follow it.
    pub base_branch: String,
    pub auth: &'a AuthPolicy,            // ro-core::auth
    pub timeout: Duration,
    /// Supplied `--message`. Presence means "one commit, this subject".
    pub message_override: Option<&'a str>,
    /// Merged into the child process env.
    pub env: &'a [(String, String)],
}

pub enum EngineOutcome {
    Committed { commits: Vec<CommitRecord> },
    NothingToCommit,
    Failed { error: String, class: FailureClass },  // ro-core::auth
    Unavailable { binary: String, hint: String },
    TimedOut { after: Duration },                    // distinct from Failed:
                                                     // a kill is not a refusal
}
```

(`Engine` is declared once, in §5 above. `Send + Sync` because `&Engine` is shared read-only across `std::thread::scope` workers. The trait carries the extensibility; the three-entry registry does not pretend to be a plugin system.)

`Engine::checkpoint` taking a value rather than a `Result` is the load-bearing design decision. A `Result` invites `?` at some call site inside a worker, and one `?` that escapes converts a per-repo failure into a fleet abort.

**No free-form `[engines.*]` TOML table.** A user-editable table reintroduces exactly the silent-failure class the config section exists to kill: `engines.cladue.bin = "claude"` parses cleanly, registers a phantom engine, and fails at dispatch. Instead — three fixed override slots, each its own `#[serde(deny_unknown_fields)]` struct, plus a per-run binary override for the "installed under an unusual name" case:

```toml
[engine_claude]
bin = "claude"
default_args = ["-p", "--output-format", "stream-json"]
[engine_codex]
bin = "codex"
default_args = ["exec"]
[engine_git]
bin = "git"
default_args = []
```

```
ro checkpoint --engine claude --engine-bin /opt/bin/claude-nightly
```

Adding a fourth engine is one new impl of `Engine`, one `EngineKind` variant, one arm in `dispatch()`, one fixed `[engine_x]` slot. Nothing in the orchestrator, the config resolver, or the CLI changes shape — which satisfies "adding an engine later must not require reworking core" without a registry.

`which_in` currently lives at `crates/ro/src/doctor.rs:392`, inside `mod doctor;` in the **binary** crate, so a library cannot call it. It moves to `ro-git` (a PATH probe for a subprocess belongs with the code that spawns subprocesses) as `ro_git::mutation::which_in`, and both `doctor` and `ro-engine` call it from there. Its hand-rolled Windows `.exe`/`.cmd` handling is already correct and is kept.

### 6.3 End-to-end flow

```
ro checkpoint --all --execute
 │
 ├─0. PARSE          per-field resolve_paths; open DB; load + validate config
 ├─1. RUN RECORD     ro_jobs::open_run(conn, "checkpoint", &args) -> run_id
 ├─2. TARGETS        ro_sync::targets::resolve(&conn, &Selector, &opts)
 │                    WHERE archived = 0 AND disabled = 0     <-- unless --include-archived
 │                    real repo_tags lookup for --tag
 │                    invalid glob is an ERROR (no silent "*" fallback)
 │                    has: branch removed (it was a bool expression, not a match)
 │                    0 targets matched an explicit name/--tag     => usage error 64
 ├─3. SCAN           ONE snapshot for the whole run, on the coordinator thread:
 │                    branch, dirty, ahead/behind, worktree-exists, remote,
 │                    base_branch = the branch currently checked out
 │                    <-- this is the ONLY DB-touching phase before workers
 ├─4. DRY RUN        default. Print per-repo "would ..." from the step-3
 │                    snapshot. No engine, no lock, no branch, no push.
 │                    (dry-run and execute read the SAME snapshot shape, so
 │                     the printed plan cannot drift from the real outcome)
 ├─5. WORKERS        std::thread::scope, N = resolve_effective().parallel
 │     one target each, touching ONLY git + the filesystem:
 │       a. lock        RepoLock::acquire_at(state_dir/locks/<hex>.lock, t)
 │       b. preflight   conflict::detect -> Skip
 │                     is_dirty / `status --porcelain=v1 -z -uall` -> Skip(Clean)
 │                     denylist + secret_scan -> Block  (BEFORE the engine runs)
 │       c. config      resolve_effective(repo_root, cli)  [files only, no DB]
 │       d. mode        protected + --direct -> refuse, force WIP
 │       e. base        `git fetch` + `git rebase --autostash origin/<base>`
 │                     -> on conflict: Stop, or --resolve (see below)
 │       f. branch      WIP only: create + checkout ro/wip/<slug>-<run>
 │       g. engine      engine.checkpoint(ctx)
 │       h. push        always `git push`; credential per AuthPolicy
 │       i. PR          WIP + GitHub remote: `gh pr list --head` -> create or reuse
 │       j. unlock
 │       k. return      RepoOutcome  (value; `?` is forbidden, it becomes Failed)
 ├─6. COORDINATOR    drains outcomes IN TARGET ORDER (not completion order),
 │                    writes one ro_jobs::append_event row per repo
 ├─7. SUMMARY        totals + per-repo one-liners; NDJSON if --format ndjson
 └─8. FINALIZE       ro_jobs::finalize_run(conn, run_id, exit_code)
                      0 all ok | 1 partial | 2 all failed
```

**Step (e) rebases *before* the engine, and that ordering is the reason the happy path never force-pushes.** An earlier revision of this flow ran `commit → push → on rejection rebase → force-push`, which is the conventional order and it has a structural cost: the branch is rewritten, so the push needs `--force-with-lease`, and a force-push across a fleet is the one operation in this tool that can destroy someone else's work.

Rebasing first inverts that. The worktree is dirty, so `git rebase --autostash origin/<base>` stashes, rebases, and pops — the tree comes back dirty and already on top of the remote's version of the base. The engine then commits *once*, on top of an up-to-date base, and the push is an ordinary fast-forward. No lease, no force, no window where the branch is temporarily unwritable by anyone else.

`--autostash` is the load-bearing flag and it is a real risk surface of its own: if the pop conflicts, git leaves the stash in the stash list and the worktree needs manual attention. So `(e)` treats a failed pop as a **per-repo failure with a named recovery instruction** (`git stash list`, `git stash pop`) rather than proceeding to the engine on a half-popped tree. The alternative — `git stash` / `rebase` / `git stash pop` as three separate steps ro drives itself — is the same thing with three places to forget the cleanup, which is exactly the failure class the credential design avoids by never rewriting a remote in the first place.

After the rebase, the engine's own rebase-solve work is usually gone: the conflicts it would have resolved are conflicts ro already resolved mechanically before it was ever called. That is the point — a model call is expensive, and the most common reason a push fails is arithmetic, not judgement.

**The scan moves before the split, not after.** Today `main.rs:1216-1300` plans every repo in loop 1, prints, then applies in loop 2. Once an engine can mutate a worktree, a plan computed in loop 1 is stale by loop 2. Doing the full status scan once, up front, on the coordinator, removes the staleness without persisting a plan artifact — and it is what makes `base_branch` trustworthy. Step (e) reads the base from that snapshot rather than re-deriving it, so the branch the rebase targets is the branch the plan said it would. Today `main.rs:1216-1300` plans every repo in loop 1, prints, then applies in loop 2. Once an engine can mutate a worktree, a plan computed in loop 1 is stale by loop 2. Doing the full status scan once, up front, on the coordinator, removes the staleness without persisting a plan artifact — and it is what makes `base_branch` trustworthy.

**Per-repo independence is a value return, not a `Result`.** `ro_sync::sync::sync_repo` already models this correctly; `ro_sync::status::status_all` does **not** (it uses `?` on `status_repo` and aborts the whole fleet on one corrupt working copy at `crates/ro-sync/src/status.rs:90+`). Do not copy the second one.

**Roll forward, never auto-rollback.** `apply_repo` (`ro-sweep/src/commit_sweep.rs:444`) increments `failed` mid-loop and never rolls back, and there is no `reset_hard` call anywhere in the sweep path. Keep that shape, but make it explicit: a half-applied commit series is left in place and reported loudly, because `reset --hard` destroys work an agent may have done deliberately. An automated rollback across an agent-modified worktree is scarier than a partial commit.

### 6.4 The per-repo transaction, detail by detail

**Locking.** `ro_git::lock::RepoLock` (`crates/ro-git/src/lock.rs`) exists with **zero production call sites**, and `ro-sync/src/sync.rs`'s module doc claiming "acquire fs4 lock → clone → fetch → pull" is a lie. Three changes before it is wired:

1. New signature `RepoLock::acquire_at(lock_path: &Path, timeout_secs: u64)`. The current `acquire(repo_path, timeout)` derives `<repo_path>/.ro.lock` and has no way to be pointed at `<state_dir>/locks/`. `ro-git` must **not** grow a `ro-config` or `ro-state` edge to find the state dir — the orchestrator resolves it and passes the path down. `acquire(repo_path, …)` stays as a thin wrapper for tests.
2. `Drop` currently does `let _ = std::fs::remove_file(&self.lock_path)` (`lock.rs:84`). That is a textbook TOCTOU: process B can create the path after A's unlink and before A's unlock, and both then believe they hold the lock. **Leave the file; only unlock.** This breaks `lock_acquire_and_release` (`lock.rs:91-105`), which asserts `!path.exists()` after drop — update it to assert a second `acquire` succeeds immediately (proving the lock was released, not that the file vanished).
3. `acquire` calls `create_dir_all(repo_path)`, so a typo'd path gets created. Drop that side effect. And the lock file must live outside the worktree — `<repo>/.ro.lock` shows up as `?? .ro.lock` in `git status --porcelain`, which makes `is_dirty` return true and has checkpoint diagnosing its own lockfile as dirty work. **`<state_dir>/locks/<hex(repo_path_bytes)>.lock`**; hex of the raw path bytes is collision-free and needs no hash crate (which matters: `sha2` in `[workspace.dependencies]` is referenced by zero Rust files — the SHA256 in `install.sh`/`install.ps1` is shell/Powershell, and `checksum = "sha256"` in `Cargo.toml:101` is a cargo-dist string, not the crate).

**Conflict preflight.** `ro_git::conflict::detect` is a genuinely finished, well-tested subsystem (599 lines, `MarkResolvedError` with five variants) already wired to `ro conflict` in `main.rs`. Reuse it verbatim as the per-repo preflight. One fix first: `detect_op` (`conflict.rs:308-322`) probes only `MERGE_HEAD` / `REBASE_HEAD` / `CHERRY_PICK_HEAD` / `REVERT_HEAD` as **files** and never checks the `rebase-merge/` and `rebase-apply/` **directories**, which are the actual marker of an in-progress rebase. A paused rebase is currently invisible to `ro conflict` and would be silently clobbered. Also drop `ConflictedFile::stages` — it is hardcoded to `vec![]` at `conflict.rs:370` and its doc comment describes something that never happens.

**Dirty detection.** `git status --porcelain=v1 -z -uall`, keeping the NUL-safe rename/copy parse and the nested-repo trailing-`/` guard from `commit_sweep.rs:175-224`. `-uall` is what makes "untracked files are included" true, because it expands an untracked directory into individual files. Do **not** reuse `ro-sweep/src/commit.rs:91`, which runs porcelain *without* `-uall` and keeps only `path.is_file()` — every file inside a brand-new directory is silently dropped there.

**Safety preflight runs before the engine, and blocks.** This is a real bug being fixed, not a nicety: the fleet path today applies **only** the denylist. `commit_sweep.rs` never calls `quality_gates::run_all` and never calls `secret_scan::scan_files`, so a detected secret or a failing test suite goes straight into a commit and a push. Two more defects in the same area: `commit.rs:72` calls `should_block(SecretScanMode::Warn, …)` while `should_block` (`secret_scan.rs:190`) is `matches!(mode, Block) && !findings.is_empty()` — so `CommitOutcome::BlockedBySecrets` is **unreachable**; and `plan_repo` `continue`s over denylisted paths with no warning (`commit_sweep.rs:348`, `:365`), so a user's untracked `.env` vanishes and they get `NothingCommittable`. Denied paths go into `RepoOutcome::warnings` and are printed.

`quality_gates` stays **off by default**. `run_all` invokes `cargo test --workspace` over the whole tree, so an unrelated pre-existing failure in an untouched crate blocks a one-file checkpoint, and across a 20-repo fleet it is the dominant cost. It is opt-in via `[checkpoint].quality_gates = "on"`.

**Tracked but not cloned — now a legacy state, not a first-class one.** `resolve_multi_repo_targets` (main.rs:596-606) defaults `local_path` to `<state_dir>/projects/<owner>/<name>` whether or not it exists, and `status_repo` guards with an explicit `<local_path>/.git` check that the resolver does not. This state exists only because `ro repos add` used to record a row without cloning. Once `add` becomes clone-then-register (§3), **nothing new produces it**, so the right treatment is to treat it as corrupt data rather than teach the whole system a first-class mode: checkpoint **skips with reason `NotCloned` and prints the two ways out** — `ro repos add <spec>` to clone and register properly, or `ro repos remove <key>` to drop the row if the repo is gone for good. Checkpoint still never clones: a typo in a local path must not pull a full repository over the network in the middle of a commit-and-push loop. The `ro sync --clone-only` path stays for bulk backfill of old inventories.

**Empty repository.** `head_oid` returns `None`. There is no base branch to snapshot. Ask `gh repo view --json defaultBranchRef` — the same live source `ro pr` uses — and mark the outcome `Committed { base_inferred: true }` so the summary can say so. If there is no remote either, there is nothing to infer from: commit normally, skip the PR step, and say why.

**No remote.** Local commit only. `pushed: false`, counted as a success, and the summary notes it — a local-only checkpoint is the intended behaviour on a plane, not a failure.

### 6.5 Concurrency and SQLite

`rusqlite::Connection` is **not** `Sync` (it wraps `RefCell<InnerConnection>`). A `&Connection` cannot be shared with `std::thread::scope` workers; it fails to compile with E0277. The shape is therefore load-bearing, not an implementation detail:

- **One coordinator thread owns the `Connection`** for the entire run. It performs the target query, the step-3 scan, `open_run`, every `append_event`, and `finalize_run`.
- **Workers receive an owned `RepoTarget` and return an owned `RepoOutcome`.** They touch git and the filesystem. They never touch the database.
- Outcomes are drained **in target order**, not completion order, so `-j 1` and `-j 4` produce byte-identical summaries for the same input. This is testable and should be.
- The one-writer invariant is already assumed by `delete_repo_cascade`, so this introduces no new constraint.

WAL mode and `busy_timeout=5000` are already applied in `crates/ro-state/src/lib.rs:47-58`. They are what would make a per-worker connection viable if this shape is ever revisited; they are not needed for the coordinator design, which never has two writers.

**No async runtime in this path.** The git mutation layer stays synchronous. Agent engines are `std::process::Command` with a hard timeout and a killed child tree. The only genuinely async code in the workspace is octocrab, and section 6.6 removes it from this path entirely.

### 6.6 Transport: always `git push`, credential source decides

This is the single most important correction in the plan, so state it plainly.

**octocrab cannot push commits.** Its only git-data endpoint is `repos::create_ref`, which requires you to have already uploaded loose objects through the blobs/trees API. There is no push. So "gh transport vs git transport" is not a real distinction at the binary level — **every push is `git push`**, and `provider` selects where the *credential* comes from.

| | `[auth] https` credential | machine default (no `[auth]`) |
|---|---|---|
| Credential | HTTPS token, injected per-invocation | The repo's existing SSH key / credential manager |
| Mechanism | `git -c http.https://github.com/.extraheader="AUTHORIZATION: basic <base64(x-access-token:TOKEN)>" push …` | plain `git push` |
| Identity source | `gh api user --jq .login` | `git config user.email` |
| Reads the ambient SSH key? | no | yes |

**`GH_TOKEN` in the child environment does not authenticate `git push`.** Git never reads that variable. The env-injection seam in `RunOpts` is still necessary for the `gh` subprocess that does the identity lookup — and **only** for it: the agent engine's environment has the token *stripped*, see the subtraction section above. If the credential is not injected as an `extraheader` (or via a scoped credential helper, or `gh auth setup-git`), the gh-credentialed push silently falls back to whatever SSH key is configured — **which is the exact leak the vision calls a HARD RULE, arriving through a different door.** This is the failure mode most likely to ship looking green: every test that only asserts "a push was attempted" passes while the push went out over the wrong identity. At least one test (§8, #29) must assert the credential reached git.

**The no-fallback rule, restated over the credential source:**

```
provider == Https:
    token = GH_TOKEN -> GITHUB_TOKEN -> `gh auth token`      (one shot, no chain fallback)
    none  -> repo FAILS. No `git push` is attempted at all.
    if expected_login set and `gh api user` disagrees -> repo FAILS before push
    push with the extraheader credential
    on push failure:
        allow_fallback == true  -> loud stderr warning, retry as provider=git
        allow_fallback == false -> repo FAILS. No retry.

provider == Git:
    if author_email set and `git config user.email` disagrees -> repo FAILS before push
    push via the ambient SSH key / credential manager
```

The fallback warning names the repo, the requested provider, and the cause, e.g. `warning: repo_orchestrator: gh-credentialed push failed (403); retrying with the git credential — this may publish under a different identity`.

**PRs and identity shell out to `gh`.** `gh pr list --head <branch> --json number,state`, `gh pr create --draft --base <base_branch> --head <head_branch>`, `gh api user --jq .login`. In one move this removes: octocrab's async requirement (which contradicted the plan's own "no async runtime" bullet, and whose sync sketch signatures would not have compiled), the GHE `base_uri` bug at `auth.rs:100` (it builds `https://{host}` with no `/api/v3`, so Enterprise hits the web UI), and the `list_pulls` pagination gap (`issues.rs:116` is a single page with `per_page` defaulting to 50, so head_ref matching can miss a PR on a busy repo). It is also the smaller and more faithful design: the vision names `gh` as the preferred transport precisely because it works without `gh auth login` and takes `GH_TOKEN` per-process.

Consequence: **`ro-github` drops out of the checkpoint path entirely.** It stays in `crates/ro/Cargo.toml` because `ro doctor` still calls `ro_github::auth::discover_token` (`doctor.rs:25`, `:239`) — cutting `ro import` does **not** make the dependency removable, and an implementer who "tidies" it away breaks the build. Token discovery itself stays in `ro-github::auth` (the `gh auth token` shell-out already lives there and the binary already depends on it); only the *policy types* move (§6.7).

`AuthToken` currently derives `Debug`, so `{:?}` prints the raw token and `redact()` is opt-in and manual. Before any token starts flowing into child processes and argv, give it a manual `Debug` that delegates to `redact()`. `from_env` must check `GH_TOKEN` **before** `GITHUB_TOKEN` (gh CLI precedence) and use `var_os`, not `var` — the latter fails on non-UTF8 values. The `auto` and `config-token` strategies are deleted: `auto` is the silent fallback chain the vision forbids, and `config-token` can never succeed because `GitHubConfig` has no `token` field and `main.rs:1309` hardcodes `config_token: None`.

The identity guard runs **before** the push, never after. On mismatch the repo aborts with its local commits intact and its remote unmoved. The resolved login is cached **once per run** — 20 repos × one `/user` call each is 19 wasted network round-trips for the same answer.

### 6.7 Shared types live in `ro-core`

`AuthPolicy` was going to be defined in `ro-github::auth` *and* referenced from `ro-config::resolve::EffectiveConfig`, which would require `ro-config → ro-github` and `ro-github → ro-config` simultaneously. `ro-github` declares no `ro-config` dependency, and the existing edge runs the other way — which is precisely why `AppConfig.github.auth` is validated and then read by nothing.

All shared vocabulary is defined **once, in `ro-core`**, which is already a leaf depended on by `ro-config`, `ro-github`, and `ro-sweep`:

```rust
// crates/ro-core/src/auth.rs
pub enum AuthProvider { Https, Ssh, Machine }
pub struct AuthPolicy {
    pub provider: AuthProvider,   // derived from WHICH [auth] key is present, or Machine if none
    pub allow_fallback: bool,
    pub expected_login: Option<String>,   // verified when provider == Https
    pub author_email: Option<String>,     // verified when provider == Git
}
pub enum FailureClass { /* … */ }
impl FailureClass { pub fn classify(stderr: &str) -> Self; }
```

`ro-github` re-exports these for its own callers, and `ro-jobs` re-exports `FailureClass` for its existing API. This also kills the unwanted `ro-engine → ro-jobs → ro-state → rusqlite` edge that `EngineOutcome::Failed { class: FailureClass }` would otherwise have dragged into the engine crate for the sake of one discriminant.

**The two error taxonomies collapse to one, and the signatures are not drop-in equivalents.** `ro_git::mutation::GitErrorKind` (7 variants) takes only `stderr`; `ro_jobs::failure::classify` (10 variants) takes `(exit_code, stderr)` and adds rate-limit / quality-gate / secret-scan. The merge: one `FailureClass::classify(stderr)` in `ro-core`; ro-jobs keeps a thin `classify_exit(exit_code, stderr)` that consults the exit code first and delegates otherwise. `GitErrorKind::GitMissing` is unreachable today (nothing inspects spawn failure) — the new classifier checks the spawn error directly, so a missing `git` is finally distinguishable from a git that failed.

**One redaction rule, not three.** `ro_core::redaction::redact_secret` (first-N, and it byte-slices `&secret[..visible]`, which panics on a multi-byte boundary), `ro_github::auth::AuthToken::redact` (first-4/last-4, also byte-slices), and `ro_sweep::secret_scan::redact` (first-4, correctly `chars().take(4)`) are three incompatible policies, two of them panic-prone. The identity guard needs exactly one. Adopt first-4-plus-last-4 with a `chars()`-based implementation in `ro-core`, delete the other two, and have `secret_scan::SecretFinding.redacted` use it.

### 6.8 Exit codes

| Code | Meaning | Raised by |
|---|---|---|
| 0 | every targeted repo committed/skipped-clean | checkpoint aggregate |
| 1 | partial — at least one ok, at least one failed | checkpoint aggregate |
| 2 | every targeted repo failed | checkpoint aggregate |
| 64 | usage error | clap, remapped from its default 2 |
| 69 | a required binary is missing fleet-wide (`git` itself; `gh` when `provider=gh` and no token is obtainable at all) | fatal path |
| 70 | internal error — an unclassified `anyhow` escaping to `main` | fatal path |
| 78 | config could not be loaded or failed validation | fatal path |

**A fatal path is required, or the table is unenforceable.** `main()` today is `if let Err(err) = run() { eprintln!(…); std::process::exit(1) }` (`main.rs:483`), so any escaped `anyhow::bail!` — bad config, `open_db` failure, duplicate add — already exits 1, the same code the table assigns to "partial success". Introduce a typed `FatalError` with `fn exit_code(&self) -> i32`; `run()` returns `Result<(), FatalError>`. Per-repo errors are captured into `RepoOutcome::Failed` by construction (the worker converts `?` itself), so **any `Err` that reaches `main` is fatal by definition** — that invariant is what makes the two meanings separable.

Usage errors move from clap's 2 to 64. `main()` must own parsing rather than `run()`:

```rust
let matches = match Cli::command().try_get_matches() {   // needs clap::CommandFactory
    Ok(m)  => m,
    Err(e) => { let _ = e.print(); std::process::exit(if e.use_stderr() { 64 } else { 0 }); }
};
let cli = match Cli::from_arg_matches(&matches) { Ok(c) => c, Err(e) => e.exit() };
```

`--help` and `--version` arrive as errors with `use_stderr() == false` and correctly exit 0.

**Count push and PR outcomes, not just commits.** `main.rs:1301` exits 1 only when the *commit* failure count is non-zero; `outcome.push_error` is printed to stderr and never reaches the exit code, so a run where 8 of 8 repos fail to push exits 0 today. Any new policy must include them, and the `continue-on-error` flag is an exit-code/verbosity switch — the loop already always continues.

**Per-command codes are unchanged and documented as such**, so the 0/1/2 table is not misread as global:

- `ro doctor` → 0 all required checks pass, 1 at least one Fail, 2 internal. `report.exit_code()` at `main.rs:1324` stays. Its `Severity::Optional` provider probes can never change this code, and the docs should say so.
- `ro repos prune` → 0, plus the confirmation gate. The undocumented `std::process::exit(3)` at `main.rs:812` goes away.
- `ro schema`, `ro config` → 0, or 78 on a config error.

`--all` on an empty inventory is a legitimate 0. `ro checkpoint nonexistent` or a `--tag` that matches nothing is a **64** — a typo is a usage error, not an empty run.

### 6.9 Flags

The twelve the vision names:

| Flag | Notes |
|---|---|
| `--all` | every managed repo; the default when nothing else is given |
| *(positional)* | `ro checkpoint cass voice-ai-agent`; no args means the cwd repo. Each accepts `owner/name`, a bare name, or an alias. `--all` is the explicit fleet escape |
| `--tag <T>` | a real `repo_tags` row lookup, not `label.contains()` |
| `--engine <NAME>` | `claude\|codex\|git`; overrides `ro.local.toml` then `[engine] default` |
| `--message <MSG>` | presence means one commit with this subject (§9 decision 8) |
| `--direct` | commit onto the current branch |
| `--wip` | new branch + draft PR into the snapshot base |
| `--dry-run` | **the default**; scan-only, no engine dispatch |
| `--execute` | actually run the engines |
| `--continue-on-error` | exit-code/verbosity switch; the loop always continues |
| `--auth <gh\|git>` | overrides per-repo then global credential provider |
| `--allow-fallback` | permits gh→git with a loud warning; default off |

Additions, each with a reason:

| Flag | Why |
|---|---|
| `--filter <health:N>` | The surviving half of today's `--filter <EXPR>`. It existed as `tag:<substr>` or `health:<N>`; `tag:` is superseded by `--tag` (and was a lie — see §3), but `health:` is real: it is the one way to say "only the repos that actually need a human" across a 20-repo fleet. Works because the scorer survives the cut — the number just never renders. Accepts `health:critical`, `health:risky`, or `health:<N>`. |
| `--format <text\|json\|ndjson>` | Unifies two unrelated mechanisms that exist today: `--format text\|json\|toon` (a clap ValueEnum) and `--output json` (a free-form `String` where only the literal `"json"` is honoured, `main.rs:1075`). One enum, one value namespace. |
| `--no-push` | Commit locally without pushing. Keeps checkpoint usable on a plane, where `--dry-run` is useless (there is nothing to preview when you just want the commit). |
| `--include-archived` | The escape hatch for the `archived = 0 AND disabled = 0` filter (§6.3). Without it, archived repos become unreachable to checkpoint; with it, the safe default is still safe. |
| `--resolve` | On a **real** merge conflict, dispatch the engine a second time to resolve the conflicted paths. The engine edits and stages; ro runs `rebase --continue` and pushes. Off by default because it is the only step in the tool that lets a model touch files mid-rebase — the mechanical non-fast-forward fix happens regardless and never needs it. |
| `--yes` | Answer the "rebase and retry?" prompt affirmatively. For CI and for a fleet run where the repos are already yours. Does **not** imply `--resolve`: auto-rebasing is mechanical and safe to automate, auto-resolving a semantic conflict is not. |
| `-j <N>` | Bounded concurrency. Overrides `[core].parallel`, which becomes live. |
| `--timeout <SECS>` | Per-git-command and per-engine timeout. Overrides `[core].timeout_secs`, which becomes live. |
| `--engine-bin <PATH>` | Per-run engine binary override. Replaces the free-form `[engines.*]` table; valid only together with `--engine`. |

**One owner per knob.** `--timeout` and `-j` are not a second and third source of truth — they are the top tier of the single 3-tier resolver (`resolve_effective`) whose middle and bottom tiers are `ro.local.toml` and `config.toml`. Today the situation is the opposite: `ro sync --timeout` is a **hardcoded `30`** at the use site (`main.rs:739`, `timeout.unwrap_or(30)`), and `SyncOptions::timeout_secs` is then **never read anywhere** in `ro-sync/src/sync.rs` — it is a field and a `Default` impl and nothing else. `--timeout` is a no-op flag today, and `[core].parallel` (default 8) is never read by anything. Phase 3 makes both real; the resolver is the single place that decides.

**`--quiet` and `--verbose` are both deleted.** `cli.verbose` is never read anywhere in `main.rs`; `cli.quiet` has a dead `let _quiet = cli.quiet;` at `main.rs:623` and exactly one real read at `main.rs:704`, which is inside the `ro import` branch that Phase 1 removes. After Phase 1 both flags have zero readers. Note this is a **deprecation**, not a silent removal: keep them for one release printing a warning, then drop them.

**Not added:** `--pr-draft` (WIP PRs are *always* draft — a constant, not a knob), a `--json` shorthand (`--format json` is enough), a configurable WIP-branch-name template (see §9), and `--clone-missing` (§6.4).

**`--allow-protected` is deleted, not carried.** Its polarity inverts. Today it makes committing on `main`/`master` *possible*; the vision refuses direct mode on a protected branch and forces WIP. `is_protected_branch`'s current set is `[main, master, production, staging]` plus a `release/` prefix, matched case-sensitively by exact equality — so `Main` is not protected. Normalize to case-insensitive, keep at least `main`/`master`, and decide deliberately whether `production`/`staging` stay.

### 6.10 Edge cases

| Case | Behaviour |
|---|---|
| Clean repo | skip, counted as ok, no engine dispatched |
| Untracked files | **included** — `porcelain -z -uall` expands untracked directories into individual files |
| Untracked directory | its files are committed individually; the `commit.rs:91` bug is not reproduced |
| Tracked but not cloned (legacy rows only) | skip with reason `NotCloned`; print `ro repos add <spec>` or `ro repos remove <key>` as the two exits. Checkpoint never clones. |
| Inventory row whose directory was deleted | same as not-cloned, plus a `ro repos prune` hint |
| Protected branch + `--direct` | refuse, force WIP, say so on stderr |
| Protected branch + `--wip` | proceed |
| Neither `--direct` nor `--wip` | WIP on a protected branch, direct otherwise (and report which was chosen) |
| Empty repo (no commits) | commit normally; `base_branch` comes from `gh repo view --json defaultBranchRef` and is marked as inferred. No remote → commit only, no PR step, and say why |
| Repo mid-merge | skip, `SkipReason::Conflict` |
| Repo mid-**rebase** | skip — requires the `rebase-merge/`+`rebase-apply/` fix in Phase 3 or this is undetectable |
| Denylisted path present | excluded from the commit **and** reported in `RepoOutcome::warnings` |
| Secret detected | block that repo, name the file and rule (redacted), continue to the next repo |
| `quality_gates = "on"` and a gate fails | block that repo; off by default |
| No GitHub remote | local commit only, `pushed: false`, counted as ok |
| `[auth] https` ref unresolvable | fail the repo; **no `git push` attempted** |
| push fails, `allow_fallback = false` | fail the repo, no retry |
| push fails, `allow_fallback = true` | loud warning, retry over the machine default |
| Identity mismatch | abort before push; local commits intact, remote unmoved |
| Duplicate PR | `gh pr list --head <branch>` finds it; do not open a second; report `PR #N already open` |
| Engine binary missing | `EngineOutcome::Unavailable` naming the binary; per-repo failure; the rest of the fleet continues |
| Engine exceeds its timeout | `EngineOutcome::TimedOut`, child tree killed; distinct from `Failed` in the summary |
| Engine reports nothing to commit | `NothingToCommit`, counted as ok, no push attempted |
| `git` itself missing | 69, fleet-wide, before any repo is touched |
| Lock held by another process | that repo waits up to the timeout, then fails with `Locked`; the fleet continues |
| An explicit repo name matched nothing | 64, usage error |

### 6.11 Output

**NDJSON** is the streaming surface and it moves into the checkpoint module. `ro-output` is deleted in Phase 1 (§7), so the emitter is `crates/ro/src/checkpoint/emit.rs` and there is exactly one of it:

```rust
pub struct NdEvent { pub ts: String, pub v: u32, pub kind: &'static str,
                     #[serde(flatten)] pub payload: serde_json::Map<String, Value> }
```

Every line carries a real timestamp because the emitter owns it. The old bug — the production path in `main.rs:1086-1153` serialised `NdjsonEvent` with bare `serde_json::to_string`, and only `NdjsonWriter::write_event` ever filled `ts`, so **every emitted NDJSON line ended with `"ts": null`** — cannot recur when there is one emitter that is the only writer.

Kinds, redrawn for the fleet vocabulary: `run_start`, `repo_scanned`, `repo_skipped`, `lock_acquired`, `safety_blocked`, `branch_created`, `engine_dispatch`, `engine_timeout`, `committed`, `pushed`, `pr_opened`, `pr_reused`, `repo_failed`, `run_done`. The `v` field is a schema version — without one, a consumer cannot tell a renamed field from a removed one.

**Text output is hand-rolled `println!` per command.** That is what happens today (`ro status`, `ro sync`, and `ro list` all format inline), and it is the honest answer: no shared renderer is being introduced, because the only candidate (`ro_output::text`) is being deleted along with everything else. For the fleet summary specifically, which is the one place a real layout matters, specify it concretely so it does not drift:

```
ro/wip/feat-checkpoint-9f2  repo_orchestrator   claude   3 commits   pushed   PR #412 (reused)
                             ru                  git      1 commit    pushed   -
                             notes-app           codex   TIMEOUT     local
  3 repos · 2 committed · 1 failed · 1 skipped (clean) · 0 blocked
```

Fixed-width columns, branch truncated to 32 chars with `…`, no colour in the default path, warnings and failures on stderr. `--format json` emits a single summary object at the end (not one-object-per-line, which is what `ro list` does today and which forces a consumer to buffer and count).

---

## 7. Implementation phases

Five phases, each independently shippable and revertable. Ordering is driven by three hard edges: a dropped SQLite table breaks `delete_repo_cascade` at runtime with no compile error; the engine cannot be selected before per-repo config exists, and cannot call what it needs before the git primitives exist; and the old fleet-commit path must die atomically with the classifier that powers it.

### Phase 1 — Baseline green + safe cuts

**Goal:** CI green on all three OSes, ~2.5k LOC of off-vision surface gone, no behaviour a user depends on changes. Everything in the section-2 cut list executes here; this is the mechanical file list and the things the cut list cannot know about.

**Baseline repairs first** — the tree is red in three places today, and two of the three are disk-touching:

- `crates/ro-sync/src/prune.rs:135` — `if let Ok(p) = r` → `.flatten()`. CI runs `cargo clippy --all-targets --all-features -- -D warnings` on a 3-OS matrix, so **the clippy job fails on all three OSes right now**. It is a two-character fix in the same function the next item touches.
- `crates/ro-sync/src/manage.rs:286-293` — `resolve_local_path` builds with `format!("{}/{}/{}", …)`, producing forward slashes on Windows. `crates/ro-sync/src/prune.rs:149` compares that against `PathBuf::join` results (backslashes) as raw strings. On Windows **every tracked repo is reported as an orphan**. Frame the harm accurately: `handle_orphans` with `Delete` is gated at `main.rs:803-812` (requires `--non-interactive` or a TTY), so the realistic harm is a TTY user being shown a list that wrongly contains their own managed repos and typing `y` — not silent deletion. Still fix it first; the two red tests confirm the cause.
  **Acceptance criterion, not just a regression test:** switching to `PathBuf::join` fixes rows written *after* the change. Every repo already in an existing `state.db` keeps its forward-slash `local_path` forever. The **canonicalize-with-fallback comparison on the prune side is what saves existing databases** — without it, the fix converts a wrong list into *every* existing user's repos becoming orphans.
- `crates/ro-github/src/import.rs:29` — `auth::build_client` is called **before** `Runtime::new()` on line 31, and `Octocrab::builder().build()` panics outside a runtime. This is a **live production crash**: `ro import --stars` panics on any machine where a token is discoverable. It passes on CI only because `gh` is off PATH there, which is why it went unnoticed. Fix by moving the `bail!("no import source specified")` **before** the client is constructed and building the client **inside** `rt.block_on`. Note that adding `#[tokio::test]` does **not** fix it — `Runtime::new()` would then panic with "cannot start a runtime from within a runtime". The file is cut in this same phase; fix it first so the diagnosis is recorded, then delete it.

**Compile blockers the cut list does not cover.** Every one of these is an instant compile break:

- Four `lib.rs` module declarations: `crates/ro-sweep/src/lib.rs` (drop `pub mod policy_check`, `pub mod risk`, `pub use risk::{…}`), `crates/ro-config/src/lib.rs` (drop `pub mod policy`, `pub use policy::{…}`), `crates/ro-core/src/lib.rs` (drop `pub mod redaction`), `crates/ro-git/src/lib.rs` (drop `GitErrorKind` from the `pub use mutation::{…}` list).
- Six `ro-config` test sites break on the schema cuts: `schema.rs` test `defaults_match_plan_example` (13 assertions across `jobs`/`mcp`/`review`/`safety`), `schema.rs::round_trip_through_toml` (line 280, asserts `parsed.jobs.max_attempts`), `loader.rs::round_trip_load_then_load` (line 87, asserts `cfg.jobs.max_attempts == 3`), `validate.rs` production rules reading `cfg.jobs.*` and `cfg.safety.max_auto_apply_risk`, `validate.rs::invalid_layout_rejected` (writes `cfg.core.layout`), and the `paths.rs:231` assertion that `review` parses.
- `crates/ro-output/Cargo.toml` declares `ro-core`, `comfy-table`, `owo-colors`, and `console`, none referenced by any `.rs` file in the crate. Removing `comfy_table`/`console` from `[workspace.dependencies]` without editing this manifest is an immediate workspace-resolution error. `owo-colors` is also entirely unused and missing from the workspace-unused list.
- `crates/ro-sweep/Cargo.toml` declares `tempfile.workspace = true` in **both** `[dependencies]` and `[dev-dependencies]`.
- `ro-output` must be removed from **three** places, not two: root `Cargo.toml` members, root `[workspace.dependencies]`, and `crates/ro/Cargo.toml` — plus `crates/ro-sweep/Cargo.toml`, which also depends on it.

**`ro schema` lands in this same commit as `generate_robot_docs` is deleted.** The vision asks for "a single JSON API/schema surface for agents" as the *replacement*, and a four-phase absence of the machine-readable surface while six other surfaces change is the worst possible window. `clap::CommandFactory` is already a dependency; this is roughly thirty lines serializing the live command tree. Note the compatibility break explicitly: `ro schema` is a clap-tree dump, a genuinely different artifact from the hand-written JSON summary, so any agent parsing `ro robot-docs commands` breaks. That goes in the release note, not just the commit message.

**Manifests:** root `Cargo.toml:14` (ro-dep-update member), `:41` (its workspace dep), `:12` and `:39` (ro-review), plus `ro-output`'s three sites. `deny.toml` `[advisories] ignore` carries `RUSTSEC-2024-0436` (via `rmcp`, deleted in `5fdece9`) and `RUSTSEC-2025-0119` (via `indicatif`, removed from ro-sync here) — `audit.yml` runs `cargo deny` on every push. `.beads/config.yaml` is entirely commented out with `issue_prefix: repo_forge`; uncomment and set it to `ro` **now**, before any of this work is filed, or the new issues inherit the legacy prefix.

**Depends on:** nothing.
**Verified by:** `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --workspace --no-fail-fast` all green on Windows, macOS, and Linux. `ro remove` and `ro prune` still work against a **v3** database, proving the V4 migration path. `ro sweep commit-sweep --all` still works, proving nothing it depends on was cut.
**Done looks like:** five modules, ten top-level commands, **four** migrations (V1/V2/V3 plus the new V4), `ro schema` live, no agent-facing surface missing.

### Phase 2 — Config foundation

**Goal:** per-repo config exists, precedence is real and enforced, `ro init` is the onboarding verb and stays fast, `ro repos` has a working update verb, and the config file stops lying — in both directions.

**The `resolve_paths` bug goes first.** `crates/ro/src/main.rs:466-478` matches on `(&config_dir, &state_dir)` as a **tuple** and honours overrides only in the `(Some, Some)` arm, so `ro --config-dir /tmp/c status` silently uses the real `~/.config`. Every E2E test happens to pass both, which is why it is untested. Make it per-field. The entire 3-tier precedence design is meaningless until this is fixed.

**New:** `RepoConfig` as **three nested tables** — `identity: { name, email }`, `auth: { provider, credential, expected_login }`, `agent: { engine, mode }` — each with `#[serde(deny_unknown_fields)]`, and **no** `token` and **no** `allow_fallback` (see the six rules in §4). `load_repo_config(repo_root)` where an absent file yields defaults, not an error; `pub const REPO_LOCAL_FILE: &str = "ro.local.toml"`; `resolve_effective(repo_root, &CliOverrides) -> EffectiveConfig`. `EffectiveConfig` carries only engine/auth/identity/mode/checkpoint settings — **not** `ConfigPaths`. Conflating "what the user configured" with "where things live on disk" forces every consumer to destructure a field it ignores; pass `ConfigPaths` separately to the two functions that need it.

**`deny_unknown_fields`, not `#[serde(flatten)]`.** They are mutually exclusive in serde, and the plan's own test requires the strict behaviour. The `#[serde(flatten)] extra` precedent at `ro-config/src/policy.rs:38-39` is a *different* semantic (tolerate and warn, do not fail) and was cited for the wrong thing. No struct in `ro-config` carries `deny_unknown_fields` today, so `authent = "gh"` parses cleanly, does nothing, and `ro doctor` still reports "config valid" — a half-implemented `ro.local.toml` fails **silently**. Strictness goes on `RepoConfig` and on the three fixed engine slots.

**Do not add `deny_unknown_fields` to `AppConfig`.** Unknown tables are currently ignored, which is the only reason an existing user's `[mcp]`/`[jobs]`/`[review]` config still loads. Adding it would make every existing config unloadable and flip `ro doctor` to "invalid config". That omission is deliberate and belongs in a comment on the struct, because the next person will otherwise "fix" it.

**Config back-compat, explicitly.** Renaming `[providers]` to three fixed `[engine_*]` slots is the plan introducing a fresh instance of the very silent-config-failure risk it flags two sections earlier. Unknown tables are ignored, so an existing `config.toml` would parse and **silently lose its engine configuration**. Ship a small compat shim: on load, read `[providers.claude]` and `[providers.codex]` and map them into the new slots when the new keys are absent; write the new form on the next `ro init`/`config set`. Loudly, not silently.

**`ro doctor --fix` must be able to upgrade an existing config, or it never will.** `check_and_optionally_fix_config` writes `default_config_toml()` only when the file is **absent**, so after this phase every existing user keeps dead `[mcp]`/`[jobs]`/`[review]` config indefinitely and `validate()` keeps checking keys the schema no longer models. With `toml_edit` available, `--fix` should add the missing `[engine]`/`[auth]`/`[checkpoint]` sections, leave unknown legacy tables in place (no deletion), and print a note naming the ignored sections. Two related defects: `doctor.rs:200` discards the applied-fix count via `let _ = applied_fix_count;`, and the module doc's claim that every mutation "backs up the prior state to `<state_dir>/doctor/runs/<run-id>/`" is **false** — no backup code exists in `doctor.rs`. Delete the claim. And `doctor.rs:249` tells users to "set `[github].token` in config.toml" — there is no `token` field on `GitHubConfig`; fix the string.

**`ro config set` uses `toml_edit`, not `toml::to_string_pretty`.** Today it loads, mutates one of seven hardcoded keys, re-validates, and `fs::write`s the whole `AppConfig` — silently dropping every comment and every key the schema does not model, on every invocation. A surgical in-place edit fixes the loss structurally, which means **do not also add a `.bak` written on every call**; that is just accumulating noise. The `.bak` belongs on the one genuinely destructive operation (the V4 table drop, and the `config.toml` rewrite during upgrade), not on every key assignment.

**`ro init` rewrite — one verb, three modes.** Full semantics in §3; the implementation shape:

- Extract mode selection into a small pure function so it is testable without touching a filesystem: `fn init_mode(args: &InitArgs, cwd: &Path) -> InitMode` returning `Global | OnboardRepo | Workspace(PathBuf)`. The rule is `args.add_dir` → `Workspace`, else `cwd.join(".git").exists()` → `OnboardRepo`, else `Global`. **No directory walk happens inside this function**, which is what keeps the "no implicit scan" guarantee checkable in a unit test.
- `Global`: write the default config if absent, print the path and the override hint.
- `OnboardRepo`: `Global`, plus write `ro.local.toml`, append it to `.gitignore` idempotently, and `manage::add` the repo. The inventory key comes from the `origin` remote; if there is no remote, fall back to a directory-name key and say so — a repo with no remote is a real case (a fresh `git init`).
- `Workspace`: reuse the `collect_git_dirs` walker that Phase 1 already made `pub`. Print the found set, confirm in a TTY, then per repo run the `OnboardRepo` body. `--non-interactive` skips the prompt and registers all. Report a per-repo outcome line so a partial failure is visible.
- No PATH scan, no wizard, no binary validation in **any** mode — missing-engine errors surface at `ro checkpoint` time. Dumb about engines, competent about repos.

**`ro repos update` — new verb.** `pub fn update(conn: &Connection, key: &RepoKey, patch: &RepoPatch) -> Result<Repo>` in `ro_sync::manage`, a targeted `UPDATE repos SET … WHERE host = ? AND owner = ? AND name = ?`. No schema change. The handler enforces exactly one of `--name`/`--owner`/`--alias`/`--branch`/`--archive`/`--unarchive` per invocation, and refuses a rename that would collide with an existing `(host, owner, name)`. **No `--remote`**: there is no remote column and no remotes table anywhere in the schema, and adding one would duplicate git's own state. **No `--default-branch`** either — that column is dropped in V4, because `git symbolic-ref` and `gh repo view` both answer the question live.

**`ro repos add` becomes clone-then-register, and remote-only.** Clone into the resolved local path first; insert the row only on success, so a failed clone leaves no inventory row. This removes the "tracked but not cloned" state from the data model rather than making the rest of the system tolerate it. The argument is a **remote spec only** — no local paths. `RepoSpec::parse` already rejects `.` and `C:\path`, but it silently accepts `C:/work/backend` as `owner="C:"`, so add the drive-letter and backslash rejections in `ro-core/src/repo_spec.rs` (with tests) and make the handler's error for a path-shaped argument name `ro init` as the right verb.

**`ro repos doctor`, `tag`/`untag`/`tags`, and `ro run prune` — the remaining CRUD holes, same phase.** All four are single-statement queries against tables that already exist, so none of them needs a schema change:

- `pub fn doctor(conn, &Repo) -> Vec<Drift>` in `ro_sync::manage`, four checks (missing / not-a-repo / remote-mismatch / not-cloned), read-only, no `--fix`. It **consolidates** three ad-hoc partial answers that already exist — `status_repo`'s `<local_path>/.git` guard, the checkpoint `NotCloned` reason, and `prune --missing` — into one command. Nothing it computes is new; having it in three places is the problem.
- `tag` / `untag` / `tags` against `repo_tags` (`PRIMARY KEY (repo_id, tag)`, indexed on `tag`): `INSERT OR IGNORE`, `DELETE … WHERE repo_id = ? AND tag = ?`, `SELECT tag … ORDER BY tag`. All three take multiple tags where it makes sense; `untag` on a missing tag is a no-op at exit 0.
- `pub fn prune_runs(conn, keep: usize)` in `ro-jobs`: one `DELETE … WHERE run_id IN (SELECT id FROM runs ORDER BY started_at DESC LIMIT -1 OFFSET ?)` fanned across `runs`, `run_events`, `sync_results`, `failures`. `--keep` is **required** — no default, no time-based expiry, no size cap. Report `{deleted, kept, oldest_kept}` in `--format json` so a script can log what it destroyed. This deletes rows, not tables, and is unrelated to `V4_DROP_PLANS`.

**`allow_fallback` moves to global-only.** Remove it from `RepoConfig` entirely. It is a safety posture, and a per-repo posture means one fleet run can hold two of them — which is exactly the state where you push one repo as the wrong account and cannot afterwards say which. One run, one policy. The per-run `--allow-fallback` flag stays as the visible override. `deny_unknown_fields` then turns a stale `allow_fallback` in someone's `ro.local.toml` into a loud error naming the right file, which is the correct outcome.

**Depends on:** Phase 1.
**Verified by:** `resolve_effective` unit tests covering all three tiers and a conflict; a `deny_unknown_fields` test proving `authent = "gh"` errors; a `resolve_paths` test proving `--config-dir` alone is honoured; `init_mode` table tests for all four argument shapes including "no args inside a git repo → `OnboardRepo`" and "no args outside one → `Global`"; an `ro init` idempotency test proving two runs add exactly one `.gitignore` line and that a pre-existing `/ro.local.toml` entry is not duplicated; a `--add-dir` test over a fixture tree with a nested non-repo directory proving the nested dir is skipped; a `ro repos update` test proving a colliding rename is refused and that `--archive` flips only the `archived` column; an `ro repos add` test proving a failed clone writes no row; a compat test proving a legacy `[providers.claude]` config still resolves `engine = claude`; a `toml_edit` test proving a hand-written comment survives `ro config set`.
**Done looks like:** three tiers resolve correctly, no user-visible setting is silently ignored, a legacy config keeps working, and a brand-new user can go from nothing to a registered repo with a per-repo config in two commands (`ro init`, then `cd repo && ro init`) or one (`ro init --add-dir .`).

### Phase 3 — Git, credential and safety substrate

**Goal:** every primitive checkpoint will call exists, the credential design is buildable, and nothing blocks forever.

**`run_in` gains a parameter; it does not lose one.** It is a **private** `fn run_in(cwd: Option<&Path>, args: &[&str])` at `crates/ro-git/src/mutation.rs:159`, and the `Option` is load-bearing: `clone()` calls it with `cwd: None` at `mutation.rs:262` because git must run outside the destination repository. Dropping the `Option` to `&Path` deletes the only way to spawn git without a working directory.

```rust
pub struct RunOpts<'a> {
    pub env: &'a [(String, String)],
    pub timeout: Option<Duration>,
}
fn run_in(cwd: Option<&Path>, args: &[&str], opts: &RunOpts<'_>) -> Result<GitCommandResult>;
```

Keep the existing two-argument `run()` as a thin shim over the default `RunOpts`. Four non-test callers in `ro-sweep/src/commit_sweep.rs` (lines 204, 214, 436, 505) plus twelve test call sites would otherwise all churn for no benefit; deprecate the shim and delete it when the sweep namespace goes in Phase 4.

Preserve the arg-vector discipline exactly (`--` separators, no shell interpolation, `--no-pager`) and the hardening block. Add a **real** timeout with child-kill: `Command::output()` blocks forever, and `mutation.rs:17-18` explicitly defers timeout enforcement to "the daemon", which is being cut. A hung SSH credential prompt currently stalls a whole fleet indefinitely, and the declared `tokio` dependency has zero references — the crate is fully synchronous.

**Add:** `create_branch`, `checkout`, `stage_all` / `commit_all` (the `git add -A` path the typed API currently refuses), `diff` (worktree + staged), `branch_exists`, `list_branches`, `stash`, standalone `merge`/`rebase` (`pull` currently conflates fetch+pull), `remote_url`, `git_config_user_email`. `mutation::commit` refuses an empty file list and explicitly forbids `add -A`, which is why WIP-style "commit everything" is impossible through the typed API today and why `commit.rs:110-140` hand-rolls it in violation of ro-git's own rule.

**Fix:** `read::ahead_behind` returns `AheadBehind::ZERO` on **any** git failure (`read.rs:111-112`), conflating "I do not know" with "you are in sync" — a typo'd upstream displays as up-to-date. Make the error distinguishable. `read::has_remote` and `read::merged_branches` swallow every error to a default; a broken repo is indistinguishable from a clean one. `detect_op` gains the `rebase-merge/`+`rebase-apply/` directory probe (§6.4). `lock.rs` takes `acquire_at`, stops unlinking on drop, drops the `create_dir_all` side effect, and `lock_acquire_and_release` is updated (it currently asserts the file is gone after drop).

**Credential layer:** `GH_TOKEN` before `GITHUB_TOKEN` in `from_env`, `var_os` not `var`, delete `auto` and `config-token`, wire `AppConfig`'s auth into the strategy argument, manual `Debug` on `AuthToken` delegating to `redact()`, fix the GHE `/api/v3` suffix. `ro-core::auth` gets `AuthProvider` / `AuthPolicy` / `FailureClass`; `ro-github` and `ro-jobs` re-export.

**Safety net:** wire the real `SecretScanMode` (the `should_block(Warn, …)` call at `commit.rs:72` is always false, making `BlockedBySecrets` unreachable), report denylist hits as warnings instead of dropping them silently, and make `quality_gates` opt-in.

**Depends on:** Phase 2 (for the `[auth]` / `[engine]` keys).
**Verified by:** ro-git tests for `stage_all` including files inside a brand-new untracked directory, `create_branch`, `ahead_behind` on a bad upstream, and `conflict::detect` on a paused rebase; a `run_in` test proving an injected env var reaches the child; a lock test proving a second acquire succeeds after drop; an auth test proving `GH_TOKEN` wins over `GITHUB_TOKEN`; a `unresolvable-credential test proving **no** `git push` was attempted.
**Done looks like:** a hung git or a missing `gh` fails fast and per-repo, a token can be injected as a real credential, a paused rebase is detected, and the safety net actually blocks a secret.

### Phase 4 — `ro-engine` + the atomic sweep cut

**Goal:** the core reversal lands, and the command it replaces dies in the same commit that guts it.

**The sweep namespace is cut HERE, not in Phase 5.** `Bucket::classify` / `is_test` / `is_doc` / `is_config` / `prefix` / `scope_of` / `task_id_of` and the conventional-commit template are the **only** producer of `PlannedCommit { bucket, message, files }` — `commit_sweep.rs:372` assigns the bucket and `:382` builds the message. Delete them and `plan_repo` cannot construct a plan, leaving `ro sweep commit-sweep` compiled but semantically hollow while Phase 1j keeps it alive. The old path dies atomically with the code that powered it, and Phase 4's verification must run the CLI, not just unit tests.

Same commit: delete `SweepCommands` (`main.rs:307-378`), the three handler arms (`:1043-1304`), `AutoApproveLevel` (`:37-43`), `commit.rs`, `agent.rs` (a module named "agent" that spawns no agent and never sets `applied = true`), `risk.rs` and `policy_check.rs` (zero CLI callers). **Keep** the NUL-safe `parse_porcelain`, the `-uall` decision plus the nested-repo trailing-`/` guard, `is_protected_branch`, `denylist.rs` verbatim, `secret_scan.rs` with its unreachable-branch fix, and `quality_gates.rs` as opt-in. Those move into `ro-engine` (`scan`, `git_engine`) and stay in `ro-sweep` (the safety net). The crate keeps its name; renaming it to `ro-safety` is churn that buys nothing and breaks the workspace path in a dozen places.

`ro-sweep/Cargo.toml` drops `ro-core`, `ro-jobs`, `ro-output`, `tokio`, `comfy-table`, `ro-config`, `serde_yaml`, `ro-state`, `rusqlite`, and the duplicate `tempfile`.

**`ro doctor` is left alone.** The draft contradicted itself by saying availability is checked at dispatch only and, two paragraphs later, by adding a `gh` probe to doctor. **Do not add probes.** `check_provider` is already `Severity::Optional` (`doctor.rs:372`), so it can only ever warn and cannot change `DoctorReport::exit_code()` — the docs should say that. If the `gh` probe is wanted later, it is a separate, small PR. Two tests need comment updates, not behaviour changes: `doctor.rs:562` `run_returns_at_least_six_checks` asserts `>= 6` (still true) with an inline comment listing the six checks, and `doctor.rs:536` asserts `Severity::Optional` (still true).

**Depends on:** Phases 2 and 3.
**Verified by:** unit tests per engine against a temp repo — `GitEngine` on a dirty repo with a brand-new directory produces one commit containing that directory's files; a missing `claude` returns `Unavailable` without a non-zero process exit; a registry-free dispatch table test proves a fourth engine is one `impl` plus one match arm. **CLI smoke test: `ro sweep` is gone, `ro --help` lists the intended surface.** A `--filter has:x` no longer matches every repo. An invalid `--repos` glob errors instead of silently selecting the fleet.
**Done looks like:** conventional-commit bucketing is gone from the tree, three engines implement one trait, a missing `claude` surfaces as a per-repo error at dispatch, and there is exactly one fleet-commit path left in the codebase — the one about to be registered.

### Phase 5 — `ro checkpoint`, `ro pr`, `ro conflict`, the regroup, docs, tests

**Goal:** the three verbs that replace the daily Git workflow ship, and the surface matches the vision.

**The three verbs and why they are all v1.** The daily loop is `ro status` → `ro checkpoint` → occasionally `ro pr` → occasionally `ro conflict`. Each replaces a specific multi-terminal sequence, and cutting any one of them leaves a hole the user falls back to a terminal for — which is the outcome this whole plan exists to prevent. `ro pr` matters most often (most PRs are for branches that are already committed) and `ro conflict` matters most painfully (a wedged repo mid-rebase blocks everything else). Neither is a nice-to-have.

**`ro pr` — thin, and sharing its internals.** New: `crates/ro/src/pr.rs`. It calls the same three helpers `checkpoint --wip` uses — `ensure_branch_pushed`, `find_open_pr_for_head`, `create_or_update_pr` — and adds only argument parsing, the dirty-tree precondition check, and output. If those helpers are not shared, `ro pr` and `checkpoint --wip` will drift within a month and one of them will push a branch the other cannot find. The dirty-tree check is mandatory: `ro pr` refuses when the worktree is dirty, because a PR is a statement about a specific commit and silently sweeping uncommitted work into a branch you did not just push is how WIP ends up in a reviewed diff.

**`ro conflict` — detection plus the assistant.** `conflict list` is what the plan already had. The new part is `ro conflict <REPO>` (fetch → rebase/merge → on conflict show paths and offer mergetool / abort) and `ro conflict <REPO> --continue`, per §3. Two invariants in the implementation: `--continue` **verifies the index is free of unmerged entries before** running `rebase --continue` (running it with conflicts still present produces a second, more confusing failure and the user learns to avoid the command), and `abort` **records the pre-operation HEAD in the run row first**, then aborts, then verifies the ref matches — a tool that aborts a rebase has already thrown away commits unless the ref was captured first. It is one repo at a time, never a fleet loop: the same rule as checkpoint, because an interactive command over 20 repos is not a command.

**Repo selection is positional, and the resolver is shared.** `ro checkpoint cass voice-ai-agent`, `ro sync cass`, `ro status backend`. One `resolve_targets(&[String]) -> Result<Vec<Repo>>` in `ro-sync::targets` accepts a bare name, an alias, or `owner/name`, and is used by all three verbs so they cannot disagree about what a name means. `--all` stays as the explicit fleet escape.

**New:** `crates/ro/src/checkpoint/{mod,orchestrator,emit,summary}.rs` per §6. `crates/ro-sync/src/targets.rs` — `resolve_multi_repo_targets` lifted from `main.rs:554-611` with the `health:<N>` branch deleted, the `has:` bool-expression bug fixed, the invalid-glob-becomes-`*` bug fixed, `archived = 0 AND disabled = 0` added, and a real `repo_tags` reader.

**`collect_git_dirs` must be made public.** It is a **private** `fn collect_git_dirs(dir, depth, out)` at `ro-sync/src/prune.rs:163`, with `const MAX_DEPTH: usize = 4` at `:164`, and `ro-sync/src/lib.rs` has no re-export. The only public entry point is `find_orphans`, which is coupled to SQLite and returns `Vec<Orphan>` after the tracked-path comparison — routing `ro repos scan` through it would be routing a read-only discovery command through the database and through the exact code being repaired. Make the walker `pub` (or move it to `ro-git::discover_dirs`) and turn `MAX_DEPTH` into a parameter so `ro repos scan --depth` is real.

**CLI:** register `Commands::Checkpoint` (positional repos), `Commands::Pr`, and the reworked `Commands::Conflict`; regroup `add`/`remove`/`list`/`prune` under `ro repos`, with `scan`/`update`/`doctor`/`tag`/`untag`/`tags` added; add clap `alias`/`visible_alias` for the four old flat names for **one release**, printing a deprecation line naming the removal version. There are currently **zero aliases anywhere in the clap tree**, so nothing is in the way, and this is a bigger break than the exit-code redefinition. Consolidate the two duplicate `OutputFormat` enums (`main.rs:22` has 3 variants, `ro-output/src/lib.rs:25` had 4, and the CLI used neither) into one. Delete `--quiet` and `--verbose` after their deprecation release.

**Exit codes:** the `FatalError` type, the 64 remap, the per-command table (§6.8).

**Run history:** wire `open_run` at command entry, `append_event` per per-repo outcome, `finalize_run` with the aggregate code. All three have **zero production callers** today — `ro_jobs::open_run` (`run.rs:54`), `finalize_run`, and `append_event` are called only from their own `#[cfg(test)]` modules — which is why `ro run list` always prints nothing against a fresh database and why `FEATURES.md:212`'s "every mutating operation records a run" is false.

**Fix the sync FK bug while there.** `sync_all` mints `run_id = Uuid::new_v4()` at `ro-sync/src/sync.rs:306` and never inserts a `runs` row, but `sync_results.run_id` is `NOT NULL REFERENCES runs(id)` and `ro_state::open_db` sets `PRAGMA foreign_keys=ON`. The insert fails and is swallowed by `let _ = conn.execute(…)` at `sync.rs:326`. So `sync_results` is **permanently empty**, `ro status`'s `last_synced_at` is permanently NULL, and the health score's failed-sync penalty is permanently zero. Thread a real `open_run` id through. Alongside it: a **failed pull is recorded as success** — the pull branch never checks `outcome.result.ok()` (the clone branch immediately above it does), and `PullOutcome.conflict` is computed and then discarded, so a conflicted `--rebase` pull lands in `sync_results` as `updated` / `success` / `error: None`. Add the `archived`/`disabled` filter to `sync_all` too, wire `-j` and `--timeout` to real executors, and delete `--resume` (there is no daemon to resume into).

**Tests:** rewrite all five tests in `crates/ro/tests/cli_init.rs` (every one hardcodes the flat `ro init` / `ro add` / `ro list --format json` / `ro remove` / `ro import` surface) and rename the still-`RfoTest` test struct.

**Docs:** full `README.md` and `FEATURES.md` rewrite against reality. Fix at minimum: `ro review plan --format json` (no such flag; `repo` is a required positional), the `ro fork sync` flags that never existed, the `ro robot-docs all` topic that never existed, the run-recording claim, the `--parallel`/`--resume`/`--verbose` no-ops. **Preserve `README.md:415-417`** (the no-outside-contributions policy) and the `ro-illustration.webp` reference. `install.ps1:138` claims config lives at `%LOCALAPPDATA%\ro\config.toml`; `ro init` actually writes it to `%APPDATA%` (Roaming) via `dirs::config_dir()` and `state.db` to `%LOCALAPPDATA%`. `install.sh:234-236` warns "set `RO_FORCE=0` to refuse" when `0` is already the default and the install is unconditional.

**Beads:** file the five phases as issues with the Phase 2 → 3 → 4 → 5 dependencies recorded, now that `issue_prefix` is set to `ro`.

**Depends on:** Phase 4.
**Verified by:** the §8 suite; `ro run list` returning a real row after a `ro checkpoint --dry-run`; a live 3-repo fleet run with one deliberately failing repo producing exit 1.
**Done looks like:** `init`, `repos`, `status`, `sync`, `checkpoint`, `conflict`, `run`, `doctor`, `config`, `schema` — ten commands, no review, no `ro health` command, no fork, no sweep, no import, no self-update, no robot-docs, no toon, and every documented claim true. The health *scorer* survives and stays filterable; only the command and the display are gone.

---

## 8. Testing & verification

**Starting position:** one integration test file in the entire workspace. `crates/ro/tests/cli_init.rs` has five tests, and every one breaks on the `ro repos` regroup. Everything else is crate-internal unit tests. Thirteen of those (ro-review) plus seven (health) plus four (toon) die in Phase 1 — and the draft's count of "13 more" in `ro-output` is right: deleting `OutputFormat`/`parse_output_format`/`render` also kills `parse_all_formats`, `display_round_trip`, `render_json`, `render_ndjson`, `render_text`, two in `json.rs`, and six in `format.rs`. Of `ro-output`'s 25 tests, about five survive. **The two red ro-sync prune tests are fixed, not deleted.**

### Fixtures

```rust
struct Fleet { root: TempDir, db: PathBuf, repos: Vec<FixRepo> }
struct FixRepo { id, owner, name, path: PathBuf, remote: Option<PathBuf>,
                 branch: String, remote_git: bool }
```

Each repo is configurable: branch (default `main`), clean or dirty, dirty files tracked / untracked / inside a brand-new directory, with or without a remote, with or without an `ro.local.toml`, with a chosen engine.

**Faking `gh` (and the agent engines) — cross-platform, no new crate.** A shell script is not directly executable on Windows. Write a **`gh.cmd` shim** on Windows and a `gh` shell script elsewhere, prepend the directory to `PATH`, and have the shim append its argv to a log file the test asserts on. Windows resolves `.cmd` through `PATHEXT` for `Command::new("gh")`, so `gh pr list --head X` reaches the shim with no shim-compilation step and no `required-features` bin to accidentally ship. The same mechanism fakes `claude` and `codex` with a script that runs `git add -A && git commit -m shim` — deterministic, cross-platform, and it never spawns a real agent in CI. This is the test seam; it is why the `ro-github` octocrab path does not need a `base_uri` override or `wiremock` at all.

If any GitHub call *does* survive elsewhere, it needs a `base_uri` parameter on `ro_github::auth::build_client` (it hardcodes `https://{host}`) and `wiremock` as a dev-dependency of the crate that needs it — the existing `mock_client` helpers in `issues.rs:172` and `client.rs:124` are `#[cfg(test)]`-private.

### Required tests

**Dry-run vs execute**
1. `checkpoint --all --dry-run` on a dirty fleet: exit 0, no commit in any repo, no branch created, no lock file left, no engine dispatched. **The single most important test** — the two-phase plan/apply split must not leak.
2. `checkpoint --all --execute` on the same fleet: one commit per repo.
3. `--dry-run` then `--execute`: the printed plan and the real outcome agree, because both read the same step-3 snapshot.

**Direct vs WIP**
4. Feature branch + `--direct --execute`: commit lands on the feature branch, no new branch, no PR.
5. `main` + `--direct --execute`: **refuses**, creates `ro/wip/*`, commits there, and says why on stderr. The polarity inversion is the point; test both directions.
6. Feature branch + `--wip --execute`: `ro/wip/*` created and checked out, pushed with `--set-upstream`, PR opened with `--head ro/wip/*` and **`--base` equal to the branch that was checked out before the WIP branch was created**. An engine that switches branches mid-run must not move the base.
7. Duplicate PR: run `--wip --execute` twice against the fake `gh`; the second run's shim log shows a `pr list` hit and **no** `pr create`. Assert the outcome reports `pr_reused`.

**Per-repo isolation and exit codes**
8. Fleet of 3, one repo mid-merge: skipped with a `Conflict` reason, the other two commit, exit 1.
9. Fleet of 3, one repo mid-**rebase** (a real `rebase-merge/` directory): skipped. This test is the regression for the `detect_op` gap.
10. Fleet of 3, one engine invocation exits non-zero: exit 1.
11. Fleet of 3, all fail: exit **2**.
12. Fleet of 3, all ok: exit **0**.
13. Fleet of 3, all clean: exit 0, no engine dispatched, no commit.
14. **Every commit succeeds and every push fails → not 0.** This is the current bug at `main.rs:1301`.
15. Bad usage → exit **64**, not 2. `ro checkpoint --nonexistent-flag`.
16. `ro checkpoint nonexistent` → exit **64**; `ro checkpoint --all` on an empty inventory → exit **0**.

**Target resolution**
17. `ro checkpoint a/b` selects exactly one repo; an unarchived, enabled filter applies by default.
18. A repo marked `archived = 1` is **not** selected by `--all`; it **is** selected with `--include-archived`. Same for `disabled = 1`. This is the fleet-correctness regression.
19. A tracked-but-not-cloned repo is skipped with `NotCloned`, no network access, and the outcome names the `ro sync --clone-only` command.
20. `--tag foo` selects by real `repo_tags` rows. Today `--filter tag:orch` matches any repo whose owner or name contains "orch".
21. An **invalid** glob errors instead of silently selecting the whole fleet (today it becomes `Glob::new("*")`).

**Untracked, denylist, secrets**
22. A brand-new directory with files inside is fully committed — the `-uall` behaviour, not `commit.rs:91`'s.
23. A dirty `.env` is excluded from the commit **and** appears in the warnings output.
24. A file containing a live-looking GitHub PAT **blocks** that repo and names the rule (redacted). The `should_block(Warn, …)` regression.
25. `quality_gates = "off"` by default: a repo with a failing suite still checkpoints. `"on"` blocks it.

**Credential and identity**
26. `[auth] https = "env:ABSENT"` with the variable unset, `allow_fallback = false`: the repo fails, stderr names the repo, and **no `git push` reached the remote** (assert the bare origin's reflog is unchanged).
27. Same, `allow_fallback = true`: the push happens over the git credential and stderr contains the loud warning naming the repo and the cause.
28. **`GH_TOKEN` beats `GITHUB_TOKEN`** when both are set.
29. **The credential actually reaches git.** With an `[auth] https` credential, assert the recorded git invocation carries the `http.https://github.com/.extraheader=…AUTHORIZATION: basic …` config, or (black-box variant) that `gh auth token` was consulted. A test that only asserts "a push was attempted" passes while the push went out over the wrong SSH key — that is the failure mode this whole section exists to prevent.
30. `expected_login = "someone-else"`: the repo aborts **before** any push; the local commit exists; the remote did not move.
31. `expected_login` mismatch under a named `ssh` credential: same shape — the repo aborts before any push even though the commit author is correct.

**Config precedence**
32. CLI `--engine git` beats `ro.local.toml` `engine = "claude"`.
33. `ro.local.toml` beats global `[engine] default`; global beats the built-in default.
34. A legacy config with only `[providers.claude]` still resolves `engine = claude` (the back-compat shim).
35. A typo in `ro.local.toml` (`authent = "gh"`) **errors** — the `deny_unknown_fields` guarantee.
36. `ro init` twice appends exactly one `ro.local.toml` line to `.gitignore`; a pre-existing `/ro.local.toml` entry is not duplicated.

**`ro init` modes and the `repos` CRUD**
37. `init_mode` is a pure function and is tested as one: `{}` outside a git repo → `Global`; `{}` inside one → `OnboardRepo`; `{--add-dir X}` → `Workspace(X)`; `{--add-repo X}` → `OnboardRepo(X)`. Four cases, no filesystem, no subprocess.
38. `ro init` in a fixture repo creates `ro.local.toml`, registers the row, and **does not create a `Global`-only marker** — i.e. the onboard path really did more than bootstrap.
39. `ro init` in a repo with **no `origin` remote** still registers, keyed by directory name, and prints a line saying the key was inferred. A fresh `git init` is a real case, not an error.
40. `ro init --add-dir <fixture>` over a tree containing a nested plain directory registers only the git repos, and the per-repo outcome is one line each. With `--non-interactive` it registers all without prompting; in a TTY it prompts first.
41. **The no-implicit-scan guarantee:** `ro init` with no args in a directory containing 5 git repos registers **none** of them. This is the test that keeps the feature honest — onboarding is never inferred.
42. `ro repos add` against an unreachable remote clones nothing and writes **no** row (assert the `repos` table count is unchanged).
43. `ro repos update --name` colliding with an existing `(host, owner, name)` is refused with a clear error; the original row is untouched.
44. `ro repos update --archive` flips `archived` and nothing else — assert the row's other columns are byte-identical before and after.
45. `ro repos update` preserves `repo_tags` and `repo_health_snapshots` rows for that repo, whereas `ro remove` + `add` drops them. This is the whole reason the verb exists; assert it explicitly.

**Health filter (compute kept, display gone)**
46. `ro checkpoint --filter health:critical` selects a subset and `--filter health:99` selects none, using real `repo_health_snapshots` rows — not a substring match.
47. `ro status --format text` and `ro status --format json` contain **no** `Health`/`health` key anywhere in their output, while `--filter health:` still works. The display/compute split is the contract; assert both halves.

**`repos add` spec hygiene (the Windows-path bug)**
48. `RepoSpec::parse("C:/work/backend")` is an **error**. Today it returns `Ok` with `owner = "C:"` and `name = "work/backend"` and would insert a row for a nonexistent owner. Reject a drive-letter owner.
49. `RepoSpec::parse("C:\\work\\backend")` is an error (already true — no `/` — but assert it so the drive-letter fix does not regress it).
50. `ro repos add .` errors with a message naming `ro init` as the command for an existing local repo, and inserts **no** row.

**`repos doctor`**
51. A fixture inventory with a deleted directory, a non-repo directory, a wrong `origin`, and a never-cloned row produces exactly the four expected states and a non-zero-but-not-fatal exit. Assert the `repos` table is byte-identical before and after — `doctor` writes nothing.
52. `ro repos doctor` on a healthy inventory exits 0 and prints one line per repo.

**Tag CRUD**
53. `ro repos tag a backend oss work` inserts three rows; running it again changes nothing (`INSERT OR IGNORE`) and still exits 0.
54. `ro repos untag a oss` removes exactly one row; `ro repos untag a not-a-tag` is a no-op that exits 0, not an error.
55. `ro repos tags` lists the union of tags with counts; `ro repos tags a` lists one repo's tags. `idx_repo_tags_tag` makes the aggregate query indexed.

**Global-only `allow_fallback`**
56. A `ro.local.toml` containing `allow_fallback = false` **errors** via `deny_unknown_fields`, with a message that says where the key moved. This is the enforcement of "one run, one fallback policy" — a per-repo key that silently parsed would be worse than not having the rule.
57. `ro checkpoint --allow-fallback` still overrides the global default for one run, and the summary records that the override was used.

**`ro run prune`**
58. After 20 seeded runs, `ro run prune --keep 5` leaves exactly the 5 newest, and every `run_events` / `sync_results` / `failures` row pointing at a deleted run is gone too. Assert no orphans by `run_id`.
59. `ro run prune` **without** `--keep` is a usage error (64), not a default-500 delete. The absence of a default is the feature.
60. `ro run prune --keep 1000` on a 20-run database is a no-op that reports `{deleted: 0, kept: 20}` and still exits 0.

**Per-repo identity — the thing that makes two accounts possible**
61. **Two repos, two authors, one command.** A fleet run over repo A (`identity.email = work@corp.com`) and repo B (`identity.email = me@gmail.com`) produces two commits with two different `git log -1 --format=%ae` values. This is the regression test for the whole feature: if it ever collapses to one author, the reason the tool exists is gone.
62. `identity` applied via `git -c user.name=… -c user.email=…` — assert the repo's **`.git/config` is byte-identical before and after**. Per-invocation `-c` is the whole mechanism; writing to the repo's config is the bug this prevents.
63. A repo with no `[identity]` table falls back to git's own configured user, unchanged.
64. `identity.email` and `auth.expected_login` are independent: setting `expected_login` to a value that does **not** match the credential still aborts the push, even though the commit author is correct. This is rule 5 in §4 — the author is controlled, the pushing account is not.

**Credential resolution — never a secret in a file**
65. `credential = "env:CI_GH_TOKEN"` resolves from the environment and the resolved value reaches the push as the `extraheader` credential. This is the CI path and must work with no OS keychain present.
66. `credential = "keychain:test-key"` with a `keyring` test backend returns the stored secret; with the `keychain` feature disabled the build still compiles and the error names the missing feature rather than failing to link.
67. **`SecretString` does not leak.** A `tracing` field, a `dbg!`, and a `{:?}` on an error containing the resolved credential all render `***`. This is the regression test for the `AuthToken: Debug` finding.
68. `ro.local.toml` containing `token = "ghp_…"` **errors** under `deny_unknown_fields` with a message pointing at `credential = "keychain:…"` or `credential = "env:…"`. A stale plaintext token must be a loud failure, not a silently-ignored key.
69. Repo A with `credential = "env:WORK_TOKEN"` and repo B with `credential = "env:PERSONAL_TOKEN"` push with **different** accounts in the same fleet run. This is what a global `GH_TOKEN` cannot express.

**Positional repo selection**
70. `ro checkpoint cass voice-ai-agent` and `ro checkpoint --all` select the same repos via the same resolver; a bare name, an `alias`, and an `owner/name` all resolve, and an unknown name is a **64** with the available names listed.
71. The resolver is *shared*: `ro status cass`, `ro sync cass`, and `ro checkpoint cass` all agree on what `cass` means. A name that matches two repos is an ambiguity error, not a coin flip.

**`ro pr`**
72. `ro pr` on a clean tree pushes the current branch and creates a draft PR, printing the URL. It does **not** create a commit — assert the commit count is unchanged.
73. `ro pr` on a **dirty** tree refuses and names the dirty paths. A PR is a statement about a specific commit.
74. `ro pr` twice does not open two PRs: the second run finds the open PR for the head, reports the same URL, and the fake `gh` recorded one `pr create` call.
75. `--base` resolves through the three live sources in order: `git symbolic-ref refs/remotes/origin/HEAD`, then `gh repo view --json defaultBranchRef`, then a usage error naming all three. There is no database fallback.

**`ro conflict`**
76. `ro conflict list` finds a repo left mid-rebase and reports its state, without changing anything.
77. `ro conflict <REPO>` on a repo that rebase-cleans fetches, rebases, and pushes, with no user interaction.
78. On a conflicting rebase it prints the conflicted paths and offers mergetool / abort, and **the run exits non-zero** rather than claiming success.
79. `ro conflict <REPO> --continue` **refuses when the index still has unmerged entries** and says why, instead of letting git produce a second failure.
80. `ro conflict abort <REPO>` restores the pre-operation HEAD. Assert the ref equals the value captured in the run row before the operation started — a rebase that drops commits and a rebase that does not are otherwise indistinguishable.
81. `ro conflict` refuses a repo list. It is one repo at a time, and passing two is a usage error.

**The engine-does-not-push boundary — the single most important test in this file**
82. **The agent cannot push, and the test proves it by trying.** Run a fake `claude` that (a) writes a commit and (b) attempts `git push` itself. Assert that the commit ro then pushes reaches the **other** bare remote with the `extraheader` credential, and that the fake's own push attempt did not reach it. If a future change lets the engine own the push, the credential assertion fails — which is the point.
83. The built-in prompt contains no "push" instruction. Assert on the literal prompt string, so the regression is caught at the source rather than in a behavioural test that a sufficiently literal agent could route around.
84. `EngineOutcome` has no `Pushed` variant. Adding one breaks a `match` in the orchestrator — which is the cheapest possible guard against someone "helpfully" extending it later.

**Bare `ro`**
85. `ro` inside a git repo with changes prints a checkpoint plan and changes nothing: no commit, no push, **no agent process spawned**. Assert the fake engine's invocation log is empty.
86. `ro cass backend` previews both named repos and nothing else.
87. `ro` outside any git repo and outside the inventory is a clear error, not a silent no-op.
88. `ro --format json` (bare) emits the preview as JSON, so a script can preview without parsing prose.
89. **Bare `ro` outside any git repo previews the managed inventory** — and only the managed inventory. Put four `.git` directories under a temp parent, register two, cd to the parent, run bare `ro`, assert exactly the two registered repos appear.
90. **The no-scan guarantee.** A `.git` directory sitting next to a managed one, never registered, is absent from that preview — and from `--all`. This is the test that keeps auto-discovery from creeping back in; it is the single most important safety test in the bare-`ro` block, because "ro pushed a repo I did not mean to" is not a bug report, it is a disclosure.
91. Bare `ro` outside a repo on an empty inventory is a clear "nothing managed" message, not a filesystem scan and not a silent success.

**PR base resolution**
92. A remote whose HEAD points at `origin/trunk` resolves `--base` to `trunk` — **not** `main`. A fixture that previously seeded `repos.default_branch = 'main'` can no longer do so, because the column does not exist; the test asserts against git's own answer.
93. With `refs/remotes/origin/HEAD` unset, resolution falls through to `gh repo view --json defaultBranchRef`.
94. With no remote and no `gh`, it is a usage error naming all three sources tried.
95. **The upgraded-database test.** Open a `state.db` created by the *pre-V4* binary, apply `V4_DROP_PLANS`, then run `ro pr` against it. This is the only test that catches a `SELECT` left pointing at a dropped column, because a freshly-built test database has the column absent and the stale reader compiles fine. Assert `ALTER TABLE` is idempotent on re-run (`migrate` must not try to drop it twice).

**`[agent] command` / `prompt` override**
96. `[agent] command = 'my-agent --flag'` runs `my-agent` with `--flag` and the built-in prompt.
97. `[agent] prompt` replaces the built-in instruction and is passed as a single argument.
98. **No shell interpolation.** A prompt containing `; rm -rf /`, `$(whoami)`, or a backtick reaches the agent as that literal string and nothing executes. The fake agent echoes its argv back and the test asserts the metacharacters survived verbatim. This is a command-injection test, not a formatting test.
99. A custom `command` naming a binary that does not exist yields `EngineOutcome::Unavailable` for that repo — a per-repo failure, not a fleet abort, and not a silent skip.

**The agent's environment is built by subtraction — the leak-chain tests**
100. **The parent token does not survive into the child.** Export `GH_TOKEN=ghp_test_value` in the test harness, dispatch the fake engine, and assert the value appears in **neither** the engine's environment, its argv, nor its captured stdout/stderr. This is the test for the whole leak chain, and it fails against the earlier design that injected the token.
101. The fake engine runs `printenv` and echoes everything back; the test asserts no value matches a PAT-shaped regex (`ghp_[A-Za-z0-9]{20,}` or `github_pat_…`). Belt and braces on #97, because the point is that nothing *reaches* the model, not merely that the model did not echo it.
102. A repo with **no** `[auth] credential` still gets a child environment with no `GH_TOKEN` — stripping is unconditional, not conditional on there being a credential to protect.
103. **The author does reach the agent, deliberately.** A repo with `[identity]` produces a child env carrying `GIT_AUTHOR_EMAIL`, and the fake engine asserts it is present — because `git commit` needs it and the value is public in the commit anyway.
104. **An agent cannot change the author by writing config.** The fake engine runs `git config --local user.email hijack@example.com` and commits. Assert: the commit's author is still the `[identity]` value, because ro re-asserts `-c` on every commit it performs; and the `.git/config` mutation is reported as a per-repo **warning** rather than silently accepted.
105. A parent `GITHUB_TOKEN` (rather than `GH_TOKEN`) is stripped by the same rule.

**Rebase before agent**
106. A push rejected as non-fast-forward is rebase-and-retry **without dispatching an engine**. Assert the fake engine's invocation count did not increase.
107. `--yes` makes the rebase-and-retry non-interactive, and does **not** enable `--resolve` — `--yes` alone on a real conflict still hands over to the user.
108. A real conflict with `--resolve` dispatches the engine exactly once more, with a different prompt, and the engine's captured output contains no `rebase --continue` and no `push`. ro runs both afterwards.
109. The rebase retry uses `--force-with-lease` and never `--force`. Assert the recorded invocation's argv.
110. A branch that is still non-fast-forward after a successful rebase is reported and the repo is marked failed — no second blind retry.

**Rebase happens before the engine — the ordering test**
111. **The happy path never force-pushes.** A repo that is behind `origin/main` by two commits, with local changes: assert the recorded `git push` argv contains **no** `--force` and **no** `--force-with-lease`, because the rebase at step (e) already made the push a fast-forward. This is the test that makes the ordering matter — against the old `commit → push → rebase → force-push` flow it fails immediately.
112. The recorded argv order is `fetch` → `rebase --autostash origin/<base>` → …engine… → `push`. Assert the rebase timestamp precedes the engine's invocation.
113. A **failed autostash pop** is a per-repo failure naming `git stash list` / `git stash pop`, and the engine is **not** dispatched on the half-popped tree. Assert the fake engine's invocation count is still zero.
114. `--resolve` on a real conflict during step (e) dispatches the engine once, with the conflict prompt, and the engine's output contains no `rebase --continue` and no `push`; ro runs both.
115. The base used for the rebase is the one snapshotted in the coordinator scan, not one re-derived after the engine ran.

**`GIT_CONFIG_*`, not `GIT_AUTHOR_*`**
116. The child environment carries `GIT_CONFIG_COUNT=2`, `GIT_CONFIG_KEY_0=user.name`, `GIT_CONFIG_KEY_1=user.email` — and carries **no** `GIT_AUTHOR_*` or `GIT_COMMITTER_*` variable. Assert absence as well as presence; a leftover `GIT_AUTHOR_*` would silently win over the config in some git versions.
117. A repo with no `[identity]` exports **no** `GIT_CONFIG_*` at all, and the commit falls back to git's own configured user.
118. `user.name`/`user.email` via `GIT_CONFIG_*` apply to `commit --amend` and to a tag created by the agent — assert the amending commit carries the identity, which is the concrete advantage over `GIT_AUTHOR_*`.

**Named credentials, no `provider`**
119. A repo with **no** `[auth]` table takes the `Machine` path: plain `git push`, no extraheader, and `GH_TOKEN`/`GITHUB_TOKEN`/`gh auth token` consulted in that order if git needs help.
120. `[auth] https = "env:CI_GH_TOKEN"]` resolves **only** that variable. Setting `GH_TOKEN` to a *different* value must not change which credential is used — the reference is precise, and that is the property a per-repo fleet run depends on.
121. A `token = "ghp_…"` key in `[auth]` is a `deny_unknown_fields` error, as is a `provider` key — the old name is not silently accepted, because a config that parses and is ignored is the failure this plan has been eliminating all along.
122. `https` and `ssh` both present is a validation error, not a precedence rule. Two credentials is an unstated choice, and ro should ask rather than pick.
123. An `env:` reference naming a variable that is **not set** fails that repo with a message naming the variable. It does not fall through to `GH_TOKEN`.
**Config edits — `toml_edit` and path resolution**
124. `ro config set` on a config with a hand-written comment: the comment and the unmodelled key both survive (the `toml_edit` guarantee).
125. `ro --config-dir <D>` **alone** is honoured — the `resolve_paths` tuple-match regression.

**Run history and output**
126. After a checkpoint, `ro run list` shows a row, `ro run show <id>` shows the exit code, and `ro run timeline <id>` has one event per repo outcome. All three tables are populated by zero production code paths today.
127. After a `ro sync`, `ro status`'s `last_synced_at` is no longer NULL — the FK regression.
128. Every line of `--format ndjson` has a non-null `ts` — the old production path emitted `"ts": null` on every line.
129. `--format ndjson` and `-j 4` produce the same **event ordering** as `-j 1` for the same fleet (determinism).
130. No `.ro.lock` appears in `git status --porcelain` after a run, and every repo reads as clean afterwards.

**Engine and timeout**
131. A missing engine binary yields a per-repo `Unavailable` naming the binary; the other repos still commit; exit 1.
132. An engine that hangs is killed at the timeout and reported as `TimedOut`, not `Failed`, and the fleet completes.
133. A non-zero exit from the agent is classified into the shared `FailureClass` (one taxonomy, not two).

### Platform-specific

CI runs a 3-OS matrix and the two red tests today are Windows-only, so the Windows list is not optional: the mixed-separator orphan regression (including a stored forward-slash row against a native walked path — this is the existing-databases case, not just a new-write case), a `main`-branch + `--direct` refusal (branch naming and checkout behave differently under `git for windows`), the `gh.cmd` shim path, and the `RunOpts` timeout kill (job-object semantics differ from POSIX process groups).

### Deleted along the way

`ro-review` (13 tests) · `ro-output` (~20 of 25) · `ro-github::import` (the failing `fetch_without_source_errors`) · the entire `commit_sweep` bucketing test block (~200 LOC) · `ro-sweep::{risk,policy_check}`.

**`ro-state::queries` (7 tests) is NOT deleted — this is the change from the earlier draft of this plan.** The health scorer survives because `--filter health:` is its only remaining caller. Two of its seven tests assert the `Display`/`row_to_health` shape of `HealthClass` for the `ro health` table; rewrite those against the filter's use (which classes exist, which `score` each implies) and keep the other five untouched. The `repo_health_snapshots` table survives too, so `score_repo_health` at `ro-sync/prune.rs:369,381` and the `CHILD_TABLES` entry in `manage.rs` both stay live.

**Rewritten, not deleted:** all five `crates/ro/tests/cli_init.rs` tests, and `ro-git/src/lock.rs::lock_acquire_and_release`, whose assertion that the lock file is removed on drop inverts when the TOCTOU fix lands.

---

## 9. Risks and open decisions

### Real risks

**The `delete_repo_cascade` landmine is the highest-risk item in the whole migration.** `crates/ro-sync/src/manage.rs` hard-codes `"repo_health_snapshots"` in `CHILD_TABLES` and `"plans"` in `NULLABLE_FK_TABLES` as **raw string literals**. A table name in either list with no table behind it leaves `cargo check`, `cargo clippy`, **and** `cargo test` green, then breaks `ro remove` and `ro repos prune` at runtime with a no-such-table error. Invisible to every static check. **After the health decision this got narrower and easier to get backwards:** only `"plans"` is removed from `NULLABLE_FK_TABLES`; the `CHILD_TABLES` entry for `repo_health_snapshots` **stays**, and the V4 migration no longer touches the health FK chain at all. Dropping the wrong one is the failure — a `DROP TABLE repo_health_snapshots` paired with a surviving `CHILD_TABLES` entry passes every check and dies on the first `ro remove`.

**Migrate forward, never edit V1.** `migrate::run` records applied versions in `_meta.version` and only runs migrations with `version > current`, so editing `V1_INITIAL_SCHEMA` never reaches an already-initialized database. Append `V4_DROP_PLANS` with **exactly** `DROP TABLE IF EXISTS plans;` and `ALTER TABLE repos DROP COLUMN default_branch;` — and nothing else. `all_tables_exist` (`migrate.rs:271,273`) and `all_indexes_exist` (`:300`) hard-code the expected table and index lists and must be updated — but only to remove `plans`; the health snapshot table and its index remain expected. `remove_clears_dependent_rows` (`manage.rs:437`) loses only its `plans` assertions. The `score_repo_health` calls in `ro-sync/prune.rs:369,381` are **untouched** — health scoring is still live.

**Dropping `default_branch` has a failure mode the other two drops do not.** `plans` and `repo_health_snapshots` are tables, and every reference to them is a string literal in a list you are already editing. `default_branch` is a **column with live readers** — `ro pr --base` and the empty-repo base inference both `SELECT` it. Drop the column before rewriting those two readers and the migration succeeds, the tests pass (they build a fresh DB, where the column is gone and the readers still compile), and every existing user's `ro pr` breaks at runtime. The V4 checklist therefore carries an explicit "grep the name before migrating" item. This is the same class of bug as the `delete_repo_cascade` landmine, one level deeper: a static check cannot see a missing column in a hand-written SQL string.

**The baseline is red in three places, and one of them is a production crash.** Clippy fails on all three OSes (`prune.rs:135`). Two tests fail on Windows (the orphan separator bug). And `ro import --stars` **panics in production** on any machine where a token is discoverable, because `import.rs:29` builds the octocrab client before `Runtime::new()` on line 31 — it passes on CI only because `gh` is off PATH there. Do not start disk-touching work on top of this.

**A credential design that silently does nothing.** `GH_TOKEN` in the child environment does not authenticate `git push`; git never reads it. If the token is not injected as an `extraheader` (or via a scoped credential helper, or `gh auth setup-git`), the gh-credentialed push falls through to whatever SSH key is configured — the exact leak the vision's HARD RULE forbids, arriving through a different door. This is the most likely way the auth work ships *looking* green, because every test that asserts only "a push was attempted" will pass. Test #29 exists specifically to close it.

**Identity and transport leakage is today's unguarded default, not a future risk.** `mutation::push` is a bare `git push` using whatever SSH key or credential manager the repo already has — no token injection, no login check, no email check, no `gh` path. Worse, `AuthToken` derives `Debug`, so `{:?}` prints the raw token and `redact()` is opt-in and manual. Fix the manual `Debug` **before** tokens start flowing into child processes and argv.

**Silent config failure, and the plan's own back-compat hazard.** No struct in `ro-config` carries `deny_unknown_fields`, so a typo'd or half-implemented `ro.local.toml` parses cleanly, does nothing, and `ro doctor` reports "config valid". Adding `deny_unknown_fields` to `AppConfig` would make every existing config unloadable — that omission is deliberate and must be commented, or the next person will "fix" it. And renaming `[providers]` to the fixed `[engine_*]` slots is the plan introducing a fresh instance of the risk it flags; the compat shim in Phase 2 is not optional polish, it is the difference between "your engine config silently vanished" and "nothing happened".

**The SQLite concurrency shape is load-bearing, not a detail.** `rusqlite::Connection` is `!Sync`. A design that shares one connection across `std::thread::scope` workers does not compile. The coordinator-owns-the-connection shape in §6.5 is what makes `-j` possible at all; anyone who "cleans up" by moving `append_event` into the worker loop reintroduces E0277 or, worse, a second writer.

**`ro doctor --fix` cannot upgrade an existing config today.** It writes the default only when the file is absent. After Phase 2 rewrites `DEFAULT_CONFIG_TOML`, every existing user keeps dead `[mcp]`/`[jobs]`/`[review]` config indefinitely, and `validate()` keeps checking keys the schema no longer models. Fix `--fix` to add missing sections in place, and delete the module doc's claim that every mutation backs up to `<state_dir>/doctor/runs/<run-id>/` — no backup code exists.

**Partial failure leaves half-applied work.** `apply_repo` increments `failed` mid-loop and never rolls back; there is no `reset_hard` call in the sweep path. Recommendation: **roll forward and report loudly.** `reset --hard` destroys work an agent may have done deliberately, and an automated rollback across an agent-modified worktree is scarier than a partial commit.

**Agent subprocesses are untrusted and unbounded.** `claude` and `codex` are long-running children with arbitrary write access to the worktree. No sandbox, no cancellation, and today no timeout anywhere. A hung or runaway engine stalls or corrupts a fleet run. Mitigation is a hard per-engine timeout that kills the child tree, the fixed `RepoLock`, and a dry-run that never dispatches an engine at all.

**Archived and disabled repos would be checkpointed.** `manage::list` is `SELECT {REPO_COLUMNS} FROM repos ORDER BY owner, name` (`manage.rs:208`) with no `WHERE` on `archived` or `disabled` — both columns exist in the schema — and `resolve_multi_repo_targets` calls `manage::list(conn, None)`. Without the filter, `ro checkpoint --all` would commit and push repos the user explicitly disabled. The single most consequential fleet-correctness gap, and the easiest to ship broken because the happy path looks fine.

**PR spam.** Twenty dirty repos, twenty draft PRs, re-run every few hours, is how you get a PR tab nobody reads. Mitigations: the duplicate-PR rule reuses rather than re-opens, WIP PRs are always draft, and `ro repos prune` should grow a WIP-branch sweep. Consider a deterministic branch name that encodes the run id so a stale PR is traceable to the run that made it.

**Engine availability is now a runtime dependency.** If neither `claude` nor `codex` is installed, `ro checkpoint` degrades to something strictly worse than `git commit -a` — it cannot split commits, and the vision is explicit that the agent engines are the whole reason it exists. Make the degradation loud and visible in the summary, not a silent per-repo skip.

**Exit-code redefinition breaks scripts, and so does the alias removal.** `2` currently means "clap usage error"; after this change it means "all repos failed". And `ro add` / `ro list` / `ro remove` / `ro prune` are the most-used commands in the tool and all break at the regroup. Both need release notes; the regroup is the larger break and is currently the quieter one.

### Open decisions for the dev

1. **TUI — now or never?** The vision says flags are the source of truth and any TUI is a frontend over them. **Recommendation: never.** The pain point is a fire drill under time pressure; a TUI is slower than a command. Revisit only past ~50 repos.
2. **`ro repos prune` — how much survives?** The keep-set does not include prune. `--archived` and `--missing` are pure database bookkeeping with no disk deletion and earn their place. The orphan half — `OrphanAction::{Report,Archive,Delete}`, `find_orphans`, `handle_orphans`, the archive-to-`<state_dir>/archived/<name>_<timestamp>` writer — is ~300 of `prune.rs`'s 508 lines and is the code with the data-loss bug. **Recommendation: keep `--archived`/`--missing`, gate `--orphans` behind its own flag, and drop `--delete` permanently.** If you disagree, the minimum is that `--delete` stays last, behind its own flag and an explicit confirmation.
3. **How strict should the identity guard be?** (a) never check unless `expected_login` is set — zero friction, zero protection; (b) check when set, warn loudly on every push when unset; (c) require an identity for every push. **Recommendation: (b).** Silent is the one option that must not ship.
4. **WIP branch naming and PR metadata.** `ro/wip/<slug>`? `ro/wip/<owner>-<branch>-<run-id-prefix>`? The run id makes a stale PR traceable to the run that made it, which is worth something in a 20-repo fleet. Pick a convention before the first WIP branch exists; retrofitting across existing branches is unpleasant. *(The "configurable template" question is closed: fixed naming + the run id, no template.)*
5. **Should checkpoint ever clone a missing repo?** **Recommendation: never in v1.** `ro sync --clone-only` over an explicit repo list is the deliberate tool, with its own confirmation. Pulling a full repository over the network mid-transaction because of a path typo is not a failure mode a commit tool should have.
6. **Where does `ro run` history live?** SQLite is already there, already migrated, and `delete_repo_cascade` already assumes one writer. **Recommendation: keep SQLite.** A per-run JSONL under `<state_dir>/runs/` is simpler and matches "no daemon", but it forks the audit surface for no gain.
7. **Should `ro status` and `ro checkpoint` share a scan?** Both walk the fleet reading branch/dirty/ahead-behind. A `ro_sync::targets::scan` shared by both — and later by `ro sync` — removes real duplication. The section-3/5 half of the plan should decide this once `targets.rs` exists; doing it in Phase 5 is cheap, doing it later is not.
8. **`--message` semantics with an agent engine.** Replace the engine's message entirely (single commit) or treat it as a hint? **Recommendation: replace.** A supplied message means the developer already knows what they want; otherwise omit the flag and let the engine read the diff. This also makes `--message` the only thing that forces a single commit, which is easy to document.
9. **The `ms` mandate in `AGENTS.md` is unsatisfiable on this machine.** `which ms` returns not-found. The mandatory `ms route` / `ms load` / `ms feedback` protocol cannot be executed, and the `ms feedback` step is silently skipped by every agent that works here. Either install `ms` or drop the mandate; meanwhile the file is training agents to ignore a mandatory instruction.
10. **`ffs grep --limit 0` does not mean unlimited** — it returns zero results and reports no matches. Any reference sweep run with `--limit 0` produces a confidently wrong "zero callers" report. Always pass an explicit large `--limit`.
11. **`ro doctor` is not a fleet command, and its exit code is not in the 0/1/2 table.** It keeps its own `0/1` (1 = "some check failed"), and its `Severity::Optional` probes can never move the exit code. `ro repos prune` hard-exits `3` (`main.rs:812`). **Decide whether to state this explicitly in the docs** — the plan's recommendation is yes, because otherwise a reader applies the fleet table to every command and misreads a failed environment check as "partial checkpoint success".
12. **`--filter health:` takes a class name or a number?** Today's syntax is `health:<N>` (a numeric threshold) and `HealthClass` still exists with `excellent`/`healthy`/`attention`/`risky`/`critical`. Accepting both — `health:critical` and `health:80` — is the friendliest and costs one parse. **Recommendation: accept both**, because a class is how you think ("only the broken ones") and a number is what the old flag already takes, so accepting only classes silently breaks muscle memory and only numbers keeps you typing `health:80` to mean "worse than attention".
13. **`ro repos scan` and `ro init --add-dir` are two ways to point at a directory.** `scan` is read-only; `--add-dir` registers. They share the walker, so the difference is one boolean. **Recommendation: keep both**, because "what would you pick up?" and "pick these up" are genuinely different questions and merging them forces a `--register` flag on `scan` that nobody will remember to pass. But if the command count is the thing you are optimising for, collapsing to `ro repos scan --register` is defensible.
14. **`--alias` on the repos row and the `repo_tags` table overlap.** `ro repos update --alias` writes one free-text label onto the row; `ro repos tag` writes many-to-many rows in `repo_tags`. A repo can therefore have one alias and N tags, which is probably more than anyone needs. **Recommendation: keep both** — `alias` is a single display/lookup shortcut on the row, `tag` is the multi-select for `--tag` filtering — but the docs must say which is which, or `--alias` will be discovered as a worse `--tag` within a week.
15. **What exit code does `ro repos doctor` return on drift?** The fleet commands use `0`/`1`/`2` for all-ok / partial / all-failed, and `ro doctor` keeps its own `0/1`. A drift audit has a third shape: some repos fine, some missing, none "failed". **Recommendation: 0 = no drift, 1 = drift found, 2 = nothing but drift** — and state it in the tree next to the command, because a reader who applies the checkpoint table to it will misread a single missing directory as a partial checkpoint run. A scripting-friendly alternative is to keep the exit code at 0 always and force callers to parse `--format json`, which is worse: a CI check that greps output is a check that will be forgotten.
16. **How strict is `ro repos doctor`'s remote-mismatch check?** `https://github.com/acme/worker.git` and `git@github.com:acme/worker` are the same repository, and the recorded `clone_url` is always built as `https://{host}/{owner}/{name}.git` by `repo_spec.rs:136` — so a repo cloned over SSH will mismatch on the literal string and flood the user with noise until they learn to ignore the command. **Recommendation: normalize before comparing** — strip a `.git` suffix, split the SSH `host:path` form, and compare `(host, owner, name)` only. A mismatch that survives that normalization is a real drift; one that does not is not reported at all. This must be settled before the check ships, because the alternative — reporting every SSH-cloned repo as drifted — trains the user to ignore the output within a day.
17. **Does `ro run prune` touch `context_cache` and `audit_log`?** Those two tables also grow without bound and neither is keyed by `run_id`, so the fanned delete cannot reach them with the same query. **Recommendation: leave them out of v1 and say so**, because `audit_log` is exactly the table you least want a retention command silently truncating. If they need bounding later it is a separate, separately-argued command — not a `--also-audit` flag on the run pruner.

### Settled — do not relitigate

The git mutation layer stays synchronous; no runtime is introduced for it, and octocrab is out of the checkpoint path entirely. `rusqlite` is coordinator-only; workers never touch the database. One new crate (`ro-engine`), three manifest edits, the orchestrator stays in the `ro` binary and target resolution moves to `ro-sync::targets`. An `enum EngineKind` plus a `trait Engine` — not a registry, not a free-form `[engines.*]` table. Push is always `git push`; `provider` selects the credential source, not a binary. PRs and identity shell out to `gh`. `AuthPolicy`/`AuthProvider`/`FailureClass` are defined once in `ro-core`. `run_in` keeps its `Option<&Path>` and gains a `RunOpts` third parameter. `deny_unknown_fields`, never `#[serde(flatten)]`, on the new config types. `ro doctor` gains no probes and its `Severity::Optional` provider checks stay. WIP PRs are always draft — a constant, not a flag. No `--json` shorthand. No `.bak` on every `config set` — `toml_edit` is the fix, not a backup. `ro-github` stays a dependency after `ro import` is cut, because `ro doctor` still calls `discover_token`.

**Settled in the second review round (2026-09-25), added here so they do not come back:**

- **`ro.local.toml` sits at the repo root as one bare file. No `.ro/` dotfolder.** This reverses an earlier suggestion in the same conversation; the original reasoning still holds — ro has exactly one per-repo config file, and a dotfolder holding one file is a dotfolder that exists to be justified. `.gitignore` gains a single `ro.local.toml` line.
- **The health score is a filter, not a display.** The scorer, the `repo_health_snapshots` table, and `--filter health:<N>` all survive; `ro health`, the 0-100 render, and the class table all die. This reverses the earlier "cut health entirely" position in this document. The only V4 table drop is `plans`.
- **`ro init` has three modes** — global bootstrap / onboard-the-current-repo / onboard-a-workspace — disambiguated by argv and cwd, never by an implicit directory walk. It stays dumb about engines (no PATH scan, no questions, ~1s) and is competent about repos.
- **`ro init` and `ro repos add` do not overlap.** `init` = a repo you already have, on disk, in front of you. `add` = a repo you want, cloned and registered, all-or-nothing. This removes "tracked but not cloned" as a state the rest of the system has to tolerate.
- **`ro repos update` exists** and is a targeted column edit, never `remove` + `add`. It has **no `--remote` flag**: there is no remotes column and no remotes table in the schema, and mirroring git's own state would create a second source of truth that drifts. The row key is `(host, owner, name)`, not `(owner, name)`.

**Settled in the third review round (2026-09-25):**

- **`ro repos add` takes a remote spec and nothing else.** `ro init` is the only verb that accepts a filesystem path. The two verbs are `ro init` = a repo you already have, `ro repos add` = a repo you want. A path-shaped argument produces an error naming `ro init`, not a row.
- **The Windows forward-slash path is a live bug in `RepoSpec::parse`, and it is fixed in `ro-core`, not in the handler.** `ro repos add C:/work/backend` currently parses as `owner="C:"`, `name="work/backend"`, and inserts a row for an owner that does not exist, with a clone URL of `https://github.com/C:/work/backend.git`. Reject drive-letter owners and any backslash, so every caller inherits the fix.
- **`ro repos doctor` exists and is read-only, with no `--fix`.** It consolidates three ad-hoc partial answers (the `status_repo` `.git` guard, the checkpoint `NotCloned` reason, `prune --missing`) into one command. Every remedy it could apply is destructive or opinionated, so it reports and the user acts. It is **not** folded into `ro status`: different question, different cadence.
- **`tag` ships as `tag` / `untag` / `tags`.** A table with a writer and no remover forces hand-editing SQLite, which is how people abandon tools. `INSERT OR IGNORE` and a no-op `untag` both exit 0 — "make it so" is idempotent and errors would make shell loops awkward. This was the only writer in the plan missing its remover; the audit that produced that conclusion is: for every table the plan writes, name the verb that shrinks it.
- **`allow_fallback` is global-only.** `provider`, `expected_login`, and `author_email` stay per-repo; the fallback posture does not, because one fleet run holding two safety policies is the state in which an identity leak cannot be reconstructed afterwards. The per-run `--allow-fallback` flag is the only override. A stale key in someone's `ro.local.toml` errors under `deny_unknown_fields` and names the right file.
- **`ro run prune --keep <N>` exists, retention is never automatic, and `--keep` is required.** No default, no time-based expiry, no size cap. An automatic policy eventually deletes the run that explains a bad push, and that failure is silent. Deleting rows is unrelated to `V4_DROP_PLANS`.
- **The five phases are the five PRs.** §2's `1`, `1a`…`1j` are checklist items inside PR 1, not separate units. PR 4 and PR 5 must not be merged: PR 4 leaves the tool without a fleet-commit command, which is unusable but honest, and merging them buries the atomic sweep cut — the riskiest deletion in the series — inside a large diff.

**Settled in the fourth review round (2026-09-25):**

- **`ro sync` pulls, `ro checkpoint` pushes. They do not merge.** This was asked twice and answered twice, including once after a message that proposed the opposite — so it is recorded here to stop it coming back a third time. `ro sync` reconciling the working copy with the remote is an established, safe, reversible meaning. Overloading that verb with commit+push means a user who types it out of pull habit **pushes**. `ro checkpoint` is the push verb; the name is longer and the safety is worth the keystrokes.
- **No daemon, no watch, no `watch:` config block.** Cut twice, stays cut. See the "What we are NOT doing" entry for why the ordering matters as much as the decision.
- **No workspace entity and no group concept.** Selection is a flat set of positionals plus `--all`; `repo_tags` is the one filtering mechanism. A "group" is a tag with a different word in front of it, and two mechanisms for one selection means two answers to "which repos does this run touch?".
- **No `ro clone`.** `ro repos add` clones and registers. One act, one verb.
- **Per-repo identity is real and applied, not just guarded.** `[identity] name/email` is set on the commit invocation via `git -c user.name=… -c user.email=…`, so nothing is written to `.git/config` and two repos can commit as two different people in one run. This is the single largest scope addition of this round: before it, the tool managed many repos but could not give them different identities, which is most of why the daily loop still needed a terminal.
- **`identity` and `expected_login` are different fields for different jobs.** The first is the commit author, which ro controls and therefore cannot get wrong. The second is the pushing account, which ro does not control and therefore must guard. Merging them produces a design where fixing your author silently starts author-guarding your push.
- **Credentials are references, never secrets.** `credential = "keychain:<name>"` or `credential = "env:<VAR>"`, resolved into a `SecretString` whose `Debug` renders `***`. A plaintext `token =` in a per-repo config is an **error**, not a convenience. `keyring` is the one new runtime dependency, behind a default-on feature with an `env`-only fallback so CI and minimal builds still work.
- **`ro pr` and `ro conflict` are both in v1, and `ro conflict` is the resolve assistant rather than a detector.** The daily loop is status → checkpoint → occasionally pr → occasionally conflict; cutting either of the last two leaves a hole the user falls back to a terminal for. `ro conflict` gains `<REPO>` and `--continue`; it does not become a separate `ro resolve` verb, and it never loops over a fleet.
- **Repo selection is positional everywhere, through one shared resolver.** `ro checkpoint cass voice-ai-agent`, not `--repo cass --repo voice-ai-agent`. One resolver, so `ro status`, `ro sync`, and `ro checkpoint` cannot disagree about what a name means.
- **The crate layout ends at five modules.** Registry, Git, Agent, Auth, GitHub — from twelve. `ro-jobs` folds into Registry, `ro-config` becomes a Registry module, `ro-github` reduces to the one function `ro doctor` calls, and PR creation shells out to `gh` instead of octocrab. Done as renames-and-moves inside PR 2 and PR 3, never as a standalone structural PR that touches no behaviour.
- **"No AI commit-message writing" was a misleading bullet and is rewritten.** The engines *do* write the commit messages — that is the flagship behaviour. What is deleted is ro's own deterministic bucketing, which existed to split commits *without* a model. The boundary is ro transacts and configures, the engine reasons.

**Settled in the fifth review round (2026-09-25):**

- **The engine commits; `ro` pushes. The engine must never push.** Asked three times across the review and answered the same way each time, because it is the load-bearing boundary of the entire auth design. An engine that pushes makes `extraheader`, `expected_login`, `SecretString`, per-repo identity, and the no-fallback rule all advisory — advice given to a subprocess that is free to ignore it. The built-in prompt therefore ends at "write the commits" and explicitly says *do not push*. The earlier draft of this document left the boundary unstated, which is how a prompt ending in "and then push" looked reasonable; that ambiguity is now closed and has a dedicated test.
- **The verb stays `ro checkpoint`.** `ship` (twice) and `commit` (once) were both proposed. `commit` is wrong for the default engine, which produces *several* commits; `ship` reads closer to deploy than to "save WIP". Settled, and recorded so it stops being re-proposed.
- **Bare `ro` is `ro checkpoint` on the current repo, and it is safe because it inherits `--dry-run`.** The most common action in the tool's life should not require remembering a subcommand, and the default it picks is read-only. No commit, no push, no agent process. A magic default is worth exactly one invocation, and this one cannot write anything.
- **cwd is the no-argument case, not the interface — and neither is a parent-folder scan.** A proposal to make `ro sync` mean "scan parent folders for `.git`, commit, push" was declined twice. The verb was already settled (`sync` pulls, `checkpoint` pushes), and more importantly the scan makes the set of repos ro will **push with your credential** depend on where you happen to be standing. A directory scan that reaches one repo too far is a disclosure, not a bug. Bare `ro` outside a repo gives the same one-word multi-repo ergonomics scoped to the inventory, which is opt-in by construction.
- **`ro pr` resolves the default branch from `git symbolic-ref` and `gh repo view` only — the `repos.default_branch` column is dropped in V4 and `--default-branch` is gone.** Two earlier revisions had this wrong twice: first preferring the column over git, then keeping it as a fallback. A cache of a branch name is a field that goes stale on a rename, and two live sources answer the question better than any cache can. This also removes the last reason `ro.local.toml` would ever need a `default_branch` key.
- **Dropping `default_branch` carries a failure mode the table drops do not.** It is a column with live readers in hand-written SQL, and a fresh test database has it absent, so a stale `SELECT` compiles, the suite passes, and every existing user's `ro pr` breaks at runtime. The V4 checklist therefore requires grepping the name before migrating, and test #95 runs `ro pr` against a **pre-V4 database** — the only test that can catch it.
- **`ro.local.toml` contains no `name`, no `branch`, no `default_branch`, no `remote`, no `owner`/`repo`.** Not a simplification for its own sake: the file lives at the root of the repo it describes, and each of those fields duplicates something git already knows and would eventually disagree with disk on. The file describes *how this repo behaves*, never *what it is*.
- **`[agent] command` and `[agent] prompt` are configurable, and that is the answer to "how do I add Gemini".** Two strings, not a plugin registry. The fixed three-entry `EngineKind` remains only to pick the behaviour profile — timeout, output parsing, availability probe. `{prompt}` is substituted as a single argv element and never through a shell, because the prompt is built from diff text.
- **`ro pr` prints a result; it does not open a browser.** Portability (three different launch commands, three different failure modes) and the fact that a URL on screen is already clickable.

**Settled in the sixth review round (2026-09-25):**

- **The resolved credential is NEVER placed in the agent's environment. This reverses an earlier revision of this document, which said the opposite.** The leak chain is five steps long and ro can intervene at none of them: env → the agent runs `printenv` or a build script → stdout → the agent's transcript on disk → that transcript is read, pasted into an issue, or fed to a different model next session. A transcript is *designed* to be read and shared, which is the opposite of a secret store. The engine's env is therefore assembled **by subtraction** — `GH_TOKEN`, `GITHUB_TOKEN` and any `RO_*_CREDENTIAL` removed from the parent's env — and the removal is unconditional, so a user who exports `GH_TOKEN` for `gh` does not silently hand it to every agent ro spawns. `SecretString` protects ro's own logs; it cannot protect a file ro does not write.
- **The author reaches git via `GIT_CONFIG_*`, not `GIT_AUTHOR_*`.** The agent can read it either way, so this is not a secrecy claim — it is three mechanical wins: the `GIT_CONFIG_*` form covers `commit --amend`, `tag`, and every other object-creating git call rather than only commit-ish; four variables become two; and it is the same mechanism as the `-c` ro already uses, so the author is expressed once. Stating the limit plainly matters more than the mechanism: **no approach hides an identity from a child process**, and the value is not a secret — it is about to be in a public commit. What ro guarantees is that the agent never *chooses* it.
- **An agent cannot change the author by writing `.git/config`.** The prompt says "do not modify git config"; the prompt is a request. The control is that ro's per-invocation `-c` wins over anything in the file, and ro hashes `.git/config` before dispatch and after, reporting a mutation as a per-repo warning — not to undo it, but because an agent writing git config is something the user needs to know happened.
- **No remote rewrite, therefore nothing to restore.** `git remote set-url` to a credential-bearing URL, push, then restore is a design with a second thing that can fail, needs a `finally`, can be killed between its halves, and puts a credential in an on-disk file for the duration. `git -c http.https://github.com/.extraheader=…` is per-invocation and writes nothing: there is no window in which a token exists on disk, and therefore no restore to forget. A run killed between commit and push leaves a clean local state and a token that only ever existed in memory.
- **Rebase mechanically first; call the agent only on a real conflict.** A rejected push is overwhelmingly a stale branch, not a semantic conflict, and `fetch → rebase → push --force-with-lease` is three git commands that need no model. `--resolve` is off by default and is the only step where a model edits files mid-rebase. The second dispatch gets a different prompt and the same stripped environment, and the engine still does not run `rebase --continue` or push — the rule does not relax because the situation got harder.
- **The rebase happens BEFORE the engine, not after a rejected push.** This is a better ordering than the plan originally had, and it is worth stating why: rebasing a dirty worktree with `--autostash` puts the tree on top of the remote's base *before* anything is committed, so the single commit the engine makes is a fast-forward and **the happy path never force-pushes at all**. The old order (commit → push → rejected → rebase → force-push) rewrites the branch and needs a lease, and a force-push across a fleet is the one operation here that can destroy someone else's work. A failed `--autostash` pop is a per-repo failure with a named recovery command, never a proceed-on-a-half-popped-tree.
- **`[pr] enabled` is not added.** `[agent] mode = "wip" | "direct" | "off"` already answers "does a checkpoint open a PR", and `ro pr` already has `--draft` / `--ready`. A third place to look for the same decision is how `ro` ends up with two answers to it.
- **`--force-with-lease`, never `--force`.** A tool that runs unattended across a fleet is exactly where a bare force-push turns "my branch moved while I rebased" into someone else's lost work.
