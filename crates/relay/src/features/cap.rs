//! The bound on the domain checks in flight, which the feature listener forwards without a key. A check takes a place
//! of its source first and then a place of the total, so a check that waits behind its own source holds back no other
//! source's checks. Both semaphores hand out their places in the order the calls asked, and a call dropped while it
//! waits leaves its queue.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The domain checks in flight, bounded in total and per source address.
pub(crate) struct DomainCheckCap {
    per_source: usize,
    sources: Mutex<HashMap<IpAddr, Source>>,
    total: Arc<Semaphore>,
}

/// One source address's places, with the checks that wait for one or hold one. The entry leaves the map once that
/// count is zero and not with its last place, since a waiter granted a place it has not taken yet would otherwise see
/// a later arrival make a fresh entry, and the source would hold one place past its share.
struct Source {
    places: Arc<Semaphore>,
    users: usize,
}

/// A check's count in its source's entry, from its arrival to its end.
struct Claim {
    cap: Arc<DomainCheckCap>,
    source: IpAddr,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut sources = self
            .cap
            .sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = sources.get_mut(&self.source) {
            entry.users -= 1;
            if entry.users == 0 {
                sources.remove(&self.source);
            }
        }
    }
}

/// One check's place. Dropping it frees the place, whether the check completed or not.
pub(crate) struct Place {
    // The fields drop in their order, so both places are free before the claim can take the source's entry out.
    _total: OwnedSemaphorePermit,
    _source: OwnedSemaphorePermit,
    _claim: Claim,
}

impl DomainCheckCap {
    pub(crate) fn new(total: usize, per_source: usize) -> Arc<DomainCheckCap> {
        Arc::new(DomainCheckCap {
            per_source,
            sources: Mutex::new(HashMap::new()),
            total: Arc::new(Semaphore::new(total)),
        })
    }

    /// Waits for a place for `source`, in the order the calls came.
    pub(crate) async fn place(self: &Arc<Self>, source: IpAddr) -> Place {
        let places = {
            let mut sources = self.sources.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = sources.entry(source).or_insert_with(|| Source {
                places: Arc::new(Semaphore::new(self.per_source)),
                users: 0,
            });
            entry.users += 1;
            Arc::clone(&entry.places)
        };
        let claim = Claim {
            cap: Arc::clone(self),
            source,
        };
        let source_place = places
            .acquire_owned()
            .await
            .expect("a source's places are never closed");
        let total_place = Arc::clone(&self.total)
            .acquire_owned()
            .await
            .expect("the total's places are never closed");
        Place {
            _total: total_place,
            _source: source_place,
            _claim: claim,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::net::{IpAddr, Ipv4Addr};
    use std::pin::{pin, Pin};
    use std::task::{Context, Poll, Waker};

    use super::*;

    fn address(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, last))
    }

    /// One poll of a waiting call, under a waker that wakes nothing, since the test polls again itself.
    fn poll_once(call: Pin<&mut impl Future<Output = Place>>) -> Option<Place> {
        match call.poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(place) => Some(place),
            Poll::Pending => None,
        }
    }

    #[test]
    fn a_second_source_passes_a_full_share() {
        let cap = DomainCheckCap::new(4, 2);
        let _held = [
            poll_once(pin!(cap.place(address(1)))).expect("the first place is free"),
            poll_once(pin!(cap.place(address(1)))).expect("the second place is free"),
        ];
        assert!(
            poll_once(pin!(cap.place(address(2)))).is_some(),
            "another source waited behind the first one's share"
        );
    }

    #[test]
    fn a_check_past_the_total_waits_whatever_its_source() {
        let cap = DomainCheckCap::new(4, 2);
        let mut held = vec![
            poll_once(pin!(cap.place(address(1)))).expect("a place is free"),
            poll_once(pin!(cap.place(address(1)))).expect("a place is free"),
            poll_once(pin!(cap.place(address(2)))).expect("a place is free"),
            poll_once(pin!(cap.place(address(2)))).expect("a place is free"),
        ];
        let mut third = pin!(cap.place(address(3)));
        assert!(
            poll_once(third.as_mut()).is_none(),
            "a check past the total got a place"
        );
        held.pop();
        assert!(
            poll_once(third.as_mut()).is_some(),
            "a freed place of the total served no waiter"
        );
    }

    #[test]
    fn a_check_whose_caller_left_frees_its_place() {
        let cap = DomainCheckCap::new(4, 2);
        let mut held = vec![
            poll_once(pin!(cap.place(address(1)))).expect("the first place is free"),
            poll_once(pin!(cap.place(address(1)))).expect("the second place is free"),
        ];
        {
            let mut left = pin!(cap.place(address(1)));
            assert!(
                poll_once(left.as_mut()).is_none(),
                "a check past its source's share got a place"
            );
        }
        held.pop();
        assert!(
            poll_once(pin!(cap.place(address(1)))).is_some(),
            "a check whose caller left kept its place or its turn"
        );
    }
}
