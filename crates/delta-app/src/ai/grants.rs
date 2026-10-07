//! Revocation of what a run was authorized to use (F-09, R1-A-18;
//! context-state-management §4 and §5, data-and-security: "撤权与关闭使运行失效").
//!
//! R1 defines two revocable authorizations:
//!
//! 1. **A model connection.** The user disables or removes it. Nothing more is
//!    sent to that endpoint, by any run.
//! 2. **The accounts the model may read.** Narrowing the authorized account set
//!    stops every run whose frozen scope reaches an account that is no longer
//!    authorized. Notes reach the model only through their account (notes carry
//!    an account and retrieval filters on the scope), so narrowing accounts
//!    also narrows notes; there is no separate note grant in R1.
//!
//! A run holds a [`RunLease`] from admission to its end. Revoking cancels the
//! lease's token, which every network wait, backoff and summary request in the
//! run already selects on, and records why. The checkpoint write goes through
//! [`RunLease::commit`], which holds the same lock `revoke_*` takes, so a
//! revocation either lands before the write (the write does not happen) or
//! after it (the write was authorized when it happened).

use crate::contracts::CancelToken;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

/// Why a run lost its authorization. Carries ids only, never content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revocation {
    /// The model connection with this id was revoked.
    Connection(String),
    /// These accounts, used by the run's frozen scope, are no longer authorized.
    Accounts(Vec<String>),
}

impl std::fmt::Display for Revocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Revocation::Connection(id) => write!(f, "model connection {id} revoked"),
            Revocation::Accounts(ids) => {
                write!(f, "accounts no longer authorized: {}", ids.join(", "))
            }
        }
    }
}

struct Lease {
    connection_id: String,
    accounts: Vec<String>,
    token: CancelToken,
    revocation: Option<Revocation>,
}

#[derive(Default)]
struct State {
    revoked_connections: BTreeSet<String>,
    /// `None`: every account in the library may be read (the default).
    allowed_accounts: Option<BTreeSet<String>>,
    next_lease: u64,
    leases: HashMap<u64, Lease>,
}

/// Shared authorization state. Clone it to hand the same grants to the UI
/// (which revokes) and to the runtime (which obeys).
#[derive(Clone, Default)]
pub struct Grants {
    inner: Arc<Mutex<State>>,
}

fn lock(inner: &Mutex<State>) -> MutexGuard<'_, State> {
    // A panic elsewhere must not turn a revocation into a no-op.
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

