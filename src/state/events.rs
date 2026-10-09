//! What happens to an app that every runner must act on: it was hidden, or
//! removed. The store that records the change does not reach the sockets
//! and instances it affects; it says so here, and whoever holds them acts.
//!
//! Step 1 has one runner, so the sink is in-process (`runtime::events`) and
//! does what `write_meta` used to do itself. Step 2 swaps it for the bus,
//! and every runner closes its own share.

/// One app's change, as the sink hears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppChange<'a> {
    /// The app, never a page inside it.
    pub app: &'a str,
    /// The app, or a page in it, is hidden now.
    pub hidden: bool,
    /// The app was taken down for good.
    pub removed: bool,
}

/// Where app changes go. A hidden or removed app keeps no live connection
/// open and no resident instance: retraction takes effect on the sockets
/// now, not at their next check.
pub trait AppEvents: Send + Sync {
    fn app_changed(&self, change: AppChange<'_>);
}
