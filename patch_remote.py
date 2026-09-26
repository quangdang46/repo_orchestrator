"""One-shot: simplify BareRemote construction.

`new` and `ephemeral` each ran `git init` over a path the other had already
prepared. One clear path instead.
"""
import io

PATH = "crates/ro-testkit/src/remote.rs"

OLD = '''impl BareRemote {
    /// Create a bare repo at `dir` (the directory is created for you).
    pub fn new(dir: PathBuf) -> Self {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "remote.git".to_string());
        let parent = dir.parent().unwrap_or(Path::new(".")).to_path_buf();
        std::fs::create_dir_all(&parent).expect("the parent dir is creatable");
        let out = git()
            .args(["init", "--bare", "-q", &name])
            .current_dir(&parent)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git init --bare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self {
            dir: TempDir::new_from(parent.join(&name))
                .unwrap_or_else(|_| panic!("{dir:?} should be a directory")),
        }
    }

    /// A bare repo in a fresh temp dir.
    pub fn ephemeral() -> Self {
        let tmp = TempDir::new().expect("the temp dir is creatable");
        let path = tmp.path().join("remote.git");
        std::fs::create_dir_all(&path).expect("the remote dir is creatable");
        let out = git()
            .args(["init", "--bare", "-q", "."])
            .current_dir(&path)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git init --bare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The remote owns its own directory, so the outer temp dir is kept
        // alive by leaking it into the returned path rather than dropped.
        // `TempDir::into_path` would delete on drop, which is what we want
        // for the *outer* dir but not the inner one.
        std::mem::forget(tmp);
        Self {
            dir: TempDir::new_from(path).expect("the remote is a directory"),
        }
    }
'''

NEW = '''impl BareRemote {
    /// Create a bare repo at `path`, creating the directory if needed.
    pub fn new(path: PathBuf) -> Self {
        std::fs::create_dir_all(&path).expect("the remote dir is creatable");
        let out = git()
            .args(["init", "--bare", "-q", "."])
            .current_dir(&path)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git init --bare at {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        // `TempDir::new_from` takes ownership of the path and removes it on
        // drop, which is the lifetime a test fixture wants.
        Self {
            dir: TempDir::new_from(path).expect("the remote is a directory"),
        }
    }

    /// A bare repo in a fresh temp dir.
    pub fn ephemeral() -> Self {
        let tmp = TempDir::new().expect("the temp dir is creatable");
        let path = tmp.path().join("remote.git");
        // The outer temp dir would be removed before the remote's own
        // TempDir, so it is leaked deliberately; the inner one owns the
        // real cleanup.
        std::mem::forget(tmp);
        Self::new(path)
    }
'''

with io.open(PATH, encoding="utf-8") as fh:
    src = fh.read()

assert OLD in src, "the block to replace was not found verbatim"
with io.open(PATH, "w", encoding="utf-8") as fh:
    fh.write(src.replace(OLD, NEW))
print("ok")
