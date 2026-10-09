//! The in-process `AppEvents` sink: this process's sockets and resident
//! instances, closed when their app is hidden or removed.

use crate::{
    runtime::{connections::Hub, resident::Residents},
    state::events::{AppChange, AppEvents},
};
use std::sync::Arc;

pub struct Local {
    pub connections: Arc<Hub>,
    pub residents: Arc<Residents>,
}

impl AppEvents for Local {
    fn app_changed(&self, change: AppChange<'_>) {
        if change.hidden || change.removed {
            self.connections.close_app(change.app);
            self.residents.stop(change.app);
        }
    }
}
