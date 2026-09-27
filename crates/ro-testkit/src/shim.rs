//! Cross-platform fake binaries.
//!
//! # Why this is not just "write a shell script"
//!
//! A shell script is not directly executable on Windows, and the failure
//! mode is the bad kind: on a Unix-only CI run every test using a shim
//! passes, so the fixture looks proven right up until the Windows job
//! silently does not run it. A fixture that quietly skips two thirds of the
//! matrix is worse than no fixture, because it is counted as coverage.
//!
//! So: a `gh` shell script on Unix, a **`gh.cmd` on Windows**. On Unix the
//! shell script is found and launched by bare name, so the same code that
//! spawns `gh` on macOS spawns the shim on Windows with no `#[cfg]` at the
//! call site — except that Windows does **not** launch it by bare name. A
//! probe on a real runner showed `Command::new("gh")` with the shim first on
//! `PATH` running the machine's real `gh.exe` and never looking at
//! `gh.cmd`. So on Windows a shim is spawned through
//! [`FakeBinary::program`], which resolves it. The fixture is portable; the
//! *spelling* of the spawn is not.
//!
//! The shim appends its argv to a log file, because "the command was built
//! with the right arguments" is not the same claim as "the child saw them".
//! The failure being designed against everywhere in this repo is a value
//! that is set on a builder and never reaches the process, and only the
//! child can testify to what it received.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// A fake `gh`, `claude`, `codex` or any other binary ro shells out to.
///
/// The shim is written into its own directory, which callers put **first on
/// `PATH`**. Every invocation appends one line to [`log_file`].
pub struct FakeBinary {
    dir: OwnedDir,
    name: String,
}

/// The directory a shim lives in.
///
/// Two shapes because the two lifetimes differ: a constructor-made shim
/// cleans up after itself, while a caller-written one must outlive the
/// test and is leaked by the caller on purpose.
pub enum OwnedDir {
    /// Self-cleaning, from `FakeBinary::recording` and friends.
    Temp(TempDir),
    /// Caller-owned and deliberately leaked.
    Kept(PathBuf),
}

impl OwnedDir {
    pub fn path(&self) -> &std::path::Path {
        match self {
            OwnedDir::Temp(t) => t.path(),
            OwnedDir::Kept(p) => p.as_path(),
        }
    }
}

impl FakeBinary {
    /// A shim that records its argv and exits 0.
    ///
    /// This is the `gh` shape: ro asks a question, reads stdout, and the
    /// answer comes back as data. Use [`FakeBinary::with_stdout`] when the
    /// test needs a particular answer.
    pub fn recording(name: &str) -> Self {
        let dir = TempDir::new().expect("the shim dir is creatable");
        let log = dir.path().join("argv.log");
        let body = shim_body(
            r##"printf '%s--RO-SEP--' "$*" >> "$RO_TESTKIT_LOG""##,
            // `echo`, not `<nul set /p>`. A probe on a real runner
            // showed the latter eating the first two characters of
            // every write: `--version--RO-SEP--` arrived as
            // `-version--RO-SEP--`. `echo` appends a newline, which
            // the reader tolerates because it trims each record.
            //
            // `findstr "^"` copies stdin to stdout, and `>>` sends that to
            // the log. It is here because the engine cannot hand a
            // multi-paragraph prompt to a `.cmd` as an argv element —
            // `cmd.exe` has no way to carry a newline in an argument — so
            // on Windows the prompt arrives on **stdin** instead. Without
            // this line the shim would record an empty argv and the tests
            // asserting the prompt reached the child would report "never
            // invoked" for an agent that was invoked, with the prompt.
            // On Unix the prompt is in argv and this reads nothing, so
            // the record is the same shape on both platforms.
            r##"@echo off
echo %*>> "@LOGPATH@"
findstr "^">> "@LOGPATH@"
echo --RO-SEP-->> "@LOGPATH@"
exit /b 0"##,
            &log,
        );
        write_shim(dir.path(), name, &body);
        Self {
            dir: OwnedDir::Temp(dir),
            name: name.to_string(),
        }
    }

