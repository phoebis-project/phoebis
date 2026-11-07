//! Readiness: whether the three components a request depends on past the listener are alive.

use std::sync::Arc;

use phoebis_ledger::Ledger;
use phoebis_routing::RouterHandle;
use phoebis_store::Store;

/// The three liveness reads behind `/api/ready` and `/admin/system`'s `ready` and `dead`.
pub struct Readiness {
    pub ledger: Ledger,
    pub router: RouterHandle,
    pub store: Arc<Store>,
}

impl Readiness {
    /// The dead components, `ledger`, `router`, `store` in that order, and none when the router is ready. The ledger
    /// reports its own bus subscriber beside its writer.
    pub fn dead(&self) -> Vec<&'static str> {
        dead_of(
            self.ledger.is_alive(),
            self.router.is_alive(),
            // Two point reads under the store's mutex: the route answers anyone who reaches the listener, so a
            // probe must not cost a scan of every account row.
            self.store.schema_version().is_ok(),
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
            (false, true, true, vec!["ledger"]),
            (true, false, true, vec!["router"]),
            (true, true, false, vec!["store"]),
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
