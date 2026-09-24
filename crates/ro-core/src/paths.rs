//! XDG-aware path helpers for ro.

use std::path::PathBuf;

/// Return the config directory: `$XDG_CONFIG_HOME/ro` or `~/.config/ro`.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("ro"))
}

/// Return the state directory: `$XDG_STATE_HOME/ro` or `~/.local/state/ro`.
/// On Windows, falls back to `%LOCALAPPDATA%\ro` since `dirs::state_dir()`
/// returns `None`.
pub fn state_dir() -> Option<PathBuf> {
    let base = dirs::state_dir();
    #[cfg(windows)]
    let base = base.or_else(dirs::data_local_dir);
    base.map(|p| p.join("ro"))
}

/// Return the cache directory: `$XDG_CACHE_HOME/ro` or `~/.cache/ro`.
pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|p| p.join("ro"))
}