impl Grants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Revoke a model connection: running runs on it stop, new ones are refused.
    /// Returns how many running runs were stopped.
    pub fn revoke_connection(&self, connection_id: &str) -> usize {
        let mut st = lock(&self.inner);
        st.revoked_connections.insert(connection_id.to_string());
        let mut stopped = 0;
        for lease in st.leases.values_mut() {
            if lease.connection_id == connection_id && lease.revocation.is_none() {
                lease.revocation = Some(Revocation::Connection(connection_id.to_string()));
                lease.token.cancel();
                stopped += 1;
            }
        }
        stopped
    }

    /// Authorize a revoked connection again (the user re-enabled it). Runs that
    /// were already stopped stay stopped.
    pub fn restore_connection(&self, connection_id: &str) {
        lock(&self.inner).revoked_connections.remove(connection_id);
    }

    /// Limit the model to these accounts. Running runs whose frozen scope uses
    /// any other account stop; runs fully inside the new set carry on.
    /// Returns how many running runs were stopped.
    pub fn restrict_accounts<I, S>(&self, allowed: I) -> usize
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let allowed: BTreeSet<String> = allowed.into_iter().map(Into::into).collect();
        let mut st = lock(&self.inner);
        let mut stopped = 0;
        for lease in st.leases.values_mut() {
            let denied: Vec<String> = lease
                .accounts
                .iter()
                .filter(|a| !allowed.contains(*a))
                .cloned()
                .collect();
            if !denied.is_empty() && lease.revocation.is_none() {
                lease.revocation = Some(Revocation::Accounts(denied));
                lease.token.cancel();
                stopped += 1;
            }
        }
        st.allowed_accounts = Some(allowed);
        stopped
    }

    /// Lift an account restriction: every account may be read again.
    pub fn allow_all_accounts(&self) {
        lock(&self.inner).allowed_accounts = None;
    }

    /// Admit a run: refuse it if its connection or accounts are not
    /// authorized now, otherwise hand out the lease it holds until it ends.
    /// The lease token is a child of `parent`, so the caller's own cancel
    /// still stops the run.
    pub fn admit(
        &self,
        connection_id: &str,
        accounts: &[String],
        parent: &CancelToken,
    ) -> Result<RunLease, Revocation> {
        let mut st = lock(&self.inner);
        if st.revoked_connections.contains(connection_id) {
            return Err(Revocation::Connection(connection_id.to_string()));
        }
        if let Some(allowed) = &st.allowed_accounts {
            let denied: Vec<String> = accounts
                .iter()
                .filter(|a| !allowed.contains(*a))
                .cloned()
                .collect();
            if !denied.is_empty() {
                return Err(Revocation::Accounts(denied));
            }
        }
        let token = CancelToken(Arc::new(parent.0.child_token()));
        let id = st.next_lease;
        st.next_lease += 1;
        st.leases.insert(
            id,
            Lease {
                connection_id: connection_id.to_string(),
                accounts: accounts.to_vec(),
                token: token.clone(),
                revocation: None,
            },
        );
        Ok(RunLease {
            grants: self.clone(),
            id,
            token,
        })
    }

    /// Number of runs currently holding a lease.
    pub fn active_runs(&self) -> usize {
        lock(&self.inner).leases.len()
    }
}

/// A run's authorization, held until the run ends.
pub struct RunLease {
    grants: Grants,
    id: u64,
    token: CancelToken,
}

impl RunLease {
    /// The run's cancel token: cancelled by the caller or by a revocation.
    pub fn token(&self) -> CancelToken {
        self.token.clone()
    }

    /// Why this run was revoked, if it was.
    pub fn revocation(&self) -> Option<Revocation> {
        lock(&self.grants.inner)
            .leases
            .get(&self.id)
            .and_then(|lease| lease.revocation.clone())
    }

    /// Run `write` only if the run is still authorized. The grants lock is held
    /// for the duration, so a concurrent revocation waits for the write; it
    /// cannot land between the check and the write.
    pub fn commit<T>(&self, write: impl FnOnce() -> T) -> Result<T, Revocation> {
        let st = lock(&self.grants.inner);
        if let Some(revocation) = st
            .leases
            .get(&self.id)
            .and_then(|lease| lease.revocation.clone())
        {
            return Err(revocation);
        }
        Ok(write())
    }
}

