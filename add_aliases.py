"""One-shot: one-release aliases for the removed command names.

Three edits, each an exact-text insertion, so a mismatch is an assertion
rather than a silent rewrite. (An earlier attempt used index arithmetic
and deleted eleven match arms; this one refuses to run unless every anchor
is found verbatim.)
"""
import io

P = "crates/ro/src/main.rs"
src = io.open(P, encoding="utf-8").read()

# 1. `ro health` -> `ro list`. A hidden alias, not a visible one: an alias in
#    `--help` is a second thing to read and a second thing to remember.
OLD_LIST = """    /// List tracked repos
    List {"""
NEW_LIST = """    /// List tracked repos
    ///
    /// `ro health` was this command. A hidden alias for one release, so an
    /// existing script keeps working.
    #[command(alias = "health")]
    List {"""
assert OLD_LIST in src, "the List variant anchor was not found"
src = src.replace(OLD_LIST, NEW_LIST, 1)

# 2. `ro robot-docs` -> `ro schema`.
OLD_SCHEMA = """    /// Machine-readable CLI reference, generated from the live command tree
    Schema,"""
NEW_SCHEMA = """    /// Machine-readable CLI reference, generated from the live command tree
    ///
    /// `ro robot-docs` was this command. Hidden alias, one release.
    #[command(alias = "robot-docs")]
    Schema,"""
assert OLD_SCHEMA in src, "the Schema variant anchor was not found"
src = src.replace(OLD_SCHEMA, NEW_SCHEMA, 1)

# 3. `ro sweep commit-sweep --execute` -> `ro ship`. The single most-used
#    invocation of the tool today.
OLD_SHIP_FIELD = """        /// Target repos by glob (e.g. "owner/*")
        #[arg(long)]
        pattern: Option<String>,
        filter: Option<String>,
        all: bool,
        engine: Option<String>,
        engine_bin: Option<String>,
        /// The branch the work should land on, when it is not the current one
        #[arg(long)]
        onto: Option<String>,"""
NEW_SHIP_FIELD = """        /// Target repos by glob (e.g. "owner/*")
        #[arg(long)]
        pattern: Option<String>,
        filter: Option<String>,
        all: bool,
        engine: Option<String>,
        engine_bin: Option<String>,
        /// The branch the work should land on, when it is not the current one
        #[arg(long)]
        onto: Option<String>,
        /// The old `ro sweep commit-sweep`, for one release.
        ///
        /// A flag rather than a hidden subcommand: clap's optional
        /// subcommands have to be enums, and an enum whose only purpose is
        /// to survive one release is more machinery than it earns. The
        /// translation is mechanical — `--execute` becomes the default,
        /// since the new verb opts out with `--dry-run` rather than in.
        #[arg(long, hide = true)]
        commit_sweep: bool,
        /// The old opt-in switch. Now the default.
        #[arg(long, hide = true)]
        execute: bool,"""
assert OLD_SHIP_FIELD in src, "the Ship field anchor was not found"
src = src.replace(OLD_SHIP_FIELD, NEW_SHIP_FIELD, 1)

# 4. The handler: the legacy spelling short-circuits into the same body.
OLD_ARM = """        Commands::Ship {
            repos: named,
            pattern: glob,
            filter,
            all,
            engine,
            engine_bin,
            onto,
            resolve,
            dry_run,
        } => ship::run_verb("""
NEW_ARM = """        Commands::Ship {
            repos: named,
            pattern: glob,
            filter,
            all,
            engine,
            engine_bin,
            onto,
            commit_sweep,
            execute,
            resolve,
            dry_run,
        } => {
            // The old `ro sweep commit-sweep` spelling, for one release.
            // A script that breaks on a rename is a script the user has to
            // read the release notes to fix.
            if commit_sweep {
                eprintln!(
                    "warning: ro sweep commit-sweep is now ro ship, and is removed \\
                     in the next release. This spelling still works for one release."
                );
                ship::run_verb(
                    &paths,
                    ship::HowFar::Ship,
                    /* named */ &[],
                    /* pattern */ None,
                    /* filter */ None,
                    /* all */ true,
                    /* engine */ None,
                    /* engine_bin */ None,
                    /* onto */ None,
                    /* resolve */ false,
                    /* dry_run */ !execute,
                );
            }
            ship::run_verb("""
assert OLD_ARM in src, "the Ship handler anchor was not found"
src = src.replace(OLD_ARM, NEW_ARM, 1)

# Close the new block: the original arm ended with `);`.
OLD_TAIL = """            onto.as_deref(),
            resolve,
            dry_run,
        ),"""
NEW_TAIL = """                onto.as_deref(),
                resolve,
                dry_run,
            );
        }"""
assert OLD_TAIL in src, "the Ship handler tail anchor was not found"
src = src.replace(OLD_TAIL, NEW_TAIL, 1)

io.open(P, "w", encoding="utf-8").write(src)
print("all four edits applied")
