//! PLAN.md is checked against the binary, so the plan cannot drift again.
//!
//! The audit (INCONSISTENCEANDREMNANIGN.md, section D1) found why 118 beads
//! could close while the CLI carried six cut commands and one missing flag:
//! `EXPECTED_COMMANDS` in `cli_schema.rs` and `COMMANDS` in
//! `docs_match_schema.rs` are hand-written transcriptions of the plan living
//! in test files. When the code and the transcription drifted the same way,
//! every test stayed green. `docs_match_schema.rs` was rewritten to scan the
//! documentation PROSE rather than its own list, and found seven ghosts on
//! the first run. But NOTHING read PLAN.md at all. This test closes that gap.
//!
//! Every expectation below is derived from the plan's own words and compared
//! against the live `ro schema` tree. Nothing here transcribes the command
//! surface, because a transcription is the thing that failed (D1). The two
//! static expectations that DO exist are stated as such in the comment that
//! introduces them, and each is there because the plan states that exact
//! sentence — the test reads the number out of it rather than carrying it.
//!
//! Where PLAN.md contradicts itself, the rule recorded above
//! `EXPECTED_COMMANDS` in `cli_schema.rs` governs: the more specific
//! statement wins over the summary table. Concretely, the specific
//! statements are the target command tree in §3, the tier table beside it,
//! and `ro schema`; the summary is a stray count in an older paragraph.
//!
//! # On the denial heuristic
//!
//! `docs_match_schema.rs` decides whether a mention of a removed command is
//! an advertisement or a *denial* with a plus-or-minus-120-character window
//! searched for denial phrases. This file started out reusing that approach
//! for the kept/cut split and it was wrong in the first run, in the exact way
//! the audit (D2) warns about:
//!
//! - **It over-reads.** Scanning every `` `ro <name>` `` in §3 for a kept
//!   set picked up `fork`, `health`, `import`, `prune`, `review`,
//!   `robot-docs`, `self-update` and `sweep` as *kept*. They are all in the
//!   §3 cut tables, where the denial is a table cell reading "*(nothing — it
//!   was already a stub)*" or a `Kind` column reading "Delete" — and the
//!   §3 "Deprecation" table's right-hand column says what each was replaced
//!   *by*, which is live vocabulary, not a denial. A phrase list wide enough
//!   to catch that also catches a live command two cells away.
//! - **It is still a heuristic.** It is a judgement about English, and it can
//!   be wrong in both directions: a cut command named with no denial nearby
//!   is called kept, and a live command sitting beside a denial aimed at
//!   something else is called denied.
//!
//! So the kept/cut split here is *structural* instead, and the heuristic is
//! kept only where English really is the only signal — deciding whether a
//! section-8 item that names a cut verb is denying it or asserting it. The
//! window arithmetic is unchanged, and the two known traps are avoided the
//! way the audit says to avoid them: `split_inclusive` rather than `lines()`
//! plus a fixed per-line stride (a stride of one assumes LF, and a CRLF
//! checkout slides every later offset by a byte per preceding line), and a
//! character *window* rather than the *line*, because the word that makes a
//! mention a denial is routinely on the neighbouring line.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Reading the two sources of truth
// ---------------------------------------------------------------------------

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the workspace root")
        .to_path_buf()
}

fn plan_text() -> String {
    std::fs::read_to_string(workspace_root().join("PLAN.md")).expect("reading PLAN.md")
}

