"""One-shot: replace both hand-rolled PATH swap/restore blocks with an RAII guard."""
import io
import re

PATH = "crates/ro-git/src/mutation.rs"

with io.open(PATH, encoding="utf-8") as fh:
    src = fh.read()

PATTERN = re.compile(
    r"[ ]*// SAFETY:[^\n]*\n"
    r"(?:[ ]*//[^\n]*\n)*"
    r"[ ]*let _guard = path_lock\(\)\.lock\(\)\.unwrap_or_else\(\|e\| e\.into_inner\(\)\);\n"
    r"[ ]*let previous = std::env::var\(\"PATH\"\)\.ok\(\);\n"
    r"[ ]*unsafe \{ std::env::set_var\(\"PATH\", format!\(\"\{\}:\{\}\", shim_dir\.display\(\), path\)\)\};\n"
    r"(?P<indent>[ ]*)let out = (?P<call>[^\n]*?);\n"
    r"[ ]*match previous \{\n"
    r"[ ]*Some\(p\) => unsafe \{ std::env::set_var\(\"PATH\", p\) \},\n"
    r"[ ]*None => unsafe \{ std::env::remove_var\(\"PATH\"\) \},\n"
    r"[ ]*\}\n"
)

REPLACEMENT = (
    "{indent}// PATH is process-global: the lock serialises the two tests that\n"
    "{indent}// touch it, and the guard restores on the way out of scope\n"
    "{indent}// *including* on a panic.\n"
    "{indent}let _serialised = path_lock().lock().unwrap_or_else(|e| e.into_inner());\n"
    "{indent}// SAFETY: held for the whole swap, and no other thread in this\n"
    "{indent}// binary reads the environment.\n"
    "{indent}let _restored = unsafe {{ PathSwap::install(&shim_dir) }};\n"
    "{indent}let out = {call};\n"
)

src, n = PATTERN.subn(lambda m: REPLACEMENT.format(indent=m.group("indent"), call=m.group("call")), src)
assert n == 2, "expected two sites, rewrote %d" % n

HELPER = '''    /// Restores `PATH` when dropped, including on a panic.
    ///
    /// Restoring by hand only runs on the success path. A panic between the
    /// swap and the restore leaves the shim directory first on `PATH`, and every
    /// test after it spawns the shim instead of git — a failure that looks like
    /// a dozen unrelated assertions rather than one leaked environment. Which is
    /// exactly what happened the first time this was written.
    struct PathSwap(Option<String>);

    impl PathSwap {
        /// # Safety
        ///
        /// `set_var` is unsafe because another thread may read the environment
        /// concurrently. Callers hold `path_lock` for the guard's whole life,
        /// and these tests are the only threads that touch `PATH`.
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

src = src.replace("    /// Serialises the tests that swap `PATH`.", HELPER, 1)
src = src.replace('        let path = std::env::var("PATH").unwrap_or_default();\n', "")

with io.open(PATH, "w", encoding="utf-8") as fh:
    fh.write(src)

print("rewrote %d sites and added the guard" % n)