    /// A shim that records its argv, prints `stdout`, and exits 0.
    pub fn with_stdout(name: &str, stdout: &str) -> Self {
        let dir = TempDir::new().expect("the shim dir is creatable");
        let log = dir.path().join("argv.log");
        let body = shim_body(
            &format!("cat <<'RO_TESTKIT_EOF'\n{stdout}\nRO_TESTKIT_EOF"),
            &format!(
                "@echo off\r\necho {}\r\nexit /b 0",
                stdout.replace('\n', " ")
            ),
            &log,
        );
        write_shim(dir.path(), name, &body);
        Self {
            dir: OwnedDir::Temp(dir),
            name: name.to_string(),
        }
    }

    /// A shim that records its argv, prints `stdout`, and exits with `code`.
    ///
    /// The non-zero exit is the point of the third case: "gh is installed
    /// but not authenticated" looks exactly like "gh is not installed" to
    /// a caller that only checks `status.success()`, and those two need
    /// different messages.
    pub fn failing(name: &str, code: i32, stderr: &str) -> Self {
        let dir = TempDir::new().expect("the shim dir is creatable");
        let log = dir.path().join("argv.log");
        let body = shim_body(
            &format!("cat >&2 <<'RO_TESTKIT_EOF'\n{stderr}\nRO_TESTKIT_EOF\nexit {code}"),
            // `1>&2` after the message, so the text goes to **stderr** and
            // the redirection is not part of it. Plain `echo` writes to
            // stdout, which is a different shim: the Unix body above sends
            // this same text to stderr, and a fixture that disagrees with
            // its own contract between platforms makes every assertion
            // about "what the agent said" platform-dependent.
            &format!(
                "@echo off\r\necho {} 1>&2\r\nexit /b {code}",
                stderr.replace('\n', " ")
            ),
            &log,
        );
        write_shim(dir.path(), name, &body);
        Self {
            dir: OwnedDir::Temp(dir),
            name: name.to_string(),
        }
    }

    /// A fake agent engine: it makes a real commit, like the engines do.
    ///
    /// Returns the working directory the shim commits in, read from
    /// `RO_TESTKIT_WORKDIR`. This is the shape the engine tests need — an
    /// agent that only *claims* to have committed would leave the
    /// credential test asserting against a tree nothing changed.
    pub fn agent_engine(name: &str) -> Self {
        let dir = TempDir::new().expect("the shim dir is creatable");
        let log = dir.path().join("argv.log");
        // A commit, so the tree actually changes. `-A` because that is what
        // the real engines do, and a fixture that stages less would pass a
        // test about untracked files for the wrong reason.
        let body = shim_body(
            r#"cd "$RO_TESTKIT_WORKDIR" || exit 1
git add -A
git commit -q -m "shim commit" || true"#,
            r#"@echo off
cd /d "%RO_TESTKIT_WORKDIR%"
git add -A
git commit -q -m "shim commit"
exit /b 0"#,
            &log,
        );
        write_shim(dir.path(), name, &body);
        Self {
            dir: OwnedDir::Temp(dir),
            name: name.to_string(),
        }
    }

    /// An engine that also tries to push — the boundary the plan forbids.
    ///
    /// A fake that only commits cannot prove the engine is denied the push;
    /// it can only prove nothing happened. The engine must *attempt* it, so
    /// that a future change letting the engine own the push shows up as the
    /// wrong remote receiving the commit rather than as a missing assertion.
    pub fn agent_engine_that_pushes(name: &str, remote: &str) -> Self {
        let dir = TempDir::new().expect("the shim dir is creatable");
        let log = dir.path().join("argv.log");
        let body = shim_body(
            &format!(
                r#"cd "$RO_TESTKIT_WORKDIR" || exit 1
git add -A
git commit -q -m "shim commit" || true
git push {remote} HEAD 2>>"$RO_TESTKIT_LOG" || true"#
            ),
            &format!(
                r#"@echo off
cd /d "%RO_TESTKIT_WORKDIR%"
git add -A
git commit -q -m "shim commit"
git push {remote} HEAD
exit /b 0"#
            ),
            &log,
        );
        write_shim(dir.path(), name, &body);
        Self {
            dir: OwnedDir::Temp(dir),
            name: name.to_string(),
        }
    }

