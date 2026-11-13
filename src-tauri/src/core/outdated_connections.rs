//! Closing the connections a new profile does not carry the way they are carried.

use crate::{config::profiles, core::handle, process::AsyncHandler};
use clash_verge_logging::{Type, logging};
use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::sync::{Mutex, oneshot::Receiver};

/// Bounds waiting for the saved selections of the new profile to be put in place.
const SELECTIONS_BUDGET: Duration = Duration::from_secs(45);

/// Counts the configs, and the changes to the config in use, sent to the core.
static PUBLISHED: AtomicU64 = AtomicU64::new(0);

/// Held while a config is counted before it is sent, and while a cleanup checks no config came
/// after its own and has the core judge: a config counted after that check reaches the core
/// only once the core has judged.
static PUBLISHING: Mutex<()> = Mutex::const_new(());

tokio::task_local! {
    /// The count of the last config sent to the core within `publishing`.
    static PUBLISHED_WITHIN: Cell<Option<u64>>;
}

/// Counts a config, or a change to the config in use, about to be sent to the core.
pub(crate) async fn before_publishing() {
    let _publishing = PUBLISHING.lock().await;
    let published = PUBLISHED.fetch_add(1, Ordering::AcqRel) + 1;
    let _ = PUBLISHED_WITHIN.try_with(|within| within.set(Some(published)));
}

/// Runs a config update and tells the count of the last config it sent to the core.
pub(crate) async fn publishing<T>(update: impl Future<Output = T>) -> (T, Option<u64>) {
    PUBLISHED_WITHIN
        .scope(Cell::new(None), async {
            let result = update.await;
            (result, PUBLISHED_WITHIN.with(Cell::get))
        })
        .await
}

/// Activates the saved selections of the profile just switched to and has the core close the
/// connections that profile does not carry the way they are carried, once the selections are
/// in place. `published` is the count of the config the switch sent to the core.
pub(crate) fn activate_and_close_after_switch(published: u64) {
    let (applied, generation) = profiles::activate_selected_nodes_after_switch();
    AsyncHandler::spawn(move || close_after_switch(applied, generation, published));
}

/// The core judges a connection by what the groups select where the rules lead somewhere else
/// than its own path, and the profile starts with other selections than the saved ones. Gives
/// up, keeping all connections, if the selections could not all be put in place in time, a
/// newer activation took over, or another config was sent to the core since the switch.
async fn close_after_switch(applied: Receiver<()>, generation: u64, published: u64) {
    let in_place = matches!(tokio::time::timeout(SELECTIONS_BUDGET, applied).await, Ok(Ok(())));
    let _publishing = PUBLISHING.lock().await;
    if !in_place || !profiles::is_activation_current(generation) || PUBLISHED.load(Ordering::Acquire) != published {
        logging!(
            info,
            Type::Core,
            "keeping the connections of the previous profile: the selections of the new one were not put in place, or another config came after it"
        );
        return;
    }
    match handle::Handle::mihomo().close_outdated_connections().await {
        Ok(Some(found)) => logging!(
            info,
            Type::Core,
            "closed {} connection(s) the new profile does not carry the same way, kept {}, could not judge {}",
            found.closed,
            found.kept,
            found.undetermined
        ),
        // a core that cannot tell closes them all
        Ok(None) => logging!(info, Type::Core, "the core closed all connections"),
        Err(err) => logging!(warn, Type::Core, "failed to close the outdated connections: {err}"),
    }
}
