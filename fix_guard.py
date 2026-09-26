"""One-shot: restore PATH on panic, with an RAII guard.

Swapping PATH back by hand only runs on the success path. A panic between the
swap and the restore leaves the shim directory first on PATH, and every
subsequent test in the binary spawns the shim instead of git — which is what
just happened.

This is the same shape as the RepoLock `Drop` fix: cleanup that must happen
belongs in `Drop`, because `Drop` is the only thing that also runs on a panic.
"""
import io
import re

PATH = "crates/ro-git/src/mutation.rs"

HELPER = '''    /// Restores `PATH` when dropped, including on a panic.
    ///
    /// Restoring by hand only runs on the success path. A panic between the
    /// swap and the restore leaves the shim directory first on `PATH`, and every
    /// test after it spawns the shim instead of git — a failure that looks like
    /// a dozen unrelated assertions rather than one leaked environment.
    struct PathSwap(Option<String>);

    impl PathSwap {
        /// # Safety
        ///
        /// `set_var` is unsafe because another thread may read the environment
        /// concurrently. Callers hold `path_lock` for the guard's whole life,
        /// and this crate's tests are the only threads that touch `PATH`.
        unsafe fn install(prefix: &Path) -> Self {
            let previous = std::env::var("PATH").ok();
            let current = previous.clone().unwrap_or_default();
            unsafe { std::env::set_var("PATH", format!("{}:{current}", prefix.display())) };
            Self(previous)
        }
    }

    impl Drop for PathSwap {
        fn drop(&mut self) {
            match self.0.take() {
                // SAFETY: as in `install`, under the same lock.
                Some(p) => unsafe { std::env::set_var("PATH", p) },
                None => unsafe { std::env::remove_var("PATH") },
            }
        }
    }

    /// Serialises the tests that swap `PATH`.'''

with io.open(PATH, encoding="utf-8") as fh:
    src = fh.read()

# Replace the hand-rolled set/restore pairs with the guard.
pattern = re.compile(
    r"( *// SAFETY: [^\n]*\n)?"
    r" *let _guard = path_lock\(\)\.lock\(\)\.unwrap_or_else\(\|e\| e\.into_inner\(\)\);\n"
    r" *let previous = std::env::var\(\"PATH\"\)\.ok\(\);\n"
    r" *unsafe \{ std::env::set_var\(\"PATH\", format!\(\"\{\}:\{\}\", shim_dir\.display\(\), path\)\)\};\n"
    r" *let out = (?P<call>push_with_credential\([^;]*?\));\n"
    r" *match previous \{\n"
    r" *Some\(p\) => unsafe \{ std::env::set_var\(\"PATH\", p\) \},\n"
    r" *None => unsafe \{ std::env::remove_var\(\"PATH\"\) \},\n"
    r" *\}\n",
    re.DOTALL,
)


def replace(match):
    return (
        "            // PATH is process-global: the lock serialises the two tests\n"
        "            // that touch it, and the guard restores on the way out of scope\n"
        "            // *including* on a panic.\n"
        "            let _serialised = path_lock().lock().unwrap_or_else(|e| e.into_inner());\n"
        "            // SAFETY: held across the whole swap, and no other thread in\n"
        "            // this binary reads the environment.\n"
        "            let _restored = unsafe { PathSwap::install(&shim_dir) };\n"
        "            let out = %s;\n" % match.group("call")
    )


src, n = pattern.subn(replace, src)
assert n == 2, "expected two swap sites, rewrote %d" % n

src = src.replace("    /// Serialises the tests that swap `PATH`.", HELPER, 1)

# `path` is now unused; the guard reads PATH itself.
src = src.replace('        let path = std::env::var("PATH").unwrap_or_default();\n', "")

with io.open(PATH, "w", encoding="utf-8") as fh:
    fh.write(src)

print("rewrote %d PATH swap sites with an RAII guard" % n)
