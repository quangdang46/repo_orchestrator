## MANDATORY TOOLS USE

**For any file search, grep, or symbol lookup in the current git-indexed
directory, use ffs tools.**

## ms — Skill Lookup (mandatory before non-trivial work)

This project uses [`ms`](https://github.com/quangdang46/ms) to manage reusable
skills. Before doing any non-trivial coding task, **you MUST first ask `ms`
whether a relevant skill already exists** and use it. Do not reinvent
solutions to problems we already have skills for.

### Mandatory protocol

1. **Route first.** Before starting work, run:
   ```bash
   ms route "<concise description of the task>" -O json
   ```
   Example: `ms route "fix Rust async runtime panic" -O json`.

2. **Load the top match.** If the response has `"decision": "match"`, take the
   first item in `candidates[]` and run **its `load_command` verbatim**:
   ```bash
   ms load <skill-id> --section <slug> -O json
   ```
   Read the returned skill content carefully and follow it. Cite the skill id
   in your reasoning so the user can audit which skill you used.

3. **Fallback to search.** If the route response has `"decision": "no_match"`,
   broaden with:
   ```bash
   ms search "<task>" -O json
   ```
   If a result looks promising, load it the same way as step 2.

4. **No useful skill found?** Proceed normally — but if you end up solving
   something reusable, mention it so we can capture it as a new skill with
   `ms build --from-cass "<topic>"`.

5. **Record feedback.** After you've used a skill (or tried to), record
   whether it was helpful so the bandit learns:
   ```bash
   ms feedback add <skill-id> --helpful        # it helped
   ms feedback add <skill-id> --not-helpful    # it didn't apply / was wrong
   ```

### Output handling

- All `ms ... -O json` commands write JSON to **stdout** and diagnostics to
  **stderr**. Exit code 0 = success. Parse stdout, ignore stderr unless the
  exit code is non-zero.
- Never run bare `ms` (it can launch interactive UI). Always use a
  subcommand and prefer `-O json` for machine-readable output.

### Quick reference

| When you need to... | Run |
|---------------------|-----|
| Find a skill for a task | `ms route "<task>" -O json` |
| Read a skill | `ms load <skill-id> --section <slug> -O json` |
| Search by keyword | `ms search "<query>" -O json` |
| List all skills | `ms list -O json` |
| Check what context suggests | `ms suggest -O json` |
| Show provenance for a skill | `ms evidence show <skill-id>` |
| Health-check the install | `ms doctor` |

### Native MCP integration (optional, recommended)

If your agent supports MCP (Claude Code, Codex with MCP, etc.), point it at:

```bash
ms mcp serve            # stdio transport
ms mcp serve --tcp-port 8080   # TCP, for non-stdio clients
```

This exposes `search`, `load`, `evidence`, `list`, `show`, and `feedback` as
native tools so the agent can call them directly without shelling out.

### Safety

`ms` runs all destructive shell commands through DCG (Destructive Command
Guard) and all untrusted text through ACIP (prompt-injection defense). If
you see `Destructive operation blocked` or `ACIP_*` messages, do not try
to bypass them — surface the message to the user and ask.

<!-- bv-agent-instructions-v4 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`) for issue tracking and [beads_viewer_rust](https://github.com/quangdang46/beads_viewer_rust) (`bvr`) for graph-aware triage. Issues are stored in `.beads/` and tracked in git. Current `br` workspaces normally export `.beads/issues.jsonl`; older `bd`/legacy workspaces may use `.beads/beads.jsonl`. `bvr` auto-discovers the supported JSONL files, so agents should use `br`/`bvr` commands instead of hard-coding a single filename.

### Using bvr as an AI sidecar

bvr is a graph-aware triage engine for Beads projects. Instead of parsing .beads/issues.jsonl / .beads/beads.jsonl directly or hallucinating graph traversal, use robot flags for deterministic, dependency-aware outputs with precomputed metrics (PageRank, betweenness, critical path, cycles, HITS, eigenvector, k-core).

**Scope boundary:** bvr handles *what to work on* (triage, priority, planning). `br` handles creating, modifying, and closing beads.

**CRITICAL: Use ONLY --robot-* flags. Bare bvr launches an interactive TUI that blocks your session.**

#### The Workflow: Start With Triage

**`bvr --robot-triage` is your single entry point.** It returns everything you need in one call:
- `quick_ref`: at-a-glance counts + top 3 picks
- `recommendations`: ranked actionable items with scores, reasons, unblock info
- `quick_wins`: low-effort high-impact items
- `blockers_to_clear`: items that unblock the most downstream work
- `project_health`: status/type/priority distributions, graph metrics
- `commands`: copy-paste shell commands for next steps

```bash
bvr --robot-triage        # THE MEGA-COMMAND: start here
bvr --robot-next          # Minimal: just the single top pick + claim command
```

Before claiming, verify current state with `br show <id> --json` or `br ready --json`. `recommendations` can include graph-important blocked or assigned work; only `quick_ref.top_picks` and non-empty `claim_command` fields represent claimable work.

#### Other bvr Commands

| Command | Purpose |
|---------|---------|
| `bvr --robot-insights` | Deep graph analysis: PageRank, betweenness, HITS, k-core, critical path |
| `bvr --robot-plan` | Dependency-respecting execution plan with parallel tracks |
| `bvr --robot-priority` | Priority misalignment detection |
| `bvr --robot-alerts` | Stale issues, blocking cascades |
| `bvr --robot-suggest` | Smart suggestions: duplicates, missing dependencies, labels |
| `bvr --robot-graph` | Dependency graph export (JSON/DOT/Mermaid) |
| `bvr --robot-search <query>` | Semantic search over issue titles/descriptions |
| `bvr --robot-history` | Bead-to-commit correlation from git history |
| `bvr --robot-label-health` | Per-label health metrics |
| `bvr --robot-schema` | JSON Schema definitions for all robot outputs |

#### br Quick Reference

```bash
br list                          # List all beads
br ready                         # Actionable beads (no open blockers)
br show <id>                     # View bead details
br update <id> --status in_progress  # Claim work
br update <id> --status closed   # Complete work
br dep add <id> <target>         # Add dependency
br sync --flush-only             # Sync changes to issues.jsonl
```
<!-- end-bv-agent-instructions -->