    /// A hostile agent: dumps its argv, its whole environment and a
    /// directory listing, then attempts a push of its own.
    ///
    /// The push is the point. "The engine did not push" and "the engine
    /// had nowhere to push" are the same observation, and only a remote it
    /// *could* have reached settles it.
    ///
    /// Built through the same `shim_body` path as every other shim here,
    /// so it exists as a `.cmd` on Windows rather than a POSIX script that
    /// no Windows runner can execute. A fixture written as `#!/bin/sh` and
    /// named `claude` passes on macOS and fails on two thirds of the
    /// matrix — which is a fixture that only works where it was written.
    pub fn env_dumping_agent(name: &str, remote: &str) -> Self {
        let dir = TempDir::new().expect("the shim dir is creatable");
        let log = dir.path().join("agent-report.txt");
        let body = shim_body(
            &format!(
                r#"echo "=== ARGV ===" >> "$RO_TESTKIT_LOG"
echo "$*" >> "$RO_TESTKIT_LOG"
echo "=== ENV ===" >> "$RO_TESTKIT_LOG"
printenv >> "$RO_TESTKIT_LOG" 2>&1
echo "=== FILES ===" >> "$RO_TESTKIT_LOG"
ls -a >> "$RO_TESTKIT_LOG" 2>&1
echo "=== PUSH ===" >> "$RO_TESTKIT_LOG"
cd "$RO_TESTKIT_WORKDIR" 2>/dev/null && git push {remote} HEAD >> "$RO_TESTKIT_LOG" 2>&1
echo "=== END ===" >> "$RO_TESTKIT_LOG"
exit 0"#
            ),
            &format!(
                r#"@echo off
echo === ARGV ===>> "@LOGPATH@"
echo %*>> "@LOGPATH@"
echo === ENV ===>> "@LOGPATH@"
set>> "@LOGPATH@" 2>&1
echo === FILES ===>> "@LOGPATH@"
dir>> "@LOGPATH@" 2>&1
echo === PUSH ===>> "@LOGPATH@"
cd /d "%RO_TESTKIT_WORKDIR%"
git push {remote} HEAD>> "@LOGPATH@" 2>&1
echo === END ===>> "@LOGPATH@"
exit /b 0"#
            ),
            &log,
        );
        write_shim(dir.path(), name, &body);
        Self {
            dir: OwnedDir::Temp(dir),
            name: name.to_string(),
        }
    }