impl Drop for RunLease {
    fn drop(&mut self) {
        lock(&self.grants.inner).leases.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn r1_a_18_revoking_a_connection_stops_its_runs_and_refuses_new_ones() {
        let grants = Grants::new();
        let parent = CancelToken::new();
        let on_a = grants.admit("conn-a", &accounts(&["x"]), &parent).unwrap();
        let on_b = grants.admit("conn-b", &accounts(&["x"]), &parent).unwrap();
        assert_eq!(grants.active_runs(), 2);

        assert_eq!(grants.revoke_connection("conn-a"), 1);
        assert!(on_a.token().is_cancelled());
        assert_eq!(
            on_a.revocation(),
            Some(Revocation::Connection("conn-a".into()))
        );
        assert!(!on_b.token().is_cancelled(), "other connections carry on");
        assert!(on_b.revocation().is_none());
        // The caller's own token is not touched by the revocation.
        assert!(!parent.is_cancelled());
        // New runs on the revoked connection are refused until it is restored.
        assert_eq!(
            grants
                .admit("conn-a", &accounts(&["x"]), &parent)
                .err()
                .unwrap(),
            Revocation::Connection("conn-a".into())
        );
        grants.restore_connection("conn-a");
        let again = grants.admit("conn-a", &accounts(&["x"]), &parent).unwrap();
        // The run that was stopped stays stopped.
        assert!(on_a.revocation().is_some());
        assert!(again.revocation().is_none());
        drop((on_a, on_b));
        assert_eq!(
            grants.active_runs(),
            1,
            "only the re-admitted lease is left"
        );
        drop(again);
        assert_eq!(grants.active_runs(), 0, "leases are released when runs end");
    }

    #[test]
    fn r1_a_18_narrowing_accounts_stops_only_runs_that_reach_a_removed_account() {
        let grants = Grants::new();
        let parent = CancelToken::new();
        let wide = grants
            .admit("c", &accounts(&["acc-us", "acc-crypto"]), &parent)
            .unwrap();
        let narrow = grants.admit("c", &accounts(&["acc-us"]), &parent).unwrap();
        assert_eq!(grants.restrict_accounts(["acc-us"]), 1);
        assert_eq!(
            wide.revocation(),
            Some(Revocation::Accounts(vec!["acc-crypto".into()]))
        );
        assert!(wide.token().is_cancelled());
        assert!(narrow.revocation().is_none());
        assert!(!narrow.token().is_cancelled());
        // Admission now enforces the same set.
        assert_eq!(
            grants
                .admit("c", &accounts(&["acc-us", "acc-crypto"]), &parent)
                .err()
                .unwrap(),
            Revocation::Accounts(vec!["acc-crypto".into()])
        );
        grants.allow_all_accounts();
        assert!(grants
            .admit("c", &accounts(&["acc-us", "acc-crypto"]), &parent)
            .is_ok());
    }

    #[test]
    fn r1_a_18_the_callers_cancel_still_stops_a_leased_run() {
        let grants = Grants::new();
        let parent = CancelToken::new();
        let lease = grants.admit("c", &accounts(&["a"]), &parent).unwrap();
        parent.cancel();
        assert!(lease.token().is_cancelled());
        assert!(
            lease.revocation().is_none(),
            "a plain cancel is not a revocation"
        );
    }

    #[test]
    fn r1_a_18_commit_does_not_write_after_a_revocation() {
        let grants = Grants::new();
        let parent = CancelToken::new();
        let lease = grants.admit("c", &accounts(&["a"]), &parent).unwrap();
        assert_eq!(lease.commit(|| 7), Ok(7));
        grants.revoke_connection("c");
        let mut wrote = false;
        let result = lease.commit(|| wrote = true);
        assert_eq!(result, Err(Revocation::Connection("c".into())));
        assert!(!wrote, "a revoked run must not write");
    }

    #[test]
    fn r1_a_18_a_revocation_waits_for_a_write_that_already_started() {
        use std::sync::mpsc;
        let grants = Grants::new();
        let parent = CancelToken::new();
        let lease = grants.admit("c", &accounts(&["a"]), &parent).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let revoker = grants.clone();
        let writer = std::thread::scope(|scope| {
            let lease_ref = &lease;
            let write = scope.spawn(move || {
                lease_ref.commit(|| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    "written"
                })
            });
            started_rx.recv().unwrap();
            let revoke = scope.spawn(move || revoker.revoke_connection("c"));
            // The revocation cannot finish while the write holds the lock.
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(
                !revoke.is_finished(),
                "revocation overtook an in-flight write"
            );
            release_tx.send(()).unwrap();
            assert_eq!(revoke.join().unwrap(), 1);
            write.join().unwrap()
        });
        assert_eq!(
            writer,
            Ok("written"),
            "the write had been authorized when it began"
        );
        assert!(
            lease.revocation().is_some(),
            "and the run is revoked right after"
        );
    }
}
