# tn10-flow settlement — resumable store/covenant (future work)

> Working note. Today (`TN10_SETTLE=1`) every run bootstraps a **fresh** covenant and therefore
> needs a **fresh** data dir. This file is the plan to make a settlement run resumable so one stable
> `TN10_DATA_DIR` can be reused across restarts. Until then: use a clean dir per run (the
> `tn10-settle` fish helper timestamps it).

## The invariant (why a dirty store breaks today)

A covenant's committed L2 `state` lives on **L1**, baked into the redeem script of its UTXO. The SMT
that *produces* that state lives in the **L2 store** (the RocksDB `db` dir). They must agree:

```
store SMT root (at the version the next bundle chains from)  ==  live covenant.state
```

`settler::settle_bundle` asserts exactly this before submitting:

```
parsed.prev_state == cov.state   // "settlement prev_state must chain from the live covenant state"
```

The prover reads `prev_state` from the store at `prev_version = first_batch_index - 1` (the empty SMT
on a fresh store → `EMPTY_HASH`). A fresh covenant is bootstrapped at `EMPTY_HASH`, so the two match
**only when the store is also empty**. Reusing a non-empty `db` with a freshly bootstrapped covenant
violates the invariant and the assert fires (or the on-chain script rejects the settlement).

Second coupling: the framework `Node` resumes the L1 bridge cursor from the store's last checkpoint.
A leftover store would make the bridge resume mid-chain while pointing at a brand-new covenant —
inconsistent on top of the state mismatch.

So the rule is: **the store and the covenant must be born together.** Because `start_settlement`
unconditionally bootstraps, the store must be reset every run.

## Target behavior

Reuse one `TN10_DATA_DIR`: on restart, skip the bootstrap, reload the live covenant, and keep the
existing `db` — the store's SMT already matches the persisted covenant, and the bridge resumes where
it left off.

## Plan

### 1. Persist the live covenant (write-through)

Extend `persistence::PersistedState` with the full `settler::CovenantState`, serialized as the
program's own byte format (id, outpoint `txid:index`, `state` 32 bytes, `lane_tip`, `value`,
`daa_score`). Write it:

- after the bootstrap confirms, and
- after **each** confirmed settlement (in `settler::run`, where `cov` is advanced).

Persisted covenant present ⇒ this is a resume; absent ⇒ first run.

### 2. Branch bootstrap vs resume in `start_settlement`

- **Resume** (persisted covenant present, `db` exists): skip `bootstrap_real_covenant`, load the
  `CovenantState`, open the existing store. Do **not** wipe.
- **Fresh** (no persisted covenant): bootstrap as today; assert/ensure the `db` dir is empty first
  (refuse to start on a dirty dir without a persisted covenant, rather than corrupt silently).

### 3. Startup consistency guard

Before settling on resume, read the store's current SMT root and assert it equals the persisted
`covenant.state`. On mismatch, refuse to start with a clear message (the store and covenant came from
different runs) instead of letting the per-bundle assert fire deep in the loop.

### 4. Reconcile an in-flight settlement (crash between submit and confirm)

The hard case. If the process died after `submit_transaction` but before `confirm_outpoint`, the
persisted covenant is still the *pre*-settlement one, while the settlement may or may not have landed.
On restart, before resuming normal settling:

- Derive the candidate continuation outpoint(s) and query the node (`get_utxos_by_addresses` on the
  continuation P2SH address). If the continuation UTXO exists → adopt it as the live covenant
  (advance + persist). If only the previous covenant UTXO is still unspent → re-settle from it.
- This is the RPC-driven analogue of the sim's `pending` / `REISSUE_DEADLINE` reconciliation
  (`sim/src/driver.rs`); port that logic rather than reinventing it.

### 5. Reorg after restart

Single-miner/low-reorg is still assumed (an in-flight bundle proof isn't cancellable on orphan — a
framework TODO, see `sim/HANDOFF.md` §6). `BatchEvent::RolledBack` already lets the settler drop
orphaned unproved batches; resume must additionally re-derive the live covenant from the chain if a
reorg orphaned the last persisted settlement. Out of scope for the first resumable version — keep the
no-reorg assumption and document it.

## Touch points

- `examples/tn10-flow/src/persistence.rs` — add the covenant fields + (de)serialization.
- `examples/tn10-flow/src/settler.rs` — load `CovenantState` from persistence; persist after each
  confirmed settlement; startup SMT-root guard; in-flight reconciliation.
- `examples/tn10-flow/src/main.rs` (`start_settlement`) — branch fresh-bootstrap vs resume; empty-dir
  guard on fresh.
- Reference impl for reconciliation: `sim/src/driver.rs` (`PendingCovenant`, `observe_covenant`,
  `rollback_covenant`).

## Related (not part of resume, but will bite a long-lived run)

- **Funding contention:** the issuer and the settler share one key and both take the largest UTXO; a
  tight activity cadence contends with settlement fees. A dedicated issuer key (or reserving a fee
  UTXO) is the clean fix.
- **Bridge backfill:** a fresh store makes the bridge follow from its default start; a resumed store
  continues from the last checkpoint, which is the intended steady state.