    /// What the env-dumping agent saw, as one string.
    pub fn env_report(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("agent-report.txt")).unwrap_or_default()
    }

    /// A shim that never returns.
    ///
    /// For testing the deadline, so the body has to be something the
    /// *host* can actually execute — hence here rather than a `#!/bin/sh`
    /// literal at the call site, which runs on macOS and cannot run on
    /// Windows at all.
    pub fn hanging(name: &str) -> Self {
        let dir = TempDir::new().expect("the shim dir is creatable");
        let log = dir.path().join("argv.log");
        let body = shim_body(
            r#"sleep 600"#,
            r#"@echo off
ping -n 600 127.0.0.1 > nul
exit /b 0"#,
            &log,
        );
        write_shim(dir.path(), name, &body);
        Self {
            dir: OwnedDir::Temp(dir),
            name: name.to_string(),
        }
    }

    /// A shim from a script the caller wrote, kept alive by a caller-owned
    /// directory.
    ///
    /// For a body that has to be exact — a hang, a specific non-zero exit —
    /// where the canned constructors are not enough. `keep_alive` is
    /// leaked by the caller, because a `TempDir` dropped at the end of the
    /// constructor would delete the script while the fixture still points
    /// at it.
    pub fn at(keep_alive: PathBuf, script: PathBuf, name: &str) -> Self {
        let dir = keep_alive;
        debug_assert!(
            script.starts_with(&dir),
            "the script must live in the directory kept alive, or the \
             shim is deleted out from under PATH"
        );
        Self {
            dir: OwnedDir::Kept(dir),
            name: name.to_string(),
        }
    }

    /// The directory this shim lives in.
    ///
    /// Public so a test can assert on PATH *under the lock* rather than
    /// guessing what a prepend looked like.
    pub fn dir_path(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// The command to spawn in order to run this shim.
    ///
    /// **A path, not the bare name, and that is not a style choice.** A
    /// probe run on a real `windows-latest` runner — after two fixes
    /// based on guessing — showed that `std::process::Command` does
    /// **not** apply `PATHEXT` to a bare name:
    ///
    /// ```text
    /// Command::new("claude")   with PATH prepended  => program not found
    /// Command::new("claude")   after env_clear      => program not found
    /// Command::new(".../claude.cmd")                => ok, out="ran"
    /// ```
    ///
    /// So a shim on Windows is reached by its file, and the shim is
    /// looked up on the caller's behalf. Resolving it here rather than at
    /// each call site means a test cannot forget and cannot get it subtly
    /// wrong on the one platform that needs it.
    pub fn program(&self) -> std::path::PathBuf {
        let file = self.file_name();
        if self.dir.path().is_absolute() {
            return self.dir.path().join(&file);
        }
        std::fs::canonicalize(self.dir.path().join(&file)).unwrap_or_else(|_| {
            // Not yet on disk. The plain join still names it, and a caller
            // that reaches this is about to get a spawn error naming the
            // path it tried.
            self.dir.path().join(&file)
        })
    }

    /// The file this shim actually is on this platform: `gh.cmd` on
    /// Windows, `gh` elsewhere.
    pub fn file_name(&self) -> String {
        if cfg!(windows) {
            format!("{}.cmd", self.name)
        } else {
            self.name.clone()
        }
    }

    /// This shim's name, e.g. `gh`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Run `body` with this shim first on `PATH`.
    ///
    /// Takes the lock and installs the guard **together**, in one call, and
    /// holds both until `body` returns or unwinds. That pairing is the
    /// whole reason this is not just "return a guard you can drop
    /// anywhere": an earlier version returned a bare `PathGuard` and the
    /// lock had to be taken separately, so a caller could — and a test in
    /// this very file did — drop the guard during a panic while the lock
    /// was already released, leaving `PATH` permanently swapped for every
    /// later test in the binary.
    ///
    /// # Safety
    ///
    /// `body` must not mutate `PATH` or process-wide env vars. It is free
    /// to spawn processes and to read the environment.
    pub unsafe fn with_on_path<R>(&self, body: impl FnOnce() -> R) -> R {
        // SAFETY: forwarded to TestEnv::install, which holds the one lock
        // for the whole call.
        unsafe { TestEnv::new().shim(self).run(body) }
    }

    /// The file the shim appends its argv to.
    pub fn log_file(&self) -> PathBuf {
        self.dir.path().join("argv.log")
    }

    /// Every argv the shim has seen, one entry per invocation.
    ///
    /// Split on newlines rather than parsed: a test asserting
    /// `invocations()[0].contains("--head")` should not have to care that
    /// argv is a list and not a string.
    pub fn invocations(&self) -> Vec<String> {
        let path = self.log_file();
        match std::fs::read_to_string(path) {
            // Split on the record separator, never on a newline: a prompt
            // legitimately contains newlines, and a newline-delimited log
            // would report one spawn as three.
            Ok(text) => text
                .split(RECORD_SEP)
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            // No log file means the shim never ran. Returning empty rather
            // than erroring lets a test say "assert it did not run", which
            // is a real assertion about several of these boundaries.
            Err(_) => Vec::new(),
        }
    }

    /// How many times the shim ran.
    pub fn run_count(&self) -> usize {
        self.invocations().len()
    }
}

/// Separates one recorded invocation from the next.
///
/// ASCII 0x1E (record separator): it cannot appear in an argument, so a
/// prompt containing any text at all round-trips through the log intact.
const RECORD_SEP: &str = "--RO-SEP--";

/// Builds a shim body for this platform.
///
/// The log path is baked in as a literal rather than read from an env var
/// the test must remember to set: a fixture that silently records nothing
/// because an env var was missing is a fixture that passes vacuously.
#[cfg(not(windows))]
fn shim_body(log_line: &str, windows: &str, log: &Path) -> String {
    let _ = windows;
    let log = log.display();
    format!(
        "#!/bin/sh\n\
         RO_TESTKIT_LOG='{log}'\n\
         export RO_TESTKIT_LOG\n\
         {log_line}\n"
    )
}

#[cfg(windows)]
fn shim_body(log_line: &str, windows: &str, log: &Path) -> String {
    let _ = log_line;
    // The log path is **inlined**, not passed through a variable. A
    // `.cmd` expands `%VAR%` at run time, and the probe on a real
    // `windows-latest` runner showed the variable form silently
    // producing an empty record — the shim ran, and wrote nothing, so a
    // test asserting on the log saw "never invoked" for a binary that had
    // been invoked. Inlining removes the hop that was eating the argv.
    let log = log.display();
    format!("@echo off\r\n{windows}\r\n").replace("@LOGPATH@", &log.to_string())
}

