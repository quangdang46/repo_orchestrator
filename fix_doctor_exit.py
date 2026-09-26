"""One-shot: `ro doctor` keeps its own exit code, named rather than inlined.

A failed environment check is not a partial fleet run, and reading it as
one is how a green run gets retried for the wrong reason. The doctor's
`Severity::Optional` probes can never move it either.
"""
import io

P = "crates/ro/src/main.rs"
s = io.open(P, encoding="utf-8").read()
s = s.replace(
    "            std::process::exit(report.exit_code());",
    "            // The doctor is not a fleet command: its own code, and its\n"
    "            // `Severity::Optional` probes can never move it.\n"
    "            std::process::exit(report.exit_code() as i32);",
)
io.open(P, "w", encoding="utf-8").write(s)
print("main ok")

P = "crates/ro/src/doctor.rs"
s = io.open(P, encoding="utf-8").read()
OLD = """    pub fn exit_code(&self) -> i32 {
        if self.failures() > 0 { 1 } else { 0 }
    }"""
NEW = """    /// The doctor's own 0/1, deliberately not the fleet table.
    ///
    /// A failed environment check is not a partial run, and reading it as
    /// one is how a green run gets retried for the wrong reason. Its
    /// `Severity::Optional` probes can never move it.
    pub fn exit_code(&self) -> u8 {
        if self.failures() > 0 {
            crate::exit::EX_DOCTOR_FAILED
        } else {
            crate::exit::EX_OK
        }
    }"""
assert OLD in s, "the doctor exit_code was not found verbatim"
s = s.replace(OLD, NEW, 1)
io.open(P, "w", encoding="utf-8").write(s)
print("doctor ok")
