//! Configuration loading and validation for ro.
//!
//! Reads `$XDG_CONFIG_HOME/ro/config.toml`, applies defaults, validates.

pub mod loader;
pub mod paths;
pub mod policy;
pub mod schema;
pub mod validate;

pub use loader::{load_config, load_default, write_default};
pub use paths::{ConfigPaths, default_config_toml, expand_tilde};
pub use policy::{FileViolation, Policy, PolicyReport, check_policy};
pub use schema::AppConfig;
pub use validate::validate;