/// Write the shim under both names.
///
/// On Unix only `gh` is needed. On Windows a `.cmd` is the only thing that
/// can be *spawned*, so that is the file that matters; the extensionless
/// twin exists for `which`, which resolves by name.
fn write_shim(dir: &std::path::Path, name: &str, body: &str) {
    // Windows gets **two** files, and the reason is narrower than it looks.
    //
    // A `.cmd` is what `Command` can actually launch there. The
    // extensionless name is **not** launchable — Windows will not execute a
    // file with no extension — so it does not make a bare-name spawn work.
    // A probe on a real runner settled this: with the shim first on `PATH`,
    // `Command::new("gh")` ran the machine's real `gh.exe` and reported a
    // missing `GH_TOKEN`, having never looked at `gh.cmd` at all. Spawn a
    // shim through [`FakeBinary::program`], which is what the testkit's own
    // fixtures now do.
    //
    // The extensionless file stays for name resolution, which is a
    // different question from execution.
    #[cfg(windows)]
    let files = vec![dir.join(format!("{name}.cmd")), dir.join(name)];
    #[cfg(not(windows))]
    let files = vec![dir.join(name)];

    // A `.cmd` run by `cmd.exe` needs CRLF; a `#!/bin/sh` one needs LF.
    #[cfg(windows)]
    let body = &body.replace('\n', "\r\n");

    for file in &files {
        std::fs::write(file, body).expect("the shim is writable");
    }

    // A mode bit, on the platform that has one. Windows decides what is
    // executable by extension rather than by a permission bit, so there
    // is nothing to set there.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in &files {
            let mut perms = std::fs::metadata(file)
                .expect("the shim exists")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(file, perms).expect("the mode is settable");
        }
    }
}

/// Process-global state a test needs changed, installed and restored under
/// **one** lock.
///
/// # Why this exists instead of separate guards
///
/// `PATH` and the environment are both process-global, and both had to be
/// mutated together for the engine fixture (the shim on `PATH`, the
/// workdir in an env var). An earlier version exposed `with_on_path` and
/// `EnvGuard::with_set` separately, each taking the same non-reentrant
/// `Mutex`. Composing them — the natural way to write the engine test —
/// deadlocked, and the suite hung rather than failing.
///
/// So the two are installed by one call. Nesting is no longer expressible,
/// which is the point: the fix is structural, not a comment telling callers
/// not to do the obvious thing.
pub struct TestEnv {
    /// The directory to prepend to `PATH`, if any.
    shim_dir: Option<std::path::PathBuf>,
    vars: Vec<(String, String)>,
}

impl TestEnv {
    /// An empty environment: nothing to install.
    pub fn new() -> Self {
        Self {
            shim_dir: None,
            vars: Vec::new(),
        }
    }

    /// Put `shim`'s directory first on `PATH`.
    pub fn shim(mut self, shim: &FakeBinary) -> Self {
        self.shim_dir = Some(shim.dir.path().to_path_buf());
        self
    }

    /// Set an environment variable for the duration.
    pub fn var(mut self, key: &str, value: &str) -> Self {
        self.vars.push((key.to_string(), value.to_string()));
        self
    }

