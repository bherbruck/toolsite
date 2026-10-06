//! The site as its owner uses it: publishing, and the auth that gates it.
//!
//! `client_oauth` here is for MCP *clients* — it decides who may publish,
//! by letting an admin sign one in. Visitor sign-in lives in `accounts`, and
//! the two must never be conflated.

pub mod account;
pub mod admin;
pub mod bearer;
pub mod blob_upload;
pub mod client_oauth;
pub mod deploy;
pub mod export;
pub mod inline_upload;
pub mod github;
pub mod oauth_store;
pub mod preview;
pub mod projects;
pub mod screenshot;
pub mod knowledge;
pub mod manifest;
pub mod mcp_log;
pub mod mcp;
pub mod mcp_me;
pub mod scaffold;
pub mod schedule;
pub mod secrets;
pub mod trash;
pub mod upload;
