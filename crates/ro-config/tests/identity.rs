//! The `[identity.*]` profile resolver.
//!
//! This is the layer a row's `author_ref` points at, and the part that was
//! advertised and inert: `ro add --author` documented "which `[identity.*]`
//! profile commits this repo" while `AppConfig` had **no** `identity` field
//! at all, and the resolver synthesised `email = "{name}@localhost"` from the
//! profile name. So `ro add --author work` produced a real commit authored
//! by `work@localhost` — reported as success, with nothing anywhere saying
//! the address had been invented.
//!
//! The contract these tests pin:
//!
//!   1. a defined profile resolves to the address written in the config
//!   2. an undefined profile is an **error**, not a fallback and not a
//!      synthesised address
//!   3. a repo with no `author_ref` gets `default`
//!   4. one profile needs no `default` key; two profiles without one are
//!      ambiguous rather than arbitrary

use ro_config::schema::IdentityConfig;

fn cfg(toml: &str) -> IdentityConfig {
    toml::from_str::<ro_config::schema::AppConfig>(toml)
        .expect("config should parse")
        .identity
}

#[test]
fn a_defined_profile_resolves_to_the_written_address() {
    let id = cfg(
        r#"
[identity.work]
name  = "Dang Tran Quang"
email = "quang@company.com"
"#,
    );
    let p = id.resolve("work").expect("work is defined");
    assert_eq!(p.name, "Dang Tran Quang");
    assert_eq!(p.email, "quang@company.com");
}

#[test]
fn an_undefined_profile_resolves_to_nothing() {
    let id = cfg(
        r#"
[identity.work]
name  = "A"
email = "a@corp"
"#,
    );
    assert!(
        id.resolve("personal").is_none(),
        "an undefined profile must not resolve — the caller turns this into an \
         error, and inventing `personal@localhost` here is the bug this pins"
    );
}

#[test]
fn default_is_used_when_a_row_has_no_author_ref() {
    let id = cfg(
        r#"
[identity]
default = "work"

[identity.work]
name  = "A"
email = "a@corp"

[identity.personal]
name  = "A"
email = "a@home"
"#,
    );
    let p = id.fallback().expect("default is defined");
    assert_eq!(p.email, "a@corp", "default should win");
}

#[test]
fn a_single_profile_needs_no_default_key() {
    let id = cfg(
        r#"
[identity.work]
name  = "A"
email = "a@corp"
"#,
    );
    let p = id.fallback().expect("one profile is unambiguous");
    assert_eq!(p.email, "a@corp");
}

#[test]
fn two_profiles_and_no_default_is_ambiguous_not_arbitrary() {
    let id = cfg(
        r#"
[identity.work]
name  = "A"
email = "a@corp"

[identity.personal]
name  = "A"
email = "a@home"
"#,
    );
    assert!(
        id.fallback().is_none(),
        "picking one of two profiles by map order would make the commit author \
         depend on TOML key order — the same address on one machine and a \
         different one after an edit"
    );
}

#[test]
fn no_profiles_at_all_yields_nothing_rather_than_a_placeholder() {
    let id: IdentityConfig = toml::from_str("").expect("empty config parses");
    assert!(id.fallback().is_none());
    assert!(id.names().is_empty());
}

#[test]
fn names_lists_every_profile_for_an_error_message() {
    let id = cfg(
        r#"
[identity.work]
name  = "A"
email = "a@corp"

[identity.personal]
name  = "A"
email = "a@home"
"#,
    );
    let mut n = id.names();
    n.sort();
    assert_eq!(n, vec!["personal", "work"], "the error has to say what IS valid");
}

#[test]
fn the_default_key_is_not_mistaken_for_a_profile() {
    // `default` is a key of `[identity]`, not a profile name. With a
    // flatten-based map the two must not collide.
    let id = cfg(
        r#"
[identity]
default = "work"

[identity.work]
name  = "A"
email = "a@corp"
"#,
    );
    assert!(id.resolve("default").is_none(), "default is not a profile");
    assert_eq!(id.resolve("work").expect("work is a profile").email, "a@corp");
}
