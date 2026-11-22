use phoebis_routing::{
    decide, Decided, RegistrySnapshot, RoutingState, SelectRequest, SelectResult, StateWrite,
};
use rand::Rng;

/// `decide` without its record, for a case that reads the result and the writes alone.
pub fn select(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    request: &SelectRequest,
    rng: &mut impl Rng,
) -> (SelectResult, Vec<StateWrite>) {
    let Decided { result, writes, .. } = decide(state, snapshot, request, rng);
    (result, writes)
}
