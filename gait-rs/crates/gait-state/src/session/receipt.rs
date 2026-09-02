//! Persisted first-observation receipts, for checking a `DelegationCert`'s
//! authorization window against the moment gait itself first saw an event --
//! never against `Event.ts` (self-reported, unprotected by the signature)
//! and never against a live clock read on every Retrace fold (Retrace is
//! documented and relied upon as a pure, safely-replayable function of the
//! event set; a live `now()` inside the fold would make the same historical
//! event's authorization verdict depend on *when* it was replayed, which is
//! a determinism bug, not just an edge case).
//!
//! `record_first_observation` is idempotent by design: called on every fold
//! for every delegated event, it only actually stamps a fresh `(content_hash,
//! provenance_material)` pair the first time it's seen; a later fold's
//! genuine duplicate is a no-op that leaves the original timestamp
//! untouched. `lookup` is the read side the authorization check consults --
//! it never itself reads a clock. See
//! an internal design note §5.
//!
//! The receipt is bound to `content_hash` (the event's actual signed
//! content, once verified) plus `provenance_material` (the signature bytes
//! that were checked against it) together, not `Event.id` alone --
//! `Event.id` and the rest of the envelope are not covered by
//! `Provenance.signature`, so a bare `id`-keyed lookup would let an attacker
//! present *different*, newly-signed content under an `id` that already has
//! a favorable receipt on file. A `content_hash` collision under the same
//! `id` bound to *different* material fails closed rather than silently
//! reusing or replacing the existing record.

use std::collections::HashMap;

pub struct ReceiptStore {
    records: HashMap<String, (Vec<u8>, i64)>,
}

impl Default for ReceiptStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ReceiptStore {
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
        }
    }

    /// Record `content_hash` as first observed, bound to `provenance_material`,
    /// at `observed_at_unix_seconds` -- but only if it hasn't been recorded
    /// before. A genuine repeat of the same `(content_hash, provenance_material)`
    /// pair is a no-op that preserves the original timestamp (the caller may
    /// pass a fresh `now()` on every fold; only the first call for a given
    /// pair actually stamps anything). A `content_hash` already bound to
    /// *different* material fails closed -- refuses to overwrite -- rather
    /// than reusing or silently replacing the existing record.
    pub fn record_first_observation(
        &mut self,
        content_hash: &str,
        provenance_material: &[u8],
        observed_at_unix_seconds: i64,
    ) -> anyhow::Result<()> {
        match self.records.get(content_hash) {
            None => {
                self.records.insert(
                    content_hash.to_string(),
                    (provenance_material.to_vec(), observed_at_unix_seconds),
                );
                Ok(())
            }
            Some((existing_material, _)) if existing_material == provenance_material => {
                Ok(()) // genuine duplicate delivery -- no-op, keep the original timestamp
            }
            Some(_) => {
                anyhow::bail!(
                    "content_hash {content_hash} already has a receipt bound to different material -- refusing to overwrite"
                )
            }
        }
    }

    /// `(provenance_material, observed_at_unix_seconds)` for `content_hash`,
    /// if a receipt has ever been recorded for it. Never reads a clock.
    pub fn lookup(&self, content_hash: &str) -> Option<(Vec<u8>, i64)> {
        self.records.get(content_hash).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duplicate_content_hash_with_different_material_fails_closed() {
        let mut store = ReceiptStore::new();
        store
            .record_first_observation("hash-a", b"material-1", 1000)
            .unwrap();
        let result = store.record_first_observation("hash-a", b"material-2", 2000);
        assert!(
            result.is_err(),
            "different material under a reused content_hash must be rejected, not silently replace the original record"
        );
        // The original record must be untouched.
        let (material, observed_at) = store.lookup("hash-a").unwrap();
        assert_eq!(material, b"material-1");
        assert_eq!(observed_at, 1000);
    }

    #[test]
    fn a_genuine_duplicate_delivery_of_the_same_event_is_a_no_op() {
        let mut store = ReceiptStore::new();
        store
            .record_first_observation("hash-b", b"material", 1000)
            .unwrap();
        let result = store.record_first_observation("hash-b", b"material", 9999);
        assert!(
            result.is_ok(),
            "the exact same (content_hash, material) pair arriving again must be a no-op"
        );
        let (_material, observed_at) = store.lookup("hash-b").unwrap();
        assert_eq!(
            observed_at, 1000,
            "the ORIGINAL observation time must be preserved, not overwritten by the later duplicate delivery"
        );
    }

    #[test]
    fn lookup_of_unrecorded_hash_is_none() {
        let store = ReceiptStore::new();
        assert!(store.lookup("never-seen").is_none());
    }

    /// Demonstrates the replay-determinism property `ReceiptStore` exists to
    /// guarantee, using a small local authorization check standing in for
    /// the real one `ActiveRawMessages::apply` will consult once this store
    /// is wired into a real, disk-persisted call site (deferred -- see the
    /// module doc and the commit this test lands in): a `DelegationCert`'s
    /// window is checked against the store's *stable, recorded* observation
    /// time, never a live clock read inside the check itself. Folding the
    /// same event twice, at two very different wall-clock moments (one well
    /// after the cert's window has closed), must agree -- exactly the
    /// property that would break if the check called `SystemTime::now()`
    /// directly instead of `store.lookup(..)`.
    #[test]
    fn folding_the_same_event_twice_produces_the_same_authorization_result_even_after_a_certs_window_closes(
    ) {
        fn is_authorized(store: &ReceiptStore, content_hash: &str, not_before: i64, not_after: i64) -> bool {
            match store.lookup(content_hash) {
                Some((_material, observed_at)) => observed_at >= not_before && observed_at < not_after,
                None => false, // no observation on file -- fail closed
            }
        }

        let mut store = ReceiptStore::new();
        // The event was genuinely first observed at t=150, inside a cert
        // window of [100, 200).
        store
            .record_first_observation("hash-c", b"sig-bytes", 150)
            .unwrap();

        // First "fold": authorized, as expected.
        let first_fold = is_authorized(&store, "hash-c", 100, 200);
        assert!(first_fold, "observed inside the window -- must be authorized");

        // A later fold re-observes the identical event (e.g. the log was
        // replayed after the cert's window has long since closed in real
        // wall-clock time) -- record_first_observation is idempotent, so
        // this must be a no-op that leaves the t=150 receipt untouched.
        store
            .record_first_observation("hash-c", b"sig-bytes", 999_999)
            .unwrap();
        let second_fold = is_authorized(&store, "hash-c", 100, 200);

        assert_eq!(
            first_fold, second_fold,
            "two folds of the identical event must agree -- the authorization check must use the stable recorded receipt time, not a live clock read on replay"
        );
    }
}
