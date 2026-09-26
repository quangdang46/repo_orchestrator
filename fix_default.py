"""One-shot: `EngineSlot::default` must be empty, not "git".

A slot cannot know its own field name, so a `Default` that guessed gave
`engine_claude` a `bin` of `git` — and a user who wrote

    [engine_claude]
    default_args = ["-p"]

would silently get a *git* engine with an agent's arguments. Empty means
"take the shipped value for this slot", resolved by name where the name
is known.
"""
import io

P = "crates/ro-config/src/schema.rs"
s = io.open(P, encoding="utf-8").read()

OLD = '''impl Default for EngineSlot {
    fn default() -> Self {
        Self {
            bin: default_bin(),
            default_args: Vec::new(),
        }
    }
}'''

NEW = '''impl Default for EngineSlot {
    /// Empty, not a guessed binary.
    ///
    /// A slot cannot know its own field name, so a `Default` that guessed
    /// gave `engine_claude` a `bin` of `git` — and a user who wrote only
    /// `default_args` for that slot would silently get a *git* engine with
    /// an agent's arguments. Empty means "take the shipped value for this
    /// slot", resolved by name where the name is actually known.
    fn default() -> Self {
        Self {
            bin: String::new(),
            default_args: Vec::new(),
        }
    }
}'''

assert OLD in s, "the Default impl was not found verbatim"
s = s.replace(OLD, NEW, 1)

# The serde default attribute must agree, and `default_bin` is now gone.
s = s.replace('    #[serde(default = "default_bin")]\n    pub bin: String,',
              '    #[serde(default)]\n    pub bin: String,')
s = s.replace('''fn default_bin() -> String {
    "git".to_string()
}
''', '')

io.open(P, "w", encoding="utf-8").write(s)
print("ok")
