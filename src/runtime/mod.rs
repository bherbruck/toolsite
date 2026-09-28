//! Executing an app's own code and data: the wasm sandbox, the per-app
//! SQLite database its handler is allowed to reach, and the files it keeps.

pub mod blobs;
pub mod db;
pub mod migrate;
pub mod outbound;
pub mod wasm;
