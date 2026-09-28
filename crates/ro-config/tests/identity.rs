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
        id.resolve("personal").is_err(),
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
    let p = id
        .fallback()
        .expect("default is defined")
        .expect("and it resolves");
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
    let p = id
        .fallback()
        .expect("one profile is unambiguous")
        .expect("and it resolves");
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
        id.fallback().expect("ambiguous, not broken").is_none(),
        "picking one of two profiles by map order would make the commit author \
         depend on TOML key order — the same address on one machine and a \
         different one after an edit"
    );
}

#[test]
fn no_profiles_at_all_yields_nothing_rather_than_a_placeholder() {
    let id: IdentityConfig = toml::from_str("").expect("empty config parses");
    assert!(id.fallback().expect("no profiles is not an error").is_none());
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
    assert!(
        id.resolve("default").is_err(),
        "default is a key of [identity], not a profile name"
    );
    assert_eq!(id.resolve("work").expect("work is a profile").email, "a@corp");
}

/// A profile must be buildable **one key at a time**.
///
/// This is not a hypothetical. With `name` and `email` required, setting
/// `identity.work.name` produced a `[identity.work]` table holding one key,
/// the whole file then failed to load with `missing field email`, and
/// `ro config set` reported a TOML parse error with a line number. The
/// tool that exists so a config can be written without an editor could not
/// write one: the only way to create a profile was to open the file.
///
/// Both fields are `Option` now, and an incomplete profile is a well-formed
/// config that names what it is missing.
#[test]
fn a_profile_can_be_written_one_key_at_a_time() {
    let half = cfg(
        r#"
[identity.work]
name = "A"
"#,
    );
    assert!(
        half.names().contains(&"work"),
        "a half-written profile is still a profile"
    );
    let err = half.resolve("work").expect_err("it is not usable yet");
    assert!(
        err.contains("email") && !err.contains("name"),
        "the error names what is missing and only that, got: {err}"
    );

    // The other order fails the same way, symmetrically.
    let other = cfg(
        r#"
[identity.work]
email = "a@corp"
"#,
    );
    let err = other.resolve("work").unwrap_err();
    assert!(err.contains("name") && !err.contains("email"), "got: {err}");

    // Complete it, and it resolves.
    let whole = cfg(
        r#"
[identity.work]
name  = "A"
email = "a@corp"
"#,
    );
    assert_eq!(whole.resolve("work").unwrap().email, "a@corp");
}

/// A `default` pointing at a profile that is missing — or half-written — is
/// a configuration mistake, and the alternative is committing every repo in
/// the fleet under some other identity without saying so.
#[test]
fn a_default_naming_an_unusable_profile_is_an_error_not_a_silent_fallback() {
    let missing = cfg(
        r#"
[identity]
default = "nonexistent"
"#,
    );
    let err = missing.fallback().expect_err("a typo must not be papered over");
    assert!(err.contains("nonexistent"), "got: {err}");
    assert!(
        err.contains("default"),
        "the message must blame the `default` key the user wrote, not the          profile name they did not: got: {err}"
    );

    let half = cfg(
        r#"
[identity]
default = "work"

[identity.work]
name = "A"
"#,
    );
    let err = half.fallback().expect_err("half-written is not usable");
    assert!(err.contains("email"), "got: {err}");
}

/// The error for an unknown profile has to say what *is* available, or the
/// user is left guessing at a name the tool could have printed.
#[test]
fn an_unknown_profile_error_lists_the_ones_that_exist() {
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
    let err = id.resolve("oss").unwrap_err();
    assert!(err.contains("work") && err.contains("personal"), "got: {err}");
    assert!(err.contains("oss"), "and names what was asked for, got: {err}");

    // And with none defined it says so, rather than printing an empty list.
    let empty: IdentityConfig = toml::from_str("").expect("empty config parses");
    let err = empty.resolve("work").unwrap_err();
    assert!(err.contains("no [identity.*] profile named"), "got: {err}");
}
