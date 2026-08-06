//! Per-user admission gate layered on the endpoint pool (#632, ADR-0175).
//!
//! ADR-0050's pool keys an [`EndpointState`] by `(base_url, sha256(api_key))`
//! — two users configured with *distinct* keys already land in separate
//! states and so get independent budgets for free. Two users sharing one
//! *literal* key land in the **same** `EndpointState`, and only the first
//! caller to size it (`HttpClient::endpoint`'s "first caller wins") set its
//! aggregate rpm/concurrency — every other user sharing that key was
//! silently bound by whichever user's session happened to resolve first
//! (ADR-0147 "Consequences", ledger row 12).
//!
//! This mirrors [`super::ModelSlot`]/`EndpointState::model_slot` (ADR-0140)
//! exactly, just keyed by [`UserId`] instead of model id: a second, narrower
//! gate — resolved from *that user's own* catalog `rpm`/`concurrency` rather
//! than whichever user sized the endpoint first — sitting alongside the
//! per-model gate, under the endpoint-wide cap. A user with no configured
//! rpm/concurrency (`None`, single-user mode's default) admits solely
//! through the model/endpoint gates, byte-identical to before this existed.
//! Unlike the endpoint's own [`super::RateLimiter`], a user's pacing gate is
//! **not** AIMD-adaptive — a 429 is a property of the whole endpoint, not of
//! one user's slice of it, so `penalize`/`relax` stay endpoint-wide only;
//! the user gate just paces at a fixed `rpm`.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{EndpointState, RateLimiter};
use crate::UserId;

/// One user's own rpm/concurrency budget on a shared endpoint (#632),
/// resolved from that user's own catalog `ProviderEntry` rather than the
/// (possibly different) user whose session happened to size the endpoint
/// first. Attach to a client handle via [`super::HttpClient::with_user_budget`]
/// before handing it to that user's `Llm` factory.
#[derive(Clone, Debug)]
pub struct UserBudget {
    pub user: UserId,
    pub rpm: Option<u32>,
    pub concurrency: Option<usize>,
}

/// One user's live pacing + concurrency slot on one endpoint. `None` on
/// either axis means that user carries no cap there.
pub(super) struct UserSlot {
    limiter: Option<RateLimiter>,
    semaphore: Option<Arc<Semaphore>>,
    rpm: Option<u32>,
    concurrency: Option<usize>,
}

impl UserSlot {
    fn new(rpm: Option<u32>, concurrency: Option<usize>) -> Self {
        Self {
            limiter: rpm.map(RateLimiter::new),
            semaphore: concurrency.map(|cap| Arc::new(Semaphore::new(cap.max(1)))),
            rpm,
            concurrency,
        }
    }

    /// Pace (if this user carries an rpm cap) then acquire this user's own
    /// concurrency permit (if any). Acquired **before** the model/endpoint
    /// permits (#632) — a caller blocked on its own user slot must never
    /// hold a resource shared with other users/models hostage while it
    /// waits, the same reasoning ADR-0140 applied to the model-vs-endpoint
    /// ordering.
    pub(super) async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        if let Some(limiter) = &self.limiter {
            limiter.acquire().await;
        }
        match &self.semaphore {
            Some(sem) => Some(
                sem.clone()
                    .acquire_owned()
                    .await
                    .expect("user concurrency semaphore never closed"),
            ),
            None => None,
        }
    }
}

impl EndpointState {
    /// Resolve (creating on first use) this endpoint's slot for
    /// `budget.user`. A later call for the same user supplying a *different*
    /// rpm/concurrency corrects the cached slot rather than latching the
    /// first value seen for the rest of the process — the same self-healing
    /// `model_slot` already does (#550).
    pub(super) fn user_slot(&self, budget: &UserBudget) -> Arc<UserSlot> {
        let mut map = self.user_budgets.lock().expect("user budgets poisoned");
        if let Some(slot) = map.get(&budget.user) {
            if slot.rpm == budget.rpm && slot.concurrency == budget.concurrency {
                return slot.clone();
            }
            tracing::warn!(
                user = %budget.user,
                previous_rpm = ?slot.rpm,
                previous_concurrency = ?slot.concurrency,
                corrected_rpm = ?budget.rpm,
                corrected_concurrency = ?budget.concurrency,
                "user rpm/concurrency budget changed for this user — correcting \
                 the cached slot instead of keeping the stale budget for the \
                 rest of the process"
            );
        }
        if let Some(cap) = budget.concurrency {
            if cap > self.concurrency_cap {
                tracing::warn!(
                    user = %budget.user,
                    user_concurrency = cap,
                    endpoint_concurrency = self.concurrency_cap,
                    "user concurrency cap exceeds the endpoint's own — the \
                     endpoint cap binds first, so this user can never reach \
                     their configured cap"
                );
            }
        }
        let slot = Arc::new(UserSlot::new(budget.rpm, budget.concurrency));
        map.insert(budget.user.clone(), slot.clone());
        slot
    }
}