    /// Run `body` with everything installed, then restore.
    ///
    /// The restore runs **even if `body` panics**, which is the whole
    /// point: an earlier version restored with straight-line code after
    /// `body()`, so a panic skipped it and left `PATH` permanently
    /// swapped for every later test in the binary. A test of this exact
    /// property caught it.
    ///
    /// `catch_unwind` is what makes that true; the panic is then resumed
    /// so the caller's own reporting is unaffected.
    ///
    /// The `git` resolution is **forced before the lock is taken**, not
    /// after. That ordering is the whole structural half of the resolver's
    /// guarantee, and putting it here makes it a property of the lock
    /// rather than a property of the test suite: a fixture built inside
    /// `body` calls [`git_path`] on a thread that is about to hold
    /// `path_lock` for the duration of `body`, and a first call landing
    /// there deadlocks — the `OnceLock` would need the same non-reentrant
    /// lock the caller already has. Resolving first means the expensive
    /// and only `PATH`-consulting call has already happened before the
    /// guard exists, and the fixtures below can only ever read the cached
    /// answer. See the resolver's doc comment for why the alternatives
    /// (take the lock in the fixture; make the lock reentrant) are both
    /// worse.
    ///
    /// # Safety
    ///
    /// `body` must not mutate `PATH` or process-wide env vars.
    pub unsafe fn run<R>(self, body: impl FnOnce() -> R) -> R {
        // Force the one-time `git` resolution while no guard is installed.
        // Deliberately outside the `path_lock` critical section below —
        // it takes the same lock itself when it runs the initializer.
        let _git = git_path();

        // The one lock. Held until after the restore, so no other thread
        // can observe a half-installed environment.
        let _serialised = path_lock().lock().unwrap_or_else(|e| e.into_inner());

        let restore: Vec<(String, Option<String>)> = self
            .vars
            .iter()
            .map(|(k, v)| {
                let previous = std::env::var(k).ok();
                // SAFETY: the lock above, held for the whole call.
                unsafe { std::env::set_var(k, v) };
                (k.clone(), previous)
            })
            .collect();

        let previous_path = self.shim_dir.map(|dir| {
            let previous = std::env::var("PATH").ok();
            let current = previous.clone().unwrap_or_default();
            let sep = if cfg!(windows) { ';' } else { ':' };
            // SAFETY: the lock above, held for the whole call.
            unsafe { std::env::set_var("PATH", format!("{}{sep}{current}", dir.display())) };
            previous
        });

        // SAFETY: `R` may hold references into the process environment only
        // if the caller put them there, which this function's contract
        // forbids. A panic is resumed below with the environment restored,
        // so no value that survived the unwind depends on the swap.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));

        // Restore before the lock is released, and not via `Drop` — a Drop
        // impl would run *after* the lock guard is dropped, reintroducing
        // exactly the window this design exists to close.
        if let Some(previous) = previous_path {
            match previous {
                // SAFETY: same lock, still held.
                Some(p) => unsafe { std::env::set_var("PATH", p) },
                None => unsafe { std::env::remove_var("PATH") },
            }
        }
        for (key, previous) in restore {
            match previous {
                // SAFETY: same lock, still held.
                Some(v) => unsafe { std::env::set_var(&key, v) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }

        match outcome {
            Ok(value) => value,
            // Resuming here, not inside the catch, means the restore has
            // already happened when the caller's panic handler runs.
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

impl Default for TestEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Serialises every test that swaps `PATH` or mutates the environment.
///
/// `PATH` is process-global, so two shim tests running concurrently would
/// give one of them the other's fake. This is the whole reason the
/// testkit exists rather than each test doing its own `set_var`.
pub fn path_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

/// The absolute path to `git`, resolved **once** and cached for the life of
/// the process.
///
/// # The invariant, stated once so it is auditable
///
/// **Never spawn a bare `git` from inside ro-testkit.** The process-global
/// `PATH` is *mutable by design* — [`TestEnv::run`] rewrites it, under
/// [`path_lock`], for the whole body — and `Command::new("git")` resolves
/// the name through that global at *spawn* time. So a fixture built on a
/// thread that happens to be inside a `TestEnv` body is a `PATH` lookup
/// against whatever that body installed, and the whole test binary shares
/// one process and one `PATH`. That is the flake: `Worktree::empty` on one
/// thread dying with `git runs: Os { code: 2, kind: NotFound }` because a
/// test on another thread was asserting "this binary is not installed".
///
/// An absolute path has no lookup to lose. The scrub is still what it was —
/// the point is that no *Rust-side* fixture spawn consults it any more.
/// The exception is named, not hidden: the agent-engine shim *bodies* in
/// this file shell out to a bare `git` (the `git add -A` / `git commit` /
/// `git push` lines in `agent_engine`, `agent_engine_that_pushes` and
/// `env_dumping_agent`, Unix and `.cmd` variants alike), because a body is
/// generated text, not a `Command` the resolver could have built. That name
/// is looked up through the process-global `PATH` at *shim runtime*, so a
/// `PATH` scrub that straddles a running shim still starves the shell-side
/// lookup exactly the way the cached path starved-proofs the fixture side.
/// The tripwire in `worktree.rs` cannot catch them: it reads
/// `Command::new("git")`, and these are not `Command`s. Baking the resolver
/// into the generated bodies is left open — it ends the generated text's
/// platform-channel, which `shim_body` exists to keep narrow — and until it
/// happens the honest invariant is narrower than the blanket one: the
/// absolute path protects spawns, not shell bodies.
///
/// # Why the fixture side cannot just take the lock instead
///
/// The obvious competing fix — have `worktree::run` hold [`path_lock`] —
/// **self-deadlocks**, and the reasoning is worth keeping because it looks
/// so reasonable. The lock is a non-reentrant static `Mutex`, and
/// `TestEnv::run` holds it for the whole body. Fixtures are routinely built
/// *inside* such a body (an engine test builds a `Worktree`, then runs the
/// engine under a `PATH` guard; the same shape is in
/// `credential_never_reaches_engine.rs`, `credential_never_reaches_agent.rs`
/// and `tests/fixtures.rs`). A lock in the fixture is a lock ordering the
/// caller already holds.
///
/// A **reentrant** variant keyed on [`std::thread::ThreadId`] is worse than
/// no fix, and not because of anything about thread ids: they are a
/// monotonic process-wide counter, never reused, so "this thread already
/// holds the lock" would in fact be a *sound* answer. The objection is one
/// step further out. The only thing the lock is for is keeping a `PATH`
/// swap from straddling a read — and reentrancy means the inner holder is
/// the swap itself. It would let a fixture resolve `git` out of the very
/// `PATH` the enclosing `TestEnv` is holding it against, which is the
/// mis-resolution the absolute path exists to prevent, arriving through the
/// door that was added to stop the deadlock. A hang is a bad failure; a
/// fixture that quietly ran against a half-installed environment and failed
/// an unrelated assertion is worse, because nothing says the two are
/// related. So reentrancy is rejected on the ground that the lock protects
/// the read, not on any property of `ThreadId`.
///
/// # Why the one resolution still takes the lock
///
/// Exactly once, and never again. Reading `PATH` and running `git` are
/// separate steps, and a `TestEnv` body that begins between them installs a
/// `PATH` with no `git` in it. Snapshotting `PATH` under the lock makes that
/// window unrepresentable rather than merely unlikely, and after it the
/// answer is cached in a [`OnceLock`] so the whole "fixtures under a scrub"
/// shape costs nothing.
///
/// # Why the resolution cannot deadlock, and what that rests on
///
/// The obvious first version of this held `path_lock` across the *whole*
/// resolution, and it deadlocked: the lock is a non-reentrant static
/// `Mutex`, and a fixture built inside a `TestEnv::run` body is on a thread
/// that already holds it. So the lock is now taken to snapshot `PATH` and
/// released before the walk — which is only half the answer, because the
/// *snapshot* still wants the lock, and a cold call from inside a body still
/// wants it too.
///
/// The other half is that no call can be cold while the caller holds the
/// lock: [`TestEnv::run`] forces the resolution at its top, **before** it
/// takes `path_lock`. `path_lock` is only ever held by `TestEnv::run`, and
/// `TestEnv::run` always resolves first, so by the time any body executes
/// the `OnceLock` is populated and every later call is a cache read that
/// touches no lock at all. That is an ordering in code, not a convention in
/// a test file.
///
/// **The residual, stated because it is a real coupling:** a future holder of
/// `path_lock` that does not pre-warm reintroduces a hang, and the hang has
/// no message — the caller is stuck acquiring the lock, never reaching the
/// `git_on_path` walk or the panic below. `TestEnv::run` is the only holder
/// today. If a second one appears, it needs the same forced resolution at
/// its top, or the resolver needs to stop wanting the lock.
///
/// Once resolved, a scrub cannot reach the cached answer at all: the
/// `OnceLock` is written exactly once, `TestEnv` rewrites `PATH` for the
/// duration of a body and restores it after, and no second resolution
/// consults `PATH`.
///
/// # Panics
///
/// If `git` is genuinely absent from the snapshotted `PATH`. That is a broken
/// environment, and a panic naming it *here* — once, at the one place that
/// knows the answer — beats `NotFound` surfacing a hundred tests deep with the
/// wrong culprit. A `PATH` scrub installed by a *sibling* thread cannot
/// trigger it, because the snapshot waits for `path_lock`, which that
/// thread's `TestEnv::run` holds for its whole body: the scrubber is either
/// not running, or has already restored. A scrub cannot be visible to the
/// pre-warm in `TestEnv::run` either, for the same reason — and a scrub
/// cannot be visible *from* a body, because the body can only run after the
/// pre-warm has already resolved.
pub fn git_path() -> &'static Path {
    static RESOLVED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    RESOLVED.get_or_init(|| {
        // The window this closes: `TestEnv::run` swapping `PATH` between the
        // read and the result. So the *snapshot* is taken under the lock —
        // that is the one thing the lock is for, and holding it only for the
        // read is what makes this callable from inside a `TestEnv::run` body.
        //
        // The lock is **released before the walk**. An earlier version held it
        // across the whole resolution, which deadlocked: the lock is a
        // non-reentrant static `Mutex`, `TestEnv::run` holds it for its whole
        // body, and fixtures are routinely built *inside* such a body — so a
        // cold call from there re-locked a mutex the calling thread already
        // held and hung the suite. That was a real hang, reproduced and
        // confirmed, not a theoretical one.
        //
        // Releasing first is safe precisely because the snapshot was coherent:
        // `TestEnv::run` installs every var and its `PATH` swap inside this
        // same lock, so anything read under it is a `PATH` that was fully
        // installed at some instant. A `git` found there names a real binary,
        // and that fact survives any later scrub. The race this leaves — a
        // scrub landing between the snapshot and the walk — can only make the
        // walk *miss*, never make it resolve a different `git`.
        let path_var = {
            let _serialised = path_lock().lock().unwrap_or_else(|e| e.into_inner());
            std::env::var_os("PATH")
        };
        git_on_path(&path_var).unwrap_or_else(|| {
            panic!(
                "git was not found on PATH, so no testkit fixture can build a \
                 repository. This is a broken environment, not a test failure: \
                 every `Worktree` and `BareRemote` in the workspace needs it."
            )
        })
    })
}

/// The first `git` on the current `PATH`, Windows extensions included.
///
/// A `PATH` walk rather than a bare name, for two reasons. First, the point
/// of this module: the answer must be an absolute path that survives `PATH`
/// being rewritten afterwards. Second, on Windows a bare name is not a file,
/// it is a *family* of files decided by `PATHEXT` (`git.exe`, `git.cmd`, …).
/// Getting that wrong does not fail on the machine it was written on — it
/// fails on the Windows CI leg, where a fixture that "cannot find git" costs
/// the whole job. So the extensions tried are the platform's own list, read
/// from `PATHEXT` with the usual default, in the platform's own order — the
/// same rule the loader applies, minus the loader.
///
/// Empty `PATH` entries are skipped rather than read as the current
/// directory: a `git` found relative to wherever the test binary happens to
/// run is a fixture that depends on the runner's working directory, which is
/// exactly the "quietly depends on the machine" failure every other
/// constructor comment in this file is written against.
fn git_on_path(path_var: &Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path_var = path_var.as_ref()?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for candidate in git_candidates(&dir) {
            if is_runnable(&candidate) {
                // Canonicalised now, against *this* process's directory:
                // the fixture later sets `current_dir` on the command, and
                // a relative `PATH` entry would be re-resolved against the
                // new repository instead. The fallback keeps a lost race (a
                // temp dir removed under us) from turning into a panic; the
                // path we found still names a real `git`.
                return Some(std::fs::canonicalize(&candidate).unwrap_or(candidate));
            }
        }
    }
    None
}

/// The filenames `git` can have in `dir`: the bare name everywhere, plus
/// every `PATHEXT` spelling on Windows, in the platform's order.
fn git_candidates(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![dir.join("git")];
    if cfg!(windows) {
        // The loader's own list when present (`.COM;.EXE;.BAT;.CMD;…`),
        // the documented default when it is not. Either way the platform's
        // rule, not a guess made on a Unix machine about what Windows does.
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        for ext in pathext.split(';').map(str::trim).filter(|e| !e.is_empty()) {
            let ext = ext.strip_prefix('.').unwrap_or(ext);
            out.push(dir.join(format!("git.{ext}")));
        }
    }
    out
}

/// A file that can actually be spawned: present, a file (not a directory a
/// previous install left behind), and — where the platform has the bit —
/// marked executable.
fn is_runnable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
