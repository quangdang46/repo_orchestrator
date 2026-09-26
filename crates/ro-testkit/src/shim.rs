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
//! So: a `gh` shell script on Unix, a **`gh.cmd` on Windows**. Windows
//! resolves `.cmd` through `PATHEXT` for `Command::new("gh")`, so the same
//! code that spawns `gh` on macOS spawns the shim on Windows with no
//! `#[cfg]` at the call site and no compile step.
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
    dir: TempDir,
    name: String,
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
            r#"printf '%s\n' "$*" >> "$RO_TESTKIT_LOG""#,
            r#"@echo off
echo %* >> "%RO_TESTKIT_LOG%"
exit /b 0"#,
            &log,
        );
        write_shim(&dir, name, &body);
        Self {
            dir,
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
        write_shim(&dir, name, &body);
        Self {
            dir,
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
            &format!(
                "@echo off\r\necho {}\r\nexit /b {code}",
                stderr.replace('\n', " ")
            ),
            &log,
        );
        write_shim(&dir, name, &body);
        Self {
            dir,
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
        write_shim(&dir, name, &body);
        Self {
            dir,
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
        write_shim(&dir, name, &body);
        Self {
            dir,
            name: name.to_string(),
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
            Ok(text) => text
                .lines()
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
    let log = log.display();
    format!("@echo off\r\nset \"RO_TESTKIT_LOG={log}\"\r\n{windows}\r\n")
}

/// Write the shim under both names.
///
/// On Unix only `gh` is needed. On Windows only `gh.cmd` is needed —
/// `Command::new("gh")` resolves it through `PATHEXT`. Writing both
/// everywhere would mean the Unix file is a stray non-executable, so the
/// platform decides.
fn write_shim(dir: &TempDir, name: &str, body: &str) {
    #[cfg(windows)]
    let file = dir.path().join(format!("{name}.cmd"));
    #[cfg(not(windows))]
    let file = dir.path().join(name);

    std::fs::write(&file, body).expect("the shim is writable");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&file)
            .expect("the shim exists")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&file, perms).expect("the mode is settable");
    }
    // On Windows no mode bit is needed: PATHEXT resolution does not check
    // one, and a `.cmd` is not a Unix executable in the first place.
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
    /// # Safety
    ///
    /// `body` must not mutate `PATH` or process-wide env vars.
    pub unsafe fn run<R>(self, body: impl FnOnce() -> R) -> R {
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
