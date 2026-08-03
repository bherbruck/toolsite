//! Executing an app's own code and data: the wasm sandbox, and the per-app
//! SQLite database its handler is allowed to reach.

pub mod db;
pub mod migrate;
pub mod wasm;
