//! Claim-once tracking of strategy-published OANDA transaction ids.
//!
//! A fill (or synchronous cancel) reaches the gateway twice: once in the REST
//! order response and once on the transaction stream. Whichever path claims the
//! transaction id first publishes the event to strategy clients; the other
//! suppresses its copy. Claims also persist across stream reconnects, so a
//! re-delivered transaction is not published twice.
//!
//! The set is process-wide: each gateway instance serves exactly one provider
//! (and so one OANDA account), making the transaction id a process-unique key.
//! This is what lets the REST path (on the adapter) and the stream tasks (on
//! the provider) share claims without core-trait plumbing between them.
//!
//! A claim is taken just before sending, after checking the channel is
//! connected. If the in-process receiver drops between the two (gateway
//! shutdown), the event is lost while its id stays claimed -- accepted, since
//! the receiver otherwise lives for the lifetime of the process.

use std::collections::{HashSet, VecDeque};
use std::sync::{LazyLock, Mutex, PoisonError};

/// Far above any realistic burst of in-flight transactions; bounds memory if
/// the gateway runs for months.
const CAPACITY: usize = 4096;

pub(crate) static PUBLISHED_TRANSACTIONS: LazyLock<PublishedTransactions> =
    LazyLock::new(|| PublishedTransactions::with_capacity(CAPACITY));

/// Bounded claim-once set of transaction ids, FIFO-evicted at capacity.
pub(crate) struct PublishedTransactions {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl PublishedTransactions {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Claim a transaction id for publication. Returns `true` exactly once per
    /// id (within capacity): the caller that gets `true` publishes, all others
    /// suppress their copy.
    pub(crate) fn try_claim(&self, id: &str) -> bool {
        self.try_claim_all(&[id])
    }

    /// Claim several transaction ids atomically: all of them, or none. Used
    /// for a fill and its accompanying cancel, which must be published by the
    /// same path — independent claims could split between the REST and stream
    /// paths and deliver a contradictory pair. A duplicate id within `ids` is
    /// claimed once.
    pub(crate) fn try_claim_all(&self, ids: &[&str]) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);

        if ids.iter().any(|id| inner.seen.contains(*id)) {
            return false;
        }
        for id in ids {
            if inner.seen.insert((*id).to_string()) {
                inner.order.push_back((*id).to_string());
                if inner.order.len() > self.capacity
                    && let Some(evicted) = inner.order.pop_front()
                {
                    inner.seen.remove(&evicted);
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_each_id_exactly_once() {
        let set = PublishedTransactions::with_capacity(8);
        assert!(set.try_claim("100"));
        assert!(!set.try_claim("100"));
        assert!(set.try_claim("101"));
        assert!(!set.try_claim("100"));
    }

    #[test]
    fn claims_ids_atomically_all_or_nothing() {
        let set = PublishedTransactions::with_capacity(8);
        assert!(set.try_claim("2"));
        assert!(
            !set.try_claim_all(&["1", "2"]),
            "pair claim must fail when any id is already claimed"
        );
        assert!(
            set.try_claim("1"),
            "a failed pair claim must not claim the other id"
        );
        assert!(set.try_claim_all(&["3", "4"]));
        assert!(!set.try_claim("3"));
        assert!(!set.try_claim("4"));
    }

    #[test]
    fn duplicate_ids_in_one_claim_are_counted_once() {
        let set = PublishedTransactions::with_capacity(2);
        assert!(set.try_claim_all(&["7", "7"]));
        assert!(!set.try_claim("7"));
        assert!(set.try_claim("8"), "capacity must not be double-consumed");
        assert!(
            !set.try_claim("7"),
            "the duplicate-claimed id stays claimed"
        );
    }

    #[test]
    fn evicts_oldest_id_at_capacity() {
        let set = PublishedTransactions::with_capacity(2);
        assert!(set.try_claim("1"));
        assert!(set.try_claim("2"));
        assert!(set.try_claim("3"));
        assert!(set.try_claim("1"), "evicted id is claimable again");
        assert!(!set.try_claim("3"), "recent ids stay claimed");
    }
}
