//! Readiness: whether the three components a request depends on past the listener are alive.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use phoebis_bus::Bus;
use phoebis_ledger::Ledger;
pub use phoebis_relay::ReadinessSource;
use phoebis_routing::RouterHandle;
use phoebis_store::Store;

/// The three liveness reads behind `/api/ready` and `/admin/system`'s `ready` and `dead`.
pub struct Readiness {
    pub bus: Bus,
    pub ledger: Ledger,
    pub router: RouterHandle,
    pub store: Arc<Store>,
}

impl Readiness {
    /// The dead components, `ledger`, `router`, `store` in that order; empty when the router is ready.
    pub fn dead(&self) -> Vec<&'static str> {
        // The writer's channel stays open while the assembler is gone, so the subscriber's panic counter is
        // the only sign the bus's half of the consumer died. A subscriber that never attached is dead too.
        let consumer = self
            .bus
            .stats()
            .iter()
            .find(|(name, _)| *name == "ledger")
            .is_some_and(|(_, stats)| stats.panics.load(Ordering::Relaxed) == 0);
        dead_of(
            consumer && self.ledger.is_alive(),
            self.router.is_alive(),
            self.store.accounts().is_ok(),
        )
    }
}

/// The same rule over three booleans, each `true` while that component is alive, so the composition is tested
/// without a dead task.
pub fn dead_of(ledger: bool, router: bool, store: bool) -> Vec<&'static str> {
    [("ledger", ledger), ("router", router), ("store", store)]
        .into_iter()
        .filter_map(|(name, alive)| (!alive).then_some(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::dead_of;

    #[test]
    fn dead_of_names_each_dead_component_in_order() {
        for (ledger, router, store, dead) in [
            (true, true, true, vec![]),
            (false, true, true, vec!["ledger"]),
            (true, false, true, vec!["router"]),
            (true, true, false, vec!["store"]),
            (false, false, true, vec!["ledger", "router"]),
            (false, true, false, vec!["ledger", "store"]),
            (true, false, false, vec!["router", "store"]),
            (false, false, false, vec!["ledger", "router", "store"]),
        ] {
            assert_eq!(
                dead_of(ledger, router, store),
                dead,
                "ledger alive {ledger}, router alive {router}, store alive {store}"
            );
        }
    }
}