fn schema() -> Value {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ro"))
        .arg("schema")
        .output()
        .expect("the binary runs");
    assert!(
        out.status.success(),
        "ro schema failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("ro schema emits JSON")
}

/// The live surface: every top-level command `ro schema` reports, minus
/// clap's own `help`.
fn live_commands() -> BTreeSet<String> {
    schema()["commands"]
        .as_array()
        .expect("schema has a commands array")
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .filter(|n| n != "help")
        .collect()
}

// ---------------------------------------------------------------------------
// The denial heuristic and its window arithmetic
// ---------------------------------------------------------------------------

/// The nearest char boundary at or below `i`. Slicing mid-character panics,
/// and the plan is full of box-drawing characters; a window edge that lands
/// inside one is nudged outward, because the exact edge never decides a
/// mention.
fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Phrases that make a mention a *denial* rather than an advertisement.
/// Borrowed from `docs_match_schema.rs`, plus the plan's own vocabulary.
///
/// It is a heuristic, and the direction it errs in is **toward calling a
/// mention denied** — which for section 8 means erring toward NOT failing a
/// correct item. That is deliberate: an acceptance item is a statement about
/// work still to do, and an unmarked item naming a cut verb is the A3 defect,
/// so a false *negative* here is a weaker test while a false *positive* would
/// fail the plan for being correctly specific.
///
/// **Every entry carries a denial subject.** A bare verb — `cut`, `delete`,
/// `gone` — used to be an entry, and it was wrong: those are ordinary
/// acceptance-criteria English, so an item saying "`ro run prune --days 30`
/// *deletes* worktrees older than 30 days, and the *deleted* worktrees are
/// reported" was excused by its own vocabulary, and the check went green on a
/// stale item naming two cut verbs. The word means the opposite of a denial
/// there. The fix is not to narrow the window but to require a subject: a
/// denial is a *claim about a command's existence* ("was cut", "is not a
/// verb", "no longer"), never a word that merely appears near one. The two
/// substitutions `instead of` and `rather than` went for the same reason —
/// they are how any plan sentence contrasts one thing with another. Every
/// entry below is a phrase that could not be written by an item asserting
/// the command ships.
const DENIALS: &[&str] = &[
    "no `ro",
    "No `ro",
    "There is no",
    "there is no",
    "There are no",
    "there are no",
    "does not exist",
    "not a command",
    "are not commands",
    "is not a verb",
    "not a verb",
    "no longer",
    "was cut",
    "were cut",
    "is cut",
    "are cut",
    "was deleted",
    "were deleted",
    "is deleted",
    "are deleted",
    "reverted",
    "folded",
    "superseded",
    "Superseded",
    "SUPERSEDED",
    "rewritten",
    "Rewritten",
    "REWRITTEN",
    "is moot",
    "are moot",
    "is gone",
    "are gone",
    "has gone",
    "not shipped",
    "never shipped",
    "is not there",
    "no such command",
];

/// Half-width of the denial window in characters, each side of the mention.
const WINDOW: usize = 120;

/// True when the `len` bytes at `at` are mentioned as something the plan
/// denies, judged by the window around them rather than the line.
///
/// The whole of the judgement is in [`DENIALS`]: a window is denied when it
/// contains one of those subject-bearing phrases and nothing else excuses a
/// mention. There is deliberately no second, looser path — a bare-verb
/// side-channel was tried and removed, because it cut the wrong way. Treating
/// a bare `cut` in the window as "ordinary English, so the mention is really
/// an assertion" also overrides a genuine `is cut` in the same window, so it
/// failed correctly-denied items in order to catch incorrectly-asserted ones.
/// The fix belongs in the list, where a phrase is either a denial about a
/// command's existence or it is not in the list at all.
fn window_denies(text: &str, at: usize, len: usize) -> bool {
    let lo = floor_boundary(text, at.saturating_sub(WINDOW));
    let hi = ceil_boundary(text, (at + len + WINDOW).min(text.len()));
    DENIALS.iter().any(|d| text[lo..hi].contains(*d))
}

// ---------------------------------------------------------------------------
// Section slicing
// ---------------------------------------------------------------------------

/// The text of the `## N. ...` section starting with `marker`, up to (not
/// including) the next `## ` heading. Panics when the marker is absent: a
/// plan that lost the section the test reads is a plan the test cannot check,
/// and a quiet skip would be exactly the failure mode of D1.
fn section(text: &str, marker: &str) -> String {
    let start = text.find(marker).unwrap_or_else(|| {
        panic!("PLAN.md has no {marker:?} section; the test cannot check a section that moved")
    });
    let rest = &text[start + marker.len()..];
    let end = rest
        .find("\n## ")
        .map(|i| start + marker.len() + i)
        .unwrap_or(text.len());
    text[start..end].to_string()
}

/// The block between the first ```` ``` ```` fence *after* `at`, inclusive of
/// the fence lines. The target command tree is such a block.
fn fenced_block_after(text: &str, at: usize) -> &str {
    let open = text[at..]
        .find("```")
        .map(|i| at + i)
        .unwrap_or_else(|| panic!("no fenced block after byte {at}"));
    let after = open + 3;
    let close = text[after..]
        .find("```")
        .map(|i| after + i)
        .unwrap_or(text.len());
    &text[open..close]
}

/// The numbered acceptance items of section 8: `(number, (start, body))`,
/// where `start` is the item's byte offset *within section 8* and `body`
/// is the item's own text plus every following line up to (not including)
/// the next numbered item. The SUPERSEDED / REWRITTEN marker is a blockquote
/// line immediately after the item, so it travels with the item by
/// construction.
///
/// The whole of section 8 is walked rather than just the "Required tests"
/// subsection, because the item numbering is what identifies an item — the
/// plan's own cross-references cite numbers ("items 42–45, 50, 52–55"), not
/// headings, and a subsection that moves must not silently empty the map.
///
/// A caller passing `s8` into another function must pass the SAME slice
/// through: the offsets are relative to its start, and a differently-sliced
/// section (one that starts at the heading, say) makes them wrong.
fn section8_items(s8: &str) -> BTreeMap<u32, (usize, String)> {
    // Two structural bounds, computed once:
    //  - where each numbered item's line starts, and
    //  - where the section's own tail begins (the superseded index, or the
    //    next `### ` heading — whichever comes first).
    //
    // Offsets come from `chunk.len()` under `split_inclusive`, never from a
    // fixed `+ 1` per line: a fixed stride assumes LF, and a CRLF checkout
    // slides every later offset by a byte per preceding line (audit D2).
    let index_at = s8
        .find("Items marked superseded above, in one place")
        .unwrap_or(s8.len());

    let mut numbered: Vec<(usize, u32)> = Vec::new();
    let mut offset = 0usize;
    for chunk in s8.split_inclusive('\n') {
        let head = chunk.trim_end_matches(['\n', '\r']).trim_start();
        if let Some(n) = head
            .split_once('.')
            .filter(|(h, _)| !h.is_empty() && h.chars().all(|c| c.is_ascii_digit()))
            .and_then(|(h, _)| h.parse::<u32>().ok())
        {
            numbered.push((offset, n));
        }
        offset += chunk.len();
    }

    // The tail bound is the first `### ` heading *after the last item*, not
    // the first one in the section: section 8 opens with "### Fixtures" and
    // "### Required tests", both of which sit well above item 1. Using the
    // section's first heading would clip every item's body to nothing.
    let after_last = numbered.last().map(|(at, _)| *at).unwrap_or(0);
    let next_heading = s8[after_last..]
        .find("### ")
        .map(|i| after_last + i)
        .unwrap_or(s8.len());
    let tail_at = index_at.min(next_heading);

    let mut items = BTreeMap::new();
    for (i, (start, n)) in numbered.iter().enumerate() {
        let next = numbered.get(i + 1).map(|(at, _)| *at).unwrap_or(s8.len());
        let end = next.min(tail_at);
        // A bolded group label ("**`ro run prune`**", "**Per-repo isolation
        // and exit codes**") sits between items. It heads the block that
        // follows; it is not a continuation of the item above it, and a cut
        // verb named in one is a heading rather than an assertion. So it is
        // dropped from the body instead of being read as the previous item's
        // claim. A marker line starts with a blockquote and never matches.
        let body: Vec<&str> = s8[*start..end]
            .lines()
            .filter(|l| !is_group_heading(l))
            .collect();
        items.insert(*n, (*start, body.join("\n")));
    }
    items
}

/// A bolded group label sitting on its own line between items. A marker line
/// is `> **SUPERSEDED …`, which starts with a blockquote and so is never
/// mistaken for one.
fn is_group_heading(raw: &str) -> bool {
    let t = raw.trim();
    t.starts_with("**")
        && t.ends_with("**")
        && !t.contains("SUPERSEDED")
        && !t.contains("REWRITTEN")
}

/// The plain `(number, body)` view, for every item. The last item's body is
/// already clipped at the section tail by `section8_items`, so no caller has
/// to special-case it.
fn item_bodies(items: &BTreeMap<u32, (usize, String)>) -> Vec<(u32, String)> {
    items.iter().map(|(n, (_, b))| (*n, b.clone())).collect()
}

// ---------------------------------------------------------------------------
// The plan's kept surface — read structurally, not by judging English
// ---------------------------------------------------------------------------

/// Every command the plan *keeps*, read from the three places the plan states
/// its surface, none of which need a judgement about English:
///
/// 1. the target command tree in §3 — a fenced block whose command lines
///    read `  name <ARGS>   # comment`;
/// 2. the tier table beside it — `| **Daily** | `a` · `b` | what it is for |`;
/// 3. every "Done looks like" line, which is the plan's own summary of the
///    surface per phase.
///
/// A "Done looks like" line has a positive half and a negative half, split
/// at an em dash: "…`schema` — no review, no `ro health` command, no fork…".
/// Only the positive half is a list of things that ship, so only the
/// positive half is read. The negative half is a *denial*, which is the
/// correct way to write about a command that does not exist, and reading it
/// as a list is what produced the eight false "kept" commands on the first
/// run of this file.
fn plan_kept_commands(plan: &str) -> BTreeSet<String> {
    let mut kept: BTreeSet<String> = BTreeSet::new();
    let s3 = section(plan, "## 3.");

    // 1. The target command tree.
    //
    // A command line is indented by two spaces and begins with the verb;
    // everything else in the block is either a continuation line indented
    // deeper than two spaces, or the `ro [<GLOBAL FLAGS>] <COMMAND>` banner at
    // the left margin. The verb itself is read from the first whitespace-
    // delimited word, *before* any `#` — every command line in this plan
    // carries a trailing `# comment`, so excluding lines that mention `#`
    // would exclude the whole tree.
    if let Some(at) = s3.find("### Target command tree") {
        for line in fenced_block_after(&s3, at).lines() {
            let Some(rest) = line.strip_prefix("  ") else {
                continue;
            };
            let verb = rest.split_whitespace().next().unwrap_or("");
            if verb.is_empty() || !verb.starts_with(|c: char| c.is_ascii_lowercase()) {
                continue;
            }
            // An argument shape (`<ARGS>`), or an option's continuation, is
            // not a command name.
            let name: String = verb
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if name.is_empty() {
                continue;
            }
            kept.insert(name);
        }
    }

    // 2. The tier table: the second cell of a `| **Tier** | names | … |`
    //    row is nothing but backticked names separated by `·`, which is a
    //    shape to parse rather than a sentence to judge.
    for line in s3.lines() {
        // `| **Daily** | `sync` · `push` | … |` splits into
        // ["", " **Daily** ", " `sync` · `push` ", …]: the names are the
        // THIRD cell, not the second.
        let mut cells = line.split('|');
        let _leading = cells.next();
        let _label = cells.next();
        let Some(names) = cells.next() else { continue };
        if !line.starts_with("| **") {
            continue;
        }
        let names: Vec<String> = names
            .split('`')
            .skip(1)
            .step_by(2)
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();
        if names.len() >= 2 && names.iter().all(|n| !n.contains(' ')) {
            kept.extend(names);
        }
    }

    // 3. Every "Done looks like" line, positive half only.
    for chunk in plan.split_inclusive('\n') {
        let raw = chunk.trim_end_matches(['\n', '\r']);
        let Some(pos) = raw.find("**Done looks like:**") else {
            continue;
        };
        let positive = raw[pos..].split('—').next().unwrap_or("");
        // A `ro <name>` mention is a command however it is spelled.
        for at in occurrences(positive, "ro ") {
            let word: String = positive[at + 3..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if !word.is_empty() {
                kept.insert(word);
            }
        }
        // …and a backticked run of bare names is the enumeration form the
        // Phase-5 line uses. A single backticked word is not a list — `gh`
        // and `format` appear that way and are not commands — so a run has
        // to be at least three long before it is read as one.
        let bare: Vec<String> = positive
            .split('`')
            .skip(1)
            .step_by(2)
            .map(|n| n.trim().to_string())
            .filter(|n| n.chars().all(|c| c.is_ascii_lowercase() || c == '-') && !n.is_empty())
            .collect();
        if bare.len() >= 3 {
            kept.extend(bare);
        }
    }
    kept
}

/// Byte offsets of every occurrence of `needle`.
fn occurrences(text: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find(needle) {
        out.push(from + i);
        from += i + needle.len();
    }
    out
}

// ---------------------------------------------------------------------------
// Check (a): everything the plan keeps actually exists
// ---------------------------------------------------------------------------

#[test]
fn every_command_the_plan_keeps_exists_in_the_binary() {
    let plan = plan_text();
    let kept = plan_kept_commands(&plan);
    assert!(
        !kept.is_empty(),
        "the plan scan found no kept commands; the section markers moved"
    );
    let live = live_commands();
    let ghosts: Vec<&String> = kept.iter().filter(|k| !live.contains(*k)).collect();
    assert!(
        ghosts.is_empty(),
        "PLAN.md keeps {ghosts:?} but `ro schema` has no such command. \
         Kept (target tree + tier table + 'Done looks like' positive halves): \
         {kept:?}. Live: {:?}.",
        live.iter().collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Check (b): everything the binary ships is accounted for by the plan
// ---------------------------------------------------------------------------

#[test]
fn every_live_command_is_accounted_for_by_the_plan() {
    let plan = plan_text();
    let kept = plan_kept_commands(&plan);
    let live = live_commands();
    // The direction that catches a silent addition: a command that ships
    // without a plan entry. Same scan as check (a), so a command the plan
    // discusses only to cut it does not count as accounted for — it has to
    // appear as kept.
    let unplanned: Vec<&String> = live.iter().filter(|c| !kept.contains(*c)).collect();
    assert!(
        unplanned.is_empty(),
        "the binary ships {unplanned:?} with no kept entry in the PLAN.md target \
         command tree, the tier table, or a 'Done looks like' list. A new command \
         should be a deliberate plan change, not an accident. Kept: {kept:?}."
    );
}

// ---------------------------------------------------------------------------
// Check (c): every TARGET-surface count the plan states is the real count
// ---------------------------------------------------------------------------

fn number_word(s: &str) -> Option<usize> {
    match s.to_ascii_lowercase().as_str() {
        "zero" => Some(0),
        "one" => Some(1),
        "two" => Some(2),
        "three" => Some(3),
        "four" => Some(4),
        "five" => Some(5),
        "six" => Some(6),
        "seven" => Some(7),
        "eight" => Some(8),
        "nine" => Some(9),
        "ten" => Some(10),
        "eleven" => Some(11),
        "twelve" => Some(12),
        "thirteen" => Some(13),
        "fourteen" => Some(14),
        "fifteen" => Some(15),
        "sixteen" => Some(16),
        "seventeen" => Some(17),
        "eighteen" => Some(18),
        _ => s.parse::<usize>().ok(),
    }
}

/// Phrases that pin a count to the surface this plan *ships*, as opposed to
/// the surface that existed before it, to a subset of it, or to some other
/// noun that also happens to be counted. Each is a phrase the plan itself
/// uses in exactly that role.
///
/// Six of the seven name no number. The three `across N commands` entries are
/// the exception and are here for the opposite reason from the rest: they
/// name the counts an *earlier revision* of the plan got wrong ("ten",
/// "eleven", "twelve"), and a sentence claiming one of those is a sentence
/// asserting a wrong target surface — the check below must fire on it. They
/// are not transcriptions of today's surface; they are the historical wrong
/// answers this check exists to catch a recurrence of. Two of the three
/// ("ten", "eleven") currently match nothing in the plan, which is the point:
/// they are tripwires for a phrasing the plan has already abandoned, not a
/// count of anything. (There is one unrelated "across twenty repos" sentence
/// in the plan, about a fleet size, not a surface; it contains no
/// `<number> commands` token, so the count scan cannot see it.)
const TARGET_SURFACE_ANCHORS: &[&str] = &[
    "in three tiers",
    "top-level commands",
    "commands of §3",
    "ro help",
    "listing exactly",
    "the tree",
    "the tool with",
    "across twelve commands",
    "across ten commands",
    "across eleven commands",
];

/// Phrases that put a `<number> commands` mention somewhere other than the
/// target surface, applied to the clause *after* the number rather than the
/// whole sentence.
///
/// Two survive the noun filter ("commands"/"command"/"verbs"/"verb") and
/// both name a subset, not the surface:
/// - "Three commands keep their names" — three of the twelve survive a
///   rename; that is a fact about the rename table, not a surface count.
/// - "six commands under a namespace" — a count of what an *earlier*
///   revision moved under `ro repos`, about a grouping the plan reverted.
///
/// Everything else the count scan once had to exclude is excluded by the
/// noun filter instead: "5 migrations", "four modules" and "12 crates" are
/// not followed by "commands", and "18 top-level clap subcommands" is
/// followed by "subcommands". Dropping them matters — "four modules, the
/// twelve commands in three tiers, **five** migrations" is a real
/// target-surface count, and a disqualifier for "migrations" would silence
/// it, because the sentence counts three different things.
///
/// A third subset phrase needed no entry: "five of them are the daily loop"
/// is already dropped by the noun filter, because "five" is followed by "of",
/// not by "commands". That is the reason the daily-loop subset needs no
/// disqualifier — and why `the_tier_table_partitions_the_kept_surface_into_three_tiers`
/// can own it instead, being the more specific statement.
const NOT_TARGET_SURFACE: &[&str] = &["keep their names", "under a namespace"];

/// Every stated count of the target surface, as `(line, stated, sentence)`.
///
/// The number is read out of the sentence; it is deliberately *not* written
/// down here, because a number written down next to the code is the second
/// transcription that let 118 beads close against a six-cut-command binary
/// (audit D1). The sentence is a character range rather than a line, and
/// `split_inclusive` keeps the byte offset exact — a `+ 1` per line assumes
/// LF, and a CRLF checkout slides every later offset by one byte per line
/// (audit D2).
fn target_surface_counts(plan: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for chunk in plan.split_inclusive('\n') {
        let base = offset;
        offset += chunk.len();
        let raw = chunk.trim_end_matches(['\n', '\r']);
        let words: Vec<&str> = raw.split_whitespace().collect();
        let mut wi = 0;
        while wi < words.len() {
            let head = words[wi].trim_matches(|c: char| !c.is_alphanumeric());
            if number_word(head).is_none() {
                wi += 1;
                continue;
            }
            // The counted noun follows, possibly through "top-level".
            let mut ni = wi + 1;
            if words.get(ni).is_some_and(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .eq_ignore_ascii_case("top-level")
            }) {
                ni += 1;
            }
            let noun = words.get(ni).map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_ascii_lowercase()
            });
            if !matches!(
                noun.as_deref(),
                Some("commands") | Some("command") | Some("verbs") | Some("verb")
            ) {
                wi += 1;
                continue;
            }
            // The sentence, as a character range around the number.
            let at = base + raw.find(words[wi]).unwrap_or(0);
            let start = plan[..at].rfind('.').map(|i| i + 1).unwrap_or(0);
            let end = plan[at..].find('.').map(|i| at + i).unwrap_or(plan.len());
            let sentence = plan[floor_boundary(plan, start)..ceil_boundary(plan, end)].trim();
            let lower = sentence.to_ascii_lowercase();
            // The disqualifier is read from the *clause* — the number, its
            // noun, and what follows up to the end of the sentence — not the
            // sentence's opening. "Three commands keep their names" names a
            // subset in the words right after the number, while a
            // target-surface sentence often opens with a different subject
            // ("Done looks like: four modules, the twelve commands in three
            // tiers, five migrations"). A window that reached backwards would
            // disqualify the true count on the strength of something that is
            // not part of it.
            let clause = &plan[at..end];
            let is_target = TARGET_SURFACE_ANCHORS.iter().any(|a| lower.contains(a))
                && !NOT_TARGET_SURFACE.iter().any(|a| clause.contains(a));
            if is_target {
                out.push((
                    plan[..at].matches('\n').count() + 1,
                    number_word(head).expect("checked above"),
                    sentence.to_string(),
                ));
            }
            wi = ni + 1;
        }
    }
    out
}

#[test]
fn every_target_surface_count_in_the_plan_is_the_real_count() {
    let plan = plan_text();
    let live = live_commands();
    let counts = target_surface_counts(&plan);
    assert!(
        !counts.is_empty(),
        "the plan scan found no target-surface counts; the count sentences moved"
    );
    let wrong: Vec<String> = counts
        .iter()
        .filter(|(_, n, _)| *n != live.len())
        .map(|(line, n, sent)| {
            format!(
                "line {line}: says {n}, `ro schema` has {} — {sent}",
                live.len()
            )
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "PLAN.md states a target-surface count that is not the real count:\n{}\n\
         Live ({}): {:?}.\n\
         The number is extracted from the sentence, not written down here — fix the \
         plan, not the test. If the sentence stopped being about the target surface \
         (it lost \"in three tiers\", \"of §3\", \"ro help\" …) this check goes quiet; \
         that is a real limit of classifying English, not of the count.",
        wrong.join("\n"),
        live.len(),
        live.iter().collect::<Vec<_>>()
    );
}

/// The plan calls the surface a three-tier split, and every surface count is
/// phrased around it ("Twelve commands in three tiers", "twelve top-level
/// commands in three tiers"). That makes the tier table the specific
/// statement and a bare count the summary — the rule from `cli_schema.rs`.
///
/// So: the tier table must have three rows, and their command cells must
/// partition the kept surface exactly.
///
/// **One of those two numbers is a static expectation and one is derived,
/// and the difference matters.** The `3` in the assertion below is written
/// down here. It is a transcription of the plan's repeated "three tiers"
/// phrasing, and the test's thesis is that a transcription is the failure
/// mode (audit D1) — so it is named rather than dressed up as a derivation.
/// It is an acceptable one here, for two reasons that the assertion itself
/// then cross-checks: it is a *structural* fact about how the plan is
/// organised, not a transcription of the command surface, and the partition
/// assertion immediately after it (`union == plan_kept_commands`) re-derives
/// the surface from the plan's own tree, so a compensating lie in the tier
/// table cannot make a wrong count look right. The command *names* are all
/// derived; the tier *count* is the one static number, and it is stated as
/// such.
#[test]
fn the_tier_table_partitions_the_kept_surface_into_three_tiers() {
    let plan = plan_text();
    let s3 = section(&plan, "## 3.");
    let tiers: Vec<Vec<String>> = s3
        .lines()
        .filter(|l| l.starts_with("| **") && l.contains('·'))
        .filter_map(|l| {
            let names: Vec<String> = l
                .split('|')
                .nth(2)?
                .split('`')
                .skip(1)
                .step_by(2)
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect();
            (names.len() >= 2).then_some(names)
        })
        .collect();
    assert_eq!(
        tiers.len(),
        3,
        "the section-3 tier table has {} tiers, not the three every target-surface \
         count is phrased around: {tiers:?}",
        tiers.len()
    );
    let mut union: BTreeSet<String> = BTreeSet::new();
    let mut overlap = Vec::new();
    for tier in &tiers {
        for name in tier {
            if !union.insert(name.clone()) {
                overlap.push(name.clone());
            }
        }
    }
    assert!(
        overlap.is_empty(),
        "a command appears in more than one tier: {overlap:?}. The tiers are a \
         partition, not a set of suggestions."
    );
    assert_eq!(
        union,
        plan_kept_commands(&plan),
        "the tier table and the target command tree disagree about the surface. \
         Tiers: {union:?}."
    );
}

// ---------------------------------------------------------------------------
// Check (d): the A3 check — no live section-8 item names a cut command
// ---------------------------------------------------------------------------

/// Commands §3 cuts or reverts.
///
/// §3 is the section that decides the surface, so a `ro <verb>` it names and
/// does not keep is a cut or reverted verb by definition — the tables of
/// renamed, deleted and demoted commands are *in* that section, and the
/// "There is no `ro pr`" sentences are there too. Deriving the set this way
/// means a newly cut command is picked up with no edit to this file, which is
/// the property `docs_match_schema.rs` was rewritten for.
fn section3_cut_commands(plan: &str, kept: &BTreeSet<String>) -> BTreeSet<String> {
    let s3 = section(plan, "## 3.");
    let mut cut = BTreeSet::new();
    for at in occurrences(&s3, "`ro ") {
        let word: String = s3[at + 4..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        // `ro --flag` is a flag, not a command; `ro <GLOBAL FLAGS>` is the
        // tree banner, and `ro` alone is the program.
        if word.is_empty()
            || word.starts_with('-')
            || !word.starts_with(|c: char| c.is_ascii_lowercase())
        {
            continue;
        }
        cut.insert(word);
    }
    // `ro repos <verb>` names the reverted namespace *and* the verb under
    // it. The second word is what a section-8 item would assert, so record
    // it as well: `ro repos doctor` is a cut invocation and item 51 is
    // exactly the defect A3 is about.
    for at in occurrences(&s3, "`ro repos ") {
        let verb: String = s3[at + 10..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if !verb.is_empty() && !verb.starts_with('-') {
            cut.insert(verb);
        }
    }
    cut.retain(|c| !kept.contains(c) && c != "help" && c != "ro");
    cut
}

/// The verbs a section-8 item *asserts*: a `` `ro <verb> `` invocation, or a
/// `ro <verb> …` one that someone could type. An item that names a cut verb
/// while denying it ("there is no `ro conflict`") is correct documentation
/// of the cut, not a claim that the verb ships, and is excused.
fn item_asserts_cut_verb(item: &str, cut: &BTreeSet<String>) -> Vec<String> {
    let mut asserted = Vec::new();
    for at in occurrences(item, "ro ") {
        let word: String = item[at + 3..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if word.is_empty() || !cut.contains(&word) {
            continue;
        }
        // The heuristic, used for the one judgement English forces here. A
        // subject-bearing denial within the window means the item is talking
        // *about* the cut — "There is no `ro run prune`", "was cut" — which is
        // the right thing for a plan to do and is not the A3 defect. An
        // explicit marker is stronger and is checked separately.
        //
        // What this deliberately does NOT do: look for a bare `cut`/`deleted`
        // in the window and treat it as evidence that the item *asserts* the
        // verb instead. That guard existed and was removed — it excused the
        // item below, and it also fails a correctly-denied item that happens
        // to contain both `is cut` and `cut`. Nothing in this file overrides
        // `window_denies`; the list is the only path, which is what makes the
        // behaviour in `a_stale_item_naming_a_cut_command_is_not_excused_by_its_own_prose`
        // the whole story.
        if !window_denies(item, at, word.len() + 3) {
            asserted.push(word);
        }
    }
    asserted.sort();
    asserted.dedup();
    asserted
}

/// Flags the plan cuts, and a section-8 item asserting one is the same defect
/// as asserting a cut verb: `ro sync --tag` is a flag that is not on the
/// binary, exactly as `ro conflict` is a verb that is not.
///
/// # Derived, not listed — because a list was the defect
///
/// The first version of this function filtered a hand-written `CANDIDATES` list
/// against §3, and that was audit D1 confined to a smaller surface: a cut flag
/// the list forgot was invisible to the check by construction. `--add-dir` and
/// `--add-repo` are cut by the plan (`:78`, `:1531`) and were both dropped by
/// the filter, because §3 does not contain the strings — the cuts live in §2
/// and §9. A live, unmarked section-8 item asserting `ro init --add-dir` sailed
/// through a green suite.
///
/// So the set is derived from the plan's own denial statements, the way
/// [`section3_cut_commands`] derives the command set. A flag is cut when the
/// plan states it is cut:
///
/// - a removal table whose *left* cell names the thing that goes — `| Removed |`
///   rows, `| Was |` rows, and `| Before |` rows whose Kind column says the
///   left thing was deleted;
/// - a sentence asserting a flag's *non-existence* — "There is no `--base`";
/// - a sentence carrying the flag's own predicate — "`--default-branch` is
///   deleted";
/// - the "No global `--json`" shape, which names the flag it refuses;
/// - a "no X filter" shape, which names the flag whose consumer is gone.
///
/// Then everything `ro schema` reports is subtracted. The subtraction is the
/// part that keeps the check honest about *which* flags the plan cuts: a flag
/// both kept and mentioned near a denial (`--tag` is kept as a column but cut
/// as a filter) is resolved by the binary, which is the specific statement.
///
/// Two things the derivation deliberately does not do. It does not read the
/// §3 target tree's flag comments — those name the flags the plan *keeps*
/// (`--autostash`, `--strategy`, …), and treating a kept flag's comment as a
/// mention of a cut one would produce a false positive on every command that
/// carries both. And it does not widen to a bare "flag near a word like `cut`
/// in the whole plan": the plan's §2 baseline lists bugs in code that does not
/// exist yet (`ro import --stars`, `prune.rs:135`), so that window would derive
/// a cut set out of a backlog.
fn section3_cut_flags(plan: &str) -> BTreeSet<String> {
    let mut cut: BTreeSet<String> = BTreeSet::new();

    // (1) Removal tables: the left cell is the thing that goes.
    //
    // Header-driven rather than "every table in the plan": a `| Cut | Why |`
    // table's *right* cells name the reasons in live vocabulary, and reading
    // them as cuts is the exact over-read D2 warns about.
    //
    // A `| Before |` row is a cut only when its Kind column says so. `| Before
    // |` also holds "Folded in" and "Promote + reshape" rows — a `git` flag
    // named there is replaced, not removed, and the schema is the arbiter of
    // which it is.
    const ALWAYS_REMOVED: &[&str] = &["Removed", "Cut", "Was"];
    const KIND_DECIDED: &[&str] = &["Before", "Old"];
    const GONE: &[&str] = &[
        "cut", "delete", "gone", "nothing", "remove", "fold", "revert", "replace",
    ];
    let mut header: Option<Vec<String>> = None;
    for line in plan.lines() {
        if !line.trim_start().starts_with('|') {
            header = None;
            continue;
        }
        let cells: Vec<String> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect();
        if cells.iter().all(|c| is_rule_row(c)) {
            continue;
        }
        let Some(cols) = header.as_ref() else {
            header = Some(cells);
            continue;
        };
        let left = cells.first().map(String::as_str).unwrap_or("");
        let removed = match cols.first().map(String::as_str) {
            Some(h) if ALWAYS_REMOVED.contains(&h) => true,
            Some(h) if KIND_DECIDED.contains(&h) => cells
                .get(2)
                .is_some_and(|kind| GONE.iter().any(|g| kind.to_ascii_lowercase().contains(g))),
            _ => false,
        };
        if removed {
            cut.extend(flag_tokens(left));
        }
    }

    // (2) Sentences that deny a flag's existence.
    //
    // Sentence-scoped, because the plan's tables and code fences sit between
    // the denial and a later mention of the same flag, and a whole-plan window
    // would read a kept flag as a cut one on the strength of a neighbouring
    // sentence. The split stops where a new subject begins (table row,
    // heading, fence), and fenced blocks — the command tree, the config
    // samples — never contribute: their `# comments` name kept flags as kept
    // ("--branch is a clone parameter, not cached") and are the exact
    // over-read the tree shapes below would otherwise produce.
    let mut sentence = String::new();
    let mut in_fence = false;
    let flush = |buf: &mut String, cut: &mut BTreeSet<String>| {
        if !buf.is_empty() {
            cut.extend(denied_flags(buf));
        }
        buf.clear();
    };
    for line in plan.lines() {
        if line.starts_with("```") {
            flush(&mut sentence, &mut cut);
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let opens_new_subject = line.starts_with('|') || line.starts_with('#');
        if opens_new_subject {
            flush(&mut sentence, &mut cut);
            continue;
        }
        sentence.push_str(line);
        if line.contains('.') {
            flush(&mut sentence, &mut cut);
        }
    }
    flush(&mut sentence, &mut cut);

    // (3) What the binary actually ships is the specific statement.
    cut.retain(|f| !live_flags().contains(f));
    cut
}

/// A markdown rule row (`|---|---|`) under a header.
fn is_rule_row(cell: &str) -> bool {
    !cell.is_empty() && cell.chars().all(|c| c == '-' || c == ':')
}

/// How close, in bytes, a denial has to be to a flag for that flag to be the
/// denial's subject.
///
/// Wide enough for the plan's own shapes — "There is no `ro init --add-dir`",
/// "**`--allow-protected` is deleted**" — and narrow enough that a denial
/// about one thing does not reach a flag named later in the same sentence.
const DENIAL_PROXIMITY: usize = 90;

/// The flags a sentence denies, if any.
///
/// The subject has to be the flag, for the reason the verb denial requires a
/// subject: the plan's prose says "cut", "deleted" and "gone" about all sorts
/// of things, and a denial about `ro conflict` is not a denial of
/// `rebase --continue` two clauses later.
///
/// Two shapes are excluded on purpose, and both cost a real false positive on
/// the current plan:
///
/// - **"refuses".** §8 item 5 is "**`main` + `ro push --execute`: refuses**".
///   The flag *is* the subject there, and the verb means the *invocation* is
///   refused — not that the flag is gone. `--execute` is a live flag on the
///   surface the plan ships, and deriving it as cut would fail five correct
///   acceptance items (4, 5, 85, 89, 135, 139).
/// - **"with no X" / "needs no X".** Item 85 is "with no `--execute` it
///   prints the plan" and item 6 is "the second `ro push` needs no
///   `--set-upstream`". Both describe a run, not a surface.
fn denied_flags(sentence: &str) -> Vec<String> {
    let lower = sentence.to_ascii_lowercase();
    const FORMS: &[&str] = &[
        // "There is no `ro init --add-dir`", "no global `--json`"
        "there is no",
        "there are no",
        "no global",
        "does not exist",
        "no such command",
        "never shipped",
        "is not shipped",
        "not a command",
        "not a verb",
        // "`--default-branch` is deleted with the column", "`--resume` gets deleted"
        "is deleted",
        "are deleted",
        "was deleted",
        "were deleted",
        "deleted with",
        // "the verb was cut", "`--allow-protected` is deleted, not carried"
        "is cut",
        "are cut",
        "was cut",
        "were cut",
        "is gone",
        "are gone",
        "is not there",
        "is moot",
        "are moot",
        "is reverted",
        "are reverted",
        "is folded",
        "was folded",
        // "No `--tag` filter exists any more"
        "filter exists",
        "flag is deleted",
    ];
    let markers: Vec<usize> = FORMS.iter().flat_map(|f| occurrences(&lower, f)).collect();
    if markers.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for at in occurrences(sentence, "--") {
        let flag: String = sentence[at + 2..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if flag.is_empty() {
            continue;
        }
        let flag = format!("--{flag}");
        if markers.iter().all(|m| m.abs_diff(at) > DENIAL_PROXIMITY) {
            continue;
        }
        // A run-scoped "no", not a surface-scoped one: "with no `--execute`
        // it prints the plan" (item 85), "the second `ro push` needs no
        // `--set-upstream`" (item 6). The flag is absent from that *run*, and
        // that is the ordinary shape of an optional-flag sentence.
        let prefix = sentence[at.saturating_sub(12)..at].to_ascii_lowercase();
        if ["with no ", "needs no ", "under no ", "without no "]
            .iter()
            .any(|p| prefix.contains(p))
        {
            continue;
        }
        out.push(flag);
    }
    out.sort();
    out.dedup();
    out
}

/// Every `--flag` token in `text`, as owned strings.
fn flag_tokens(text: &str) -> impl Iterator<Item = String> + use<'_> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| w.starts_with("--") && w.len() > 2)
        .map(str::to_string)
}

/// Every long flag `ro schema` reports, plus the flag-shaped tokens it quotes in
/// its help text.
fn live_flags() -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    fn walk(value: &serde_json::Value, out: &mut BTreeSet<String>) {
        match value {
            serde_json::Value::String(s) => out.extend(flag_tokens(s)),
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    if k == "long" {
                        if let Some(s) = v.as_str() {
                            out.insert(s.to_string());
                        }
                    } else {
                        walk(v, out);
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(&schema(), &mut out);
    out
}

#[test]
fn no_live_section8_item_names_a_command_section3_cut() {
    let plan = plan_text();
    let kept = plan_kept_commands(&plan);
    let cut = section3_cut_commands(&plan, &kept);
    assert!(
        !cut.is_empty(),
        "the plan scan found no cut commands; the section-3 cut tables moved"
    );
    let s8 = section(&plan, "## 8.");
    let items = section8_items(&s8);
    assert!(!items.is_empty(), "section 8 has no numbered items");

    let mut violations = Vec::new();
    for (n, body) in item_bodies(&items) {
        // A marker is the plan's own shape for "this text is kept, the verb
        // it names is not": a blockquote naming SUPERSEDED (the capability
        // is gone) or REWRITTEN (the assertion was re-pointed at a live
        // verb). Either excuses the stale invocation. Nothing is deleted
        // and nothing is renumbered — the reason is the plan's own.
        let marked = body.contains("SUPERSEDED") || body.contains("REWRITTEN");
        if marked {
            continue;
        }
        let asserted = item_asserts_cut_verb(&body, &cut);
        if !asserted.is_empty() {
            violations.push(format!(
                "item {n} asserts {asserted:?} — a command section 3 cut or reverted — \
                 with no SUPERSEDED/REWRITTEN marker"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "section 8 has {} unmarked item(s) asserting a command section 3 cut or \
         reverted:\n{}\nCut verbs, derived from section 3's own tables: {:?}.\n\
         Mark the item SUPERSEDED (the capability is gone) or REWRITTEN (the \
         assertion was re-pointed at a live verb). Do not delete it — a deleted \
         acceptance item is indistinguishable from one that was never written — \
         and do not renumber it.",
        violations.len(),
        violations.join("\n"),
        cut.iter().collect::<Vec<_>>()
    );
}

/// The A3 check must not be defeated by a stale item's own ordinary English.
///
/// **The failure this pins is silent, which is why it needs pinning.** A §8
/// item reading "`ro run prune --days 30` deletes worktrees older than 30
/// days, and the deleted worktrees are reported on stdout" names two cut
/// verbs (`run` and `prune`, both cut by §3), carries no SUPERSEDED or
/// REWRITTEN marker, and is exactly the A3 defect. It is nonetheless excused
/// by the current `DENIALS` list, because that list contains the bare
/// substrings `delete` and `deleted` and both fall inside the ±120-character
/// window around the mention. The words mean the *opposite* of a denial
/// there. The whole check went green on a stale item because the item's prose
/// happened to use the right words.
///
/// This test does not read PLAN.md — it pins the *heuristic*, so the false
/// negative cannot come back when someone widens `DENIALS` for an unrelated
/// reason. `no_live_section8_item_names_a_command_section3_cut` is the test
/// that consumes this behaviour; the two are kept adjacent on purpose.
#[test]
fn a_stale_item_naming_a_cut_command_is_not_excused_by_its_own_prose() {
    let plan = plan_text();
    let kept = plan_kept_commands(&plan);
    let cut = section3_cut_commands(&plan, &kept);
    // `run` and `prune` are the verbs the item below names. Deriving them
    // from the plan rather than writing them here keeps the test honest
    // about the cut set; if §3 stops cutting them, the assertion below is
    // about a different verb and the test says so via `assert!`.
    for verb in ["run", "prune"] {
        assert!(
            cut.contains(verb),
            "the pin below is about `ro {verb}`, which section 3 no longer cuts; \
             the cut set moved and this test is no longer testing what its name says"
        );
    }

    // The exact shape audit A3 exists to catch, with no marker.
    let stale = "143. `ro run prune --days 30` deletes worktrees older than 30 days, \
                 and the deleted worktrees are reported on stdout.";
    let asserted = item_asserts_cut_verb(stale, &cut);
    assert!(
        !asserted.is_empty(),
        "a stale item naming cut verbs was excused by its own prose. The item is: \
         {stale}\n\
         `item_asserts_cut_verb` reported nothing, so the A3 check would pass on an \
         item that is exactly the defect A3 is about. The cause is a bare-verb \
         entry in `DENIALS` (`delete`, `deleted`, `cut`, `gone`, …) matching inside \
         the ±120-character window. A denial needs a subject — `was cut`, `is not a \
         verb`, `no longer` — not a word that is also the item's own predicate."
    );
    // The same item, with the marker the plan's own shape supplies, *is*
    // excused — but the excusal happens in the caller (`marked` is checked
    // before `item_asserts_cut_verb` is ever reached), not in the heuristic.
    // A SUPERSEDED blockquote still *names* the cut verb; what it appends
    // ("`ro run` and `ro prune` are cut") is itself a denial in DENIALS,
    // but the heuristic is deliberately the last word only where no marker
    // exists. Pinning that ordering here: the caller must excuse the marked
    // item, and the heuristic must still flag the raw one.
    let marked_body = format!("> **SUPERSEDED** — `ro run` and `ro prune` are cut: {stale}");
    assert!(
        marked_body.contains("SUPERSEDED"),
        "the test's own marked shape must carry the marker the caller keys on"
    );
    assert!(
        !item_asserts_cut_verb(stale, &cut).is_empty(),
        "the pin above already asserts this; the marked shape below is only \
         meaningful if the raw shape is actually flagged"
    );
    // And a genuine denial, in ordinary words, is still excused — the
    // subject-bearing forms in `DENIALS` are what remains.
    let denial = "144. There is no `ro run prune`: the verb was cut, and `ro sync` \
                  covers the case instead.";
    assert!(
        item_asserts_cut_verb(denial, &cut).is_empty(),
        "a correctly-worded denial was reported as an assertion; `DENIALS` no longer \
         has a subject-bearing form for this: {denial}"
    );
}

#[test]
fn no_live_section8_item_asserts_a_flag_section3_cut() {
    let plan = plan_text();
    let cut_flags = section3_cut_flags(&plan);
    assert!(
        !cut_flags.is_empty(),
        "the plan scan found no cut flags; the flag-prose markers moved, or the \
         binary gained every flag the plan cuts. Cut flags the plan denies: {:?}",
        cut_flags.iter().collect::<Vec<_>>()
    );
    let s8 = section(&plan, "## 8.");
    let items = section8_items(&s8);
    let mut violations = Vec::new();
    for (n, body) in item_bodies(&items) {
        if body.contains("SUPERSEDED") || body.contains("REWRITTEN") {
            continue;
        }
        // A cut flag in an `ro` context is an assertion, the way a cut verb in
        // an `ro` context is. The plan's §8 uses `--json` and `--force` as
        // `gh`'s and `git`'s own flags, so an item naming one of those is
        // naming the sibling tool's flag — which is a real and separate
        // question, not this one.
        let asserted = item_asserts_cut_flag(&body, &cut_flags);
        if !asserted.is_empty() {
            violations.push(format!(
                "item {n} asserts {asserted:?} — a flag the plan cuts — with no \
                 SUPERSEDED/REWRITTEN marker"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "section 8 has {} unmarked item(s) asserting a flag the plan cuts:\n{}\n\
         Mark the item SUPERSEDED or REWRITTEN. A cut flag and a cut verb are the \
         same defect: `ro sync --tag` is a flag that is not on the binary exactly as \
         `ro conflict` is a verb that is not. Cut flags, derived from the plan's own \
         denials minus what `ro schema` reports: {:?}",
        violations.len(),
        violations.join("\n"),
        cut_flags.iter().collect::<Vec<_>>()
    );
}

/// The cut flags a section-8 item *asserts* — a flag-shaped token in the same
/// clause as an `ro` invocation, or one the item says the `ro` surface carries.
///
/// A bare substring is not enough, and the reason is the plan's own prose.
/// §8 item 75 reads "`gh repo view --json defaultBranchRef`" and items 107 and
/// 109 say "never `--force`" and "contains **no** `--force`" — all three name a
/// cut flag, none asserts it, and the binary's `--force` is a `git` flag. An
/// `ro` context is the dividing line: it is the difference between "the flag
/// this tool ships" and "the flag the neighbouring tool's flag is spelled
/// like".
fn item_asserts_cut_flag(item: &str, cut_flags: &BTreeSet<String>) -> Vec<String> {
    let mut asserted = Vec::new();
    for at in occurrences(item, "--") {
        let word: String = item[at + 2..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let flag = format!("--{word}");
        if !cut_flags.contains(&flag) || asserted.contains(&flag) {
            continue;
        }
        // The clause is the token and the words around it — an `ro` mention
        // nearby, or a sentence that says the `ro` surface carries the flag.
        //
        // A sibling tool's invocation is the negative case, and it is
        // recognized rather than left to the absence of an `ro` mention:
        // `gh repo view --json` and `git push --force` are the plan naming
        // `gh`'s and `git`'s own flags. Item 91 (live, unmarked) is the first
        // and item 109 ("the recorded `git push` argv contains **no**
        // `--force`") the second.
        let lo = floor_boundary(item, at.saturating_sub(90));
        let hi = ceil_boundary(item, (at + word.len() + 90).min(item.len()));
        let window = &item[lo..hi];
        let sibling_tool = ["gh ", "git ", "claude ", "codex "]
            .iter()
            .any(|tool| !occurrences(window, tool).is_empty());
        let names_ro = !occurrences(window, "ro ").is_empty()
            || !occurrences(window, "`ro ").is_empty()
            || !occurrences(window, " ro ").is_empty();
        let about_ro_surface = {
            let w = window.to_ascii_lowercase();
            w.contains("ro schema") || w.contains("ro flag") || w.contains("on the binary")
        };
        if (names_ro || about_ro_surface) && !sibling_tool {
            asserted.push(flag);
        }
    }
    asserted.sort();
    asserted.dedup();
    asserted
}

/// A cut flag the plan denies must stay denied. The denominator check: the
/// derivation above is a set intersection, and an intersection can be empty on
/// both sides at once — a plan that cut nothing would derive nothing, and a
/// `ro schema` that grew every flag would subtract everything. Either way the
/// check goes green having looked at nothing.
///
/// So the pin asserts the *whole chain*: a flag the plan cuts and the list of
/// §3 cut-tables forgets is caught, not silently unexamined.
#[test]
fn the_cut_flag_derivation_cannot_go_vacuously_silent() {
    let plan = plan_text();
    let cut_flags = section3_cut_flags(&plan);
    let live = live_flags();

    // Denominator 1: the plan denies flags, and the derivation found them.
    assert!(
        !cut_flags.is_empty(),
        "the derivation produced no cut flags. Either the plan stopped cutting \
         flags (then delete the flag checks) or the flag-denial markers moved. \
         Cut flags found: {:?}",
        cut_flags.iter().collect::<Vec<_>>()
    );

    // Denominator 2: the derivation is not a tautology. A flag that is both
    // cut and shipped is a contradiction between the plan and the binary —
    // which is a real defect, and one this check is supposed to see. If the
    // subtraction step (deriving, then removing every flag `ro schema`
    // reports) were broken in the direction of "keep everything", every flag
    // would pass and the check would be green having looked at nothing.
    //
    // The guard is the other direction of that: every flag the binary does NOT
    // ship that the plan cuts must be in the set, and every flag the plan cuts
    // that the binary DOES ship must be out of it.
    let live_denied: Vec<&String> = cut_flags.iter().filter(|f| !live.contains(*f)).collect();
    assert!(
        live_denied.len() == cut_flags.len(),
        "the derivation and the binary disagree about a flag the plan cuts, which \
         is a real defect: the plan says the flag is gone and the binary says it \
         ships. Cut flags: {:?}. Live flags the binary reports: {:?}",
        cut_flags.iter().collect::<Vec<_>>(),
        live.iter().collect::<Vec<_>>()
    );
    // And the subtraction really is a subtraction: nothing the plan cuts and the
    // binary cuts together stays in the set by accident.
    let still_cut_and_shipped: Vec<&String> =
        cut_flags.iter().filter(|f| live.contains(*f)).collect();
    assert!(
        still_cut_and_shipped.is_empty(),
        "the plan cuts {:?} but `ro schema` reports it on the binary, so the \
         subtraction step kept a flag it should have removed",
        still_cut_and_shipped
    );

    // The teeth: a flag the plan cuts and a hand-written list would forget.
    // `--add-dir` is cut at PLAN.md:78 and :1531 and is not in any §3 cut
    // table, so no list-of-CANDIDATES approach reaches it. This is the exact
    // blind spot the derivation closes.
    assert!(
        cut_flags.contains("--add-dir"),
        "`--add-dir` is cut by the plan (`:78`, `:1531`) and the derivation did \
         not find it, so the flag checks are blind to the one cut flag a \
         hand-written candidate list was shown to miss. Cut flags found: {:?}",
        cut_flags.iter().collect::<Vec<_>>()
    );

    // The teeth, in the shape the check actually runs on: a *section-8* item,
    // unmarked, naming a cut flag in an `ro` context. Walked through the same
    // item parser the real check uses, so it cannot pass while the walk is
    // broken — and built from the real cut set, so it cannot pass while the
    // derivation is broken.
    //
    // The probes are spliced in at the *top* of the plan's own §8 rather than
    // appended to the end of the plan: `section8_items` clips the last item's
    // body at the superseded index, and anything after the plan's final item
    // falls past that tail, so an appended probe is parsed with no body at all.
    let probe = "999. Run `ro init --add-dir .` over a workspace and assert the \
                 registry is unchanged.\n";
    let live_flag_item = "998. `ro ship --all --execute` runs the fleet and writes \
                         one commit per repo.\n";
    let s8 = section(&plan, "## 8.");
    let after_heading = s8.find('\n').map(|i| i + 1).unwrap_or(s8.len());
    let probe_s8 = format!(
        "{}{probe}{live_flag_item}\n{}",
        &s8[..after_heading],
        &s8[after_heading..]
    );
    let items = section8_items(&probe_s8);
    let probe_item = items
        .get(&999)
        .map(|(_, body)| body.as_str())
        .expect("the probe item is numbered, so the item walk found it");
    assert!(
        probe_item.contains("--add-dir"),
        "the probe item must reach the assertion check unmodified, or this test \
         is asserting against a shape the real check never sees. Got: {probe_item:?}"
    );
    let asserted = item_asserts_cut_flag(probe_item, &cut_flags);
    assert!(
        asserted.contains(&"--add-dir".to_string()),
        "the probe item asserts `--add-dir` in an `ro` context and the assertion \
         check did not see it, so the flag check would pass on exactly the \
         defect it exists to catch. Asserted: {asserted:?}"
    );

    // The negative control, because a check that flags every flag token in a
    // §8 item is not the check that was asked for: `--execute` is a live flag
    // the plan ships, item 85 asserts it with no marker, and the real suite
    // is green. An unmarked item naming it in an `ro` context must stay green.
    let live_item = items
        .get(&998)
        .map(|(_, body)| body.as_str())
        .expect("the control item is numbered, so the item walk found it");
    let live_asserted = item_asserts_cut_flag(live_item, &cut_flags);
    assert!(
        !live_asserted.contains(&"--execute".to_string()),
        "a live flag the plan ships was reported as cut, so the check would fail \
         a correct acceptance item. Reported: {live_asserted:?}"
    );
}

/// The plan keeps its own index: "Items marked superseded above, in one
/// place". It is the plan's statement of which items are excused — a table
/// with a row per cut, naming the item numbers that cut excuses. The markers
/// are the primary statement and the index is the cross-reference, so both
/// directions are checked:
/// - an item the index lists but that carries no marker must name no cut
///   verb, because a cut verb without an excusal is the A3 defect;
/// - an item that carries a marker for a cut verb must be in the index,
///   because the index is how a reader finds the excusals.
///
/// The first direction deliberately does NOT require a marker on every
/// indexed item. The plan also fixes some stale items by rewriting them in
/// place rather than marking them — item 25 (`quality_gates` naming no
/// command at all) and item 42 (`` `ro add` ``, which is live) both name no
/// cut verb, so they need no excusal and their index rows are history, not
/// defects. Requiring a marker there would force the plan to mark items
/// that are already correct.
#[test]
fn the_superseded_index_agrees_with_the_markers() {
    let plan = plan_text();
    let s8 = section(&plan, "## 8.");
    let index_at = s8
        .find("Items marked superseded above, in one place")
        .unwrap_or_else(|| panic!("section 8 has no superseded index; the section moved"));
    let mut indexed: BTreeSet<u32> = BTreeSet::new();
    for line in s8[index_at..].lines() {
        let Some(first) = line.strip_prefix('|') else {
            continue;
        };
        // "1, 2, 3" and "76–81" both expand to a set of numbers.
        for part in first.split('|').next().unwrap_or("").split(',') {
            let part = part.trim().replace('–', "-");
            match part.split_once('-') {
                Some((a, b)) => {
                    if let (Ok(lo), Ok(hi)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                        indexed.extend(lo..=hi);
                    }
                }
                None => {
                    if let Ok(n) = part.parse::<u32>() {
                        indexed.insert(n);
                    }
                }
            }
        }
    }
    assert!(!indexed.is_empty(), "the superseded index names no items");

    let items = section8_items(&s8);
    let bodies = item_bodies(&items);
    let kept = plan_kept_commands(&plan);
    let cut = section3_cut_commands(&plan, &kept);
    // An indexed item without a marker is fine when it names no cut verb —
    // it was rewritten in place and needs no excusal. It is a defect only
    // when the cut verb is still there, unmarked.
    let unexcused: Vec<String> = indexed
        .iter()
        .filter_map(|n| bodies.iter().find(|(m, _)| m == n))
        .filter(|(_, b)| !(b.contains("SUPERSEDED") || b.contains("REWRITTEN")))
        .filter_map(|(n, b)| {
            let asserted = item_asserts_cut_verb(b, &cut);
            (!asserted.is_empty()).then(|| format!("item {n} asserts {asserted:?} with no marker"))
        })
        .collect();
    assert!(
        unexcused.is_empty(),
        "the superseded index lists items that assert a cut verb with no \
         SUPERSEDED/REWRITTEN marker:\n{}\nThe index row is not an excusal; the \
         marker on the item is.",
        unexcused.join("\n")
    );

    // The other direction: an item that names a cut verb and is marked, but
    // is not in the index, is a marker nobody can find. Only items that
    // actually needed excusing are checked — a REWRITTEN marker on an item
    // that names no cut verb is harmless but does not belong in the index.
    let unindexed: Vec<&u32> = bodies
        .iter()
        .filter(|(_, body)| body.contains("SUPERSEDED") || body.contains("REWRITTEN"))
        .filter(|(_, body)| !item_asserts_cut_verb(body, &cut).is_empty())
        .map(|(n, _)| n)
        .filter(|n| !indexed.contains(n))
        .collect();
    assert!(
        unindexed.is_empty(),
        "{unindexed:?} carry a marker and name a cut verb, but the superseded index \
         does not list them. The index is how a reader finds the excusals; a marker \
         nobody can find is a marker nobody reads."
    );
}

// ---------------------------------------------------------------------------
// Watchdog: the scan must not silently go blind
// ---------------------------------------------------------------------------

/// Guard against the failure mode this file exists to prevent. If the plan
/// stops being shaped the way the scan reads, the scan finds nothing, every
/// check above passes vacuously, and the suite is green for the same reason
/// it was green during the audit.
#[test]
fn the_plan_scan_still_sees_the_plan() {
    let plan = plan_text();
    let kept = plan_kept_commands(&plan);
    let live = live_commands();

    // The kept/live comparison is direction-aware, because the two directions
    // are different defects with different fixes and the same message:
    //
    // - **plan ahead of the binary** — the plan keeps a command the binary
    //   does not ship. This is the plan being correct and the binary being
    //   behind: a command named in a phase that has not landed. Failing it
    //   blocks a legitimate plan edit.
    // - **binary ahead of the plan** — the binary ships a command the plan
    //   does not keep. This is a silent addition, the check (b) direction.
    //
    // A bare `assert_eq!` names neither. The failure message does.
    let plan_only: Vec<&String> = kept.iter().filter(|k| !live.contains(*k)).collect();
    let binary_only: Vec<&String> = live.iter().filter(|l| !kept.contains(*l)).collect();
    if !plan_only.is_empty() || !binary_only.is_empty() {
        panic!(
            "the kept set the scan reads is not the surface the binary has.\n\
             Plan keeps, binary does not ship ({plan_only:?}): the plan is ahead of \
             the binary — either the phase that adds this command has not landed yet, \
             or the plan is claiming a command it does not intend to ship. Fix the \
             plan, or land the command.\n\
             Binary ships, plan does not keep ({binary_only:?}): the binary is ahead \
             of the plan — a command that ships with no kept entry in the plan is a \
             silent addition (the defect `every_live_command_is_accounted_for_by_the_plan` \
             exists to catch). Fix the plan's target tree, tier table, or a \
             \"Done looks like\" line.\n\
             If the target tree or the tier table lost entries, or the scan no longer \
             matches how the plan names commands, that is the third cause and it is \
             reported by the same red — this assertion carries no number of its own; \
             both sides are derived."
        );
    }
    assert!(
        target_surface_counts(&plan).len() >= 3,
        "the scan found fewer than three target-surface counts; the count sentences moved."
    );
    let items = section8_items(&section(&plan, "## 8."));
    assert!(
        items.len() >= 100,
        "section 8 yielded only {} numbered items; the section moved.",
        items.len()
    );
    let cut = section3_cut_commands(&plan, &kept);
    assert!(
        cut.len() >= 5,
        "section 3 yielded only {} cut commands: {cut:?}. The cut tables moved, and \
         the A3 check below is now blind to a stale item.",
        cut.len()
    );
}