impl super::HttpClient {
    /// Attach `budget` to a clone of this client (#632): every subsequent
    /// [`super::HttpClient::execute_with_retry`] call through the clone
    /// additionally admits through `budget.user`'s own rpm/concurrency slot
    /// on whichever endpoint it talks to, layered under that endpoint's
    /// aggregate cap. The endpoint pool itself stays shared (`pool:
    /// Arc<EndpointPool>` clones cheaply) — only the budget attached to
    /// *this* handle differs, the same way a per-user API key differs while
    /// the transport is shared.
    pub fn with_user_budget(&self, budget: UserBudget) -> Self {
        Self {
            user_budget: Some(Arc::new(budget)),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::HttpClient;

    fn budget(user: &str, rpm: Option<u32>, concurrency: Option<usize>) -> UserBudget {
        UserBudget {
            user: UserId::new(user),
            rpm,
            concurrency,
        }
    }

    #[tokio::test]
    async fn user_slot_bounds_in_flight_per_user() {
        let http = HttpClient::new().unwrap();
        let endpoint = http.endpoint("ep", None, Some(10));
        let slot = endpoint.user_slot(&budget("alice", None, Some(1)));
        let _held = slot.acquire().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), slot.acquire())
                .await
                .is_err(),
            "a second acquire for the same user must wait while the first is held"
        );
    }

    #[tokio::test]
    async fn user_slot_is_independent_across_users_on_the_same_endpoint() {
        let http = HttpClient::new().unwrap();
        let endpoint = http.endpoint("ep", None, Some(10));
        let alice = endpoint.user_slot(&budget("alice", None, Some(1)));
        let bob = endpoint.user_slot(&budget("bob", None, Some(1)));
        let _held = alice.acquire().await;
        // Bob's own slot is untouched by Alice holding hers.
        let bob_permit =
            tokio::time::timeout(std::time::Duration::from_millis(50), bob.acquire()).await;
        assert!(
            bob_permit.is_ok(),
            "bob's slot must not be blocked by alice's"
        );
    }

    #[test]
    fn user_slot_is_stable_while_the_budget_agrees() {
        let http = HttpClient::new().unwrap();
        let endpoint = http.endpoint("ep", None, Some(10));
        let a1 = endpoint.user_slot(&budget("alice", Some(5), Some(2)));
        let a2 = endpoint.user_slot(&budget("alice", Some(5), Some(2)));
        assert!(Arc::ptr_eq(&a1, &a2));
    }

    #[test]
    fn user_slot_corrects_a_changed_budget_instead_of_latching_it_forever() {
        let http = HttpClient::new().unwrap();
        let endpoint = http.endpoint("ep", None, Some(10));
        let wrong = endpoint.user_slot(&budget("alice", None, Some(5)));
        assert_eq!(wrong.concurrency, Some(5));
        let corrected = endpoint.user_slot(&budget("alice", None, Some(1)));
        assert!(!Arc::ptr_eq(&wrong, &corrected));
        assert_eq!(corrected.concurrency, Some(1));
        let again = endpoint.user_slot(&budget("alice", None, Some(1)));
        assert!(Arc::ptr_eq(&corrected, &again));
    }

    #[test]
    fn a_user_with_no_budget_carries_no_gate() {
        let http = HttpClient::new().unwrap();
        let endpoint = http.endpoint("ep", None, Some(10));
        let slot = endpoint.user_slot(&budget("alice", None, None));
        assert!(slot.semaphore.is_none());
        assert!(slot.limiter.is_none());
    }

    #[tokio::test]
    async fn with_user_budget_shares_the_same_endpoint_pool() {
        // Two clients built via `with_user_budget` off the same base client
        // must still resolve to the *same* `EndpointState` for a shared pool
        // key — the whole point is layering a gate on top of one shared
        // endpoint, not forking a separate pool per user.
        let base = HttpClient::new().unwrap();
        let alice_http = base.with_user_budget(budget("alice", None, Some(1)));
        let bob_http = base.with_user_budget(budget("bob", None, Some(1)));
        let ep_a = alice_http.endpoint("shared", None, Some(3));
        let ep_b = bob_http.endpoint("shared", None, Some(3));
        assert!(Arc::ptr_eq(&ep_a, &ep_b));
    }
}
