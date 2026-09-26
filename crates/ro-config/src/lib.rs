//! Configuration loading and validation for ro.
//!
//! Reads `$XDG_CONFIG_HOME/ro/config.toml`, applies defaults, validates.

pub mod loader;
pub mod paths;
pub mod schema;
pub mod validate;

pub use loader::{load_config, load_default, set_key_in_file, write_default};
pub use paths::{ConfigPaths, default_config_toml, expand_tilde};
pub use schema::AppConfig;
pub use validate::validate;
