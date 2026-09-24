//! MCP server integration for ro.
//!
//! Provides MCP tools, resources, and server entry point.
//! Full stdio server implementation is rfo-34.

pub mod resources;
pub mod server;
pub mod tools;

pub use resources::{ResourceDef, list_resources};
pub use server::{ServerCapabilities, default_capabilities, version};
pub use tools::{ToolDef, list_tools};
