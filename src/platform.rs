//! The site as its owner uses it: publishing, and the auth that gates it.
//!
//! `client_oauth` here is for MCP *clients* — it decides who may publish.
//! Visitor sign-in lives in `accounts`, and the two must never be conflated.

pub mod bearer;
pub mod client_oauth;
pub mod mcp;
pub mod upload;
