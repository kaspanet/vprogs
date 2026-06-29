//! Closure-based combinators on `Resource<'a>` that layer typed views onto
//! the framework's byte-backed storage.
//!
//! The framework's `Resource<'a>` is byte-backed and lifecycle-agnostic (`data()` / `is_dirty()` /
//! `resize()`); the per-tx lifecycle state machine lives one layer up in
//! [`ApplyContext`](crate::action::ApplyContext) (see [`crate::lifecycle`]). This extension trait
//! layers typed views onto the bytes, reading liveness off data-emptiness alone:
//!
//! - `kind()`: read the leading kind byte (`KIND_CONFIG` / `KIND_USER` / ...); `None` on an empty
//!   slot (never-created or torn-down).
//! - `view_*`: closure receives a borrowed typed view; returns `None` on kind mismatch or an empty
//!   slot, which is what rejects use-after-delete here.
//! - `modify_*`: closure receives a mutable typed view; takes `&mut Resource` so the framework
//!   marks the resource dirty internally.
//! - `init_*`: fresh-resource init into an empty slot; `resize()` to the new payload's length and
//!   writes the canonical wire bytes. The caller owns the `New -> Live` transition via
//!   `ApplyContext::mark_created`, which rejects double-create and re-create-after-delete.
//! - `set_*_lock`: variable-tail rewrite for lock rotation. Handles shrink, same-size, and grow by
//!   deferring to `Resource::resize` (the framework promotes to a heap buffer when needed).

use vprogs_zk_abi::transaction_processor::Resource;

use crate::{
    config::{CONFIG_HEADER_LEN, ConfigView, ConfigViewMut, config_total_len, write_config},
    kind::{KIND_CONFIG, KIND_USER, kind_of},
    lock::LockEnum,
    user::{USER_HEADER_LEN, UserView, UserViewMut, user_total_len, write_user},
};

/// Extension trait. Implemented for `Resource<'a>`; see module docs.
pub trait ResourceExt<'a> {
    fn kind(&self) -> Option<u8>;

    fn view_config<R>(&self, f: impl FnOnce(ConfigView<'_>) -> R) -> Option<R>;
    fn view_user<R>(&self, f: impl FnOnce(UserView<'_>) -> R) -> Option<R>;

    fn modify_config<R>(&mut self, f: impl FnOnce(&mut ConfigViewMut<'_>) -> R) -> Option<R>;
    fn modify_user<R>(&mut self, f: impl FnOnce(&mut UserViewMut<'_>) -> R) -> Option<R>;

    fn init_config(
        &mut self,
        min_withdrawal_amount: u64,
        covenant_id: &[u8; 32],
        lock: &LockEnum<'_>,
    ) -> Result<(), &'static str>;
    fn init_user(
        &mut self,
        balance: u64,
        initial_lock_hash: &[u8; 32],
        lock: &LockEnum<'_>,
    ) -> Result<(), &'static str>;

    fn set_config_lock(&mut self, new_lock: &LockEnum<'_>) -> Result<(), &'static str>;
    fn set_user_lock(&mut self, new_lock: &LockEnum<'_>) -> Result<(), &'static str>;
}

impl<'a> ResourceExt<'a> for Resource<'a> {
    fn kind(&self) -> Option<u8> {
        // Empty data means the slot holds no live resource: a `New` slot has nothing yet and a
        // `Deleted` one was emptied on teardown. Reading both off data-emptiness keeps this view
        // layer lifecycle-agnostic (the lifecycle lives in `ApplyContext`) while still rejecting
        // use-after-delete: `view_*`/`modify_*` return `None` on an emptied slot.
        if self.data().is_empty() {
            return None;
        }
        kind_of(self.data())
    }

    fn view_config<R>(&self, f: impl FnOnce(ConfigView<'_>) -> R) -> Option<R> {
        if self.kind()? != KIND_CONFIG {
            return None;
        }
        ConfigView::from_bytes(self.data()).ok().map(f)
    }

    fn view_user<R>(&self, f: impl FnOnce(UserView<'_>) -> R) -> Option<R> {
        if self.kind()? != KIND_USER {
            return None;
        }
        UserView::from_bytes(self.data()).ok().map(f)
    }

    fn modify_config<R>(&mut self, f: impl FnOnce(&mut ConfigViewMut<'_>) -> R) -> Option<R> {
        if self.kind()? != KIND_CONFIG {
            return None;
        }
        let mut mv = ConfigViewMut::from_bytes_mut(self.data_mut()).ok()?;
        Some(f(&mut mv))
    }

    fn modify_user<R>(&mut self, f: impl FnOnce(&mut UserViewMut<'_>) -> R) -> Option<R> {
        if self.kind()? != KIND_USER {
            return None;
        }
        let mut mv = UserViewMut::from_bytes_mut(self.data_mut()).ok()?;
        Some(f(&mut mv))
    }

    fn init_config(
        &mut self,
        min_withdrawal_amount: u64,
        covenant_id: &[u8; 32],
        lock: &LockEnum<'_>,
    ) -> Result<(), &'static str> {
        if !self.data().is_empty() {
            return Err("init_config: slot is not empty");
        }
        let total = config_total_len(lock);
        self.resize(total);
        write_config(self.data_mut(), min_withdrawal_amount, covenant_id, lock)
    }

    fn init_user(
        &mut self,
        balance: u64,
        initial_lock_hash: &[u8; 32],
        lock: &LockEnum<'_>,
    ) -> Result<(), &'static str> {
        if !self.data().is_empty() {
            return Err("init_user: slot is not empty");
        }
        let total = user_total_len(lock);
        self.resize(total);
        write_user(self.data_mut(), balance, initial_lock_hash, lock)
    }

    fn set_config_lock(&mut self, new_lock: &LockEnum<'_>) -> Result<(), &'static str> {
        if self.kind() != Some(KIND_CONFIG) {
            return Err("set_config_lock: not a config resource");
        }
        // Snapshot fixed-header fields before resize (which would invalidate
        // the existing data slice).
        let (min_withdrawal_amount, covenant_id) = {
            let view = ConfigView::from_bytes(self.data())?;
            (view.min_withdrawal_amount(), *view.covenant_id())
        };
        let new_total = CONFIG_HEADER_LEN + new_lock.wire_body_len();
        self.resize(new_total);
        write_config(self.data_mut(), min_withdrawal_amount, &covenant_id, new_lock)
    }

    fn set_user_lock(&mut self, new_lock: &LockEnum<'_>) -> Result<(), &'static str> {
        if self.kind() != Some(KIND_USER) {
            return Err("set_user_lock: not a user resource");
        }
        let (balance, ilh) = {
            let view = UserView::from_bytes(self.data())?;
            (view.balance(), *view.initial_lock_hash())
        };
        let new_total = USER_HEADER_LEN + new_lock.wire_body_len();
        self.resize(new_total);
        write_user(self.data_mut(), balance, &ilh, new_lock)
    }
}
