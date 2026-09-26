"""One-shot: the spawned-engine test could pass vacuously.

`with_stdout` prints a fixed banner, so an engine that never ran and an
engine that ran and said nothing look the same to the assertion above it.
"""
import io

PATH = "crates/ro-engine/tests/credential_never_reaches_engine.rs"

OLD = '''    assert!(
        !captured.contains(token),
        "the token reached the spawned engine:\\n{captured}"
    );
}'''

NEW = '''    assert!(
        !captured.contains(token),
        "the token reached the spawned engine:\\n{captured}"
    );
    // The vacuity guard. Without it this test passes on a harness where
    // the shim never ran: `captured` would be empty, and an empty string
    // contains no token.
    assert!(
        !captured.trim().is_empty(),
        "the engine shim produced no output, so nothing was actually \\
         observed and the check above passes for the wrong reason"
    );
}'''

with io.open(PATH, encoding="utf-8") as fh:
    src = fh.read()

assert OLD in src, "anchor not found"
with io.open(PATH, "w", encoding="utf-8") as fh:
    fh.write(src.replace(OLD, NEW, 1))
print("ok")
