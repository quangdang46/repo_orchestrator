//! MCP server entry point.
//!
//! Placeholder for stdio-based MCP server implementation.
//! The full server will be implemented in rfo-34.

use serde::{Deserialize, Serialize};

/// Server capabilities exposed to MCP clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub tools: bool,
    pub resources: bool,
}

/// Default server capabilities.
pub fn default_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        tools: true,
        resources: true,
    }
}

/// Server version info.
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capabilities_has_tools_and_resources() {
        let caps = default_capabilities();
        assert!(caps.tools);
        assert!(caps.resources);
    }

    #[test]
    fn version_returns_semver() {
        let v = version();
        assert!(!v.is_empty());
        assert!(v.contains('.'));
    }
}
