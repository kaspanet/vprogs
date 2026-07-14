//! Per-resource lifecycle state machine, tracked by the runtime as a transaction's actions apply.
//!
//! The ABI's `Resource` is deliberately lifecycle-agnostic: it only knows the host's `is_new`
//! snapshot, its current bytes, and a dirty flag, which is all the journal needs (a changed
//! resource commits `hash(data)`, an emptied one commits `EMPTY_HASH`). *Whether* a slot may be
//! created, credited, or deleted is an application concern, so the lifecycle lives here and in
//! [`ApplyContext`](crate::action::ApplyContext), not in the ABI.
//!
//! The state advances *within* a single transaction (`New -> Live -> Deleted`), so an action reads
//! the effect of an earlier same-tx action rather than the stale input snapshot; the create-vs-
//! credit decision is made from the live state. `Deleted` is terminal and distinct from `New`, so a
//! torn-down slot can be told apart from a never-created one and re-creation is rejected.

use vprogs_zk_abi::transaction_processor::Resource;

/// Where a resource slot sits in its create/use/delete lifecycle for the current transaction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lifecycle {
    /// Slot holds no resource: it neither existed in committed state nor has been created yet by
    /// this tx. The only legal move out is a create transition ([`Lifecycle::created`]).
    New,
    /// Slot holds live data: either committed prior state, or a resource created earlier in this
    /// tx. Reads and writes are valid; it may be deleted ([`Lifecycle::deleted`]).
    Live,
    /// Slot was torn down: committed-empty, or deleted earlier in this tx. Terminal: both reads
    /// and re-creation are rejected.
    Deleted,
}

impl Lifecycle {
    /// Maps a freshly decoded resource to its starting lifecycle: a new slot is `New`; an existing
    /// slot with data is `Live`; an existing-but-empty slot is `Deleted` (torn down before this
    /// tx).
    pub fn from_resource(r: &Resource<'_>) -> Self {
        match (r.is_new(), r.data().is_empty()) {
            (true, _) => Self::New,
            (false, false) => Self::Live,
            (false, true) => Self::Deleted,
        }
    }

    /// Create transition `New -> Live`. Errors for any other source state, so a same-tx double
    /// create or a re-create after delete is rejected instead of silently overwriting the slot the
    /// earlier action wrote.
    pub fn created(self) -> Result<Self, &'static str> {
        match self {
            Self::New => Ok(Self::Live),
            Self::Live => Err("lifecycle: double create (slot already live)"),
            Self::Deleted => Err("lifecycle: re-create after delete"),
        }
    }

    /// Delete transition `Live -> Deleted`. Errors unless currently live, so deleting a
    /// never-created slot or double-deleting is rejected.
    pub fn deleted(self) -> Result<Self, &'static str> {
        match self {
            Self::Live => Ok(Self::Deleted),
            Self::New => Err("lifecycle: delete of never-created slot"),
            Self::Deleted => Err("lifecycle: double delete"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `New -> Live` is the only legal create; a second create on a live slot is rejected (the
    /// within-tx double-deposit fund-loss path), as is re-creating a deleted slot.
    #[test]
    fn create_transition_rules() {
        assert_eq!(Lifecycle::New.created(), Ok(Lifecycle::Live));
        assert!(Lifecycle::Live.created().is_err());
        assert!(Lifecycle::Deleted.created().is_err());
    }

    /// `Live -> Deleted` is the only legal delete; deleting a `New` or already-`Deleted` slot is
    /// rejected.
    #[test]
    fn delete_transition_rules() {
        assert_eq!(Lifecycle::Live.deleted(), Ok(Lifecycle::Deleted));
        assert!(Lifecycle::New.deleted().is_err());
        assert!(Lifecycle::Deleted.deleted().is_err());
    }

    /// The full ephemeral path a single tx can drive: create then tear down.
    #[test]
    fn ephemeral_new_live_deleted() {
        let live = Lifecycle::New.created().unwrap();
        assert_eq!(live, Lifecycle::Live);
        assert_eq!(live.deleted().unwrap(), Lifecycle::Deleted);
    }
}
