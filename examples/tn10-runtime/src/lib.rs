//! Shared library for the `tn10-runtime` example.
//!
//! The account-model wire format the `runtime-processor` guest reads is hand-rolled here, ported
//! faithfully from the guest's proven e2e harness
//! (`zk/backend/risc0/runtime-processor/tests/e2e.rs`). Both the driver binary (`src/main.rs`) and
//! the acceptance test (`tests/runtime_actions.rs`) build their bytes through [`actions`], so a
//! single set of encoders is exercised by the test and shipped by the driver.

pub mod actions;
pub mod config;
pub mod deposit;
