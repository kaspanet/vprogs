//! Closure-based combinators on `Resource<'a>` that layer typed views onto
//! the framework's byte-backed lifecycle.
//!
//! The framework's `Resource<'a>` already encodes the state machine the
//! design doc describes (`is_new()` / `is_dirty()` / `resize()`), with deletion
//! collapsed onto empty data (`data().is_empty()`).
//! This extension trait adds:
//!
//! - `kind()`: read the leading kind byte (`KIND_CONFIG` / `KIND_USER` / ...).
//! - `view_*`: closure receives a borrowed typed view; returns `None` on kind mismatch or empty
//!   slot (`is_new() || data().is_empty()`).
//! - `modify_*`: closure receives a mutable typed view; takes `&mut Resource` so the framework
//!   marks the resource dirty internally.
//! - `init_*`: fresh-resource init requiring `is_new()`, then `resize()` to the new payload's
//!   length and writes the canonical wire bytes.
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
        if self.is_new() || self.data().is_empty() {
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
        lock: &LockEnum<'_>,
    ) -> Result<(), &'static str> {
        if !self.is_new() {
            return Err("init_config: resource not new");
        }
        let total = config_total_len(lock);
        self.resize(total);
        write_config(self.data_mut(), min_withdrawal_amount, lock)
    }

    fn init_user(
        &mut self,
        balance: u64,
        initial_lock_hash: &[u8; 32],
        lock: &LockEnum<'_>,
    ) -> Result<(), &'static str> {
        if !self.is_new() {
            return Err("init_user: resource not new");
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
        let min_withdrawal_amount = ConfigView::from_bytes(self.data())?.min_withdrawal_amount();
        let new_total = CONFIG_HEADER_LEN + new_lock.wire_body_len();
        self.resize(new_total);
        write_config(self.data_mut(), min_withdrawal_amount, new_lock)
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
