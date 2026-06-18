//! Library surface of the tn10-flow example, exposing the node-building [`daemon`] module so
//! integration tests can construct proving nodes the same way the binary does.
//!
//! The binary ([`main.rs`](../main.rs)) keeps its own private `mod daemon;`; this lib target exists
//! only to let tests reuse the node wiring without duplicating it.

pub mod daemon;
