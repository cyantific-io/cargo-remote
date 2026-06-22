//! Library facet of the agent, exposing the wire [`proto`]col for integration tests.
//!
//! The *deployable* artifact is the binary (`main.rs`), which embeds the same `proto` module; it
//! is compiled by rustle-core's `build.rs`, bundled into the cli/mcp, and deployed as a
//! prebuilt binary — it does not depend on this lib.

pub mod proto;
