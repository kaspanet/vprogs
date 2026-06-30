//! Milestone 4: a deterministic scenario engine with an expected-state model.
//!
//! A scenario is a list of [`Action`]s in SUBMISSION order (which may differ from logical order).
//! An in-test [`Model`] predicts, action-by-action in execution order, whether each action is
//! accepted or rejected by the runtime and the resulting balances, mirroring the runtime's
//! per-resource lifecycle rules (a premature action, e.g. transfer-before-deposit, is rejected and
//! the whole tx reverts). The engine then drives the SAME sequence through the real runtime ELF via
//! the scheduler and asserts the committed state matches the model exactly.
//!
//! This is the "deterministic set of txs verifies we handle everything, caring about the state that
//! should be" tool. Run under `RISC0_DEV_MODE=1`.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use kaspa_hashes::Hash;
use tempfile::TempDir;
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_test_suite::{
    L1TransactionExt, batch_aggregator_elf, batch_processor_elf,
    runtime_flow::{
        EXAMPLE_DEPOSIT_COVENANT_ID, EXAMPLE_MIN_CREATE_BALANCE, RuntimeSigner, deposit_tx,
        init_config_tx, transfer_create_tx, transfer_tx, update_config_tx, user_balance,
        withdraw_tx,
    },
    runtime_processor_elf,
};
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// A runtime action in a scenario. User handles are deterministic seeds (`RuntimeSigner::user`).
#[derive(Clone, Debug)]
enum Action {
    InitConfig { min_withdrawal: u64 },
    UpdateConfig { new_min_withdrawal: u64 },
    Deposit { user: u64, value: u64 },
    Transfer { src: u64, dst: u64, amount: u64 },
    TransferCreate { src: u64, dst: u64, amount: u64 },
    Withdraw { user: u64, amount: u64 },
}

/// The expected-state model: predicts acceptance + balances per the runtime's rules.
struct Model {
    min_create_balance: u64,
    config_min_withdrawal: Option<u64>,
    balances: BTreeMap<u64, u64>,
    exits: u64,
}

impl Model {
    /// A fresh model: config absent, no live users, no exits.
    fn new() -> Self {
        Self {
            min_create_balance: EXAMPLE_MIN_CREATE_BALANCE,
            config_min_withdrawal: None,
            balances: BTreeMap::new(),
            exits: 0,
        }
    }

    /// Predicts whether `action` is accepted, mutating the model on acceptance. Mirrors the
    /// runtime: a rejected action leaves state untouched (the whole tx reverts).
    fn predict(&mut self, action: &Action) -> bool {
        match *action {
            Action::InitConfig { min_withdrawal } => {
                if self.config_min_withdrawal.is_some() {
                    return false; // config already exists
                }
                self.config_min_withdrawal = Some(min_withdrawal);
                true
            }
            Action::UpdateConfig { new_min_withdrawal } => {
                if self.config_min_withdrawal.is_none() {
                    return false;
                }
                self.config_min_withdrawal = Some(new_min_withdrawal);
                true
            }
            Action::Deposit { user, value } => {
                if self.config_min_withdrawal.is_none() {
                    return false;
                }
                match self.balances.get_mut(&user) {
                    Some(b) => {
                        *b += value;
                        true
                    }
                    None => {
                        if value < self.min_create_balance {
                            return false; // new user must meet the creation minimum
                        }
                        self.balances.insert(user, value);
                        true
                    }
                }
            }
            Action::Transfer { src, dst, amount } => {
                if src == dst {
                    return false;
                }
                let src_bal = match self.balances.get(&src) {
                    Some(b) => *b,
                    None => return false, // source must be a live user
                };
                if src_bal < amount || !self.balances.contains_key(&dst) {
                    return false; // insufficient, or dest not live (plain transfer)
                }
                *self.balances.get_mut(&src).unwrap() -= amount;
                *self.balances.get_mut(&dst).unwrap() += amount;
                true
            }
            Action::TransferCreate { src, dst, amount } => {
                if src == dst {
                    return false;
                }
                let src_bal = match self.balances.get(&src) {
                    Some(b) => *b,
                    None => return false,
                };
                if src_bal < amount {
                    return false;
                }
                if !self.balances.contains_key(&dst) && amount < self.min_create_balance {
                    return false; // creating dest must meet the creation minimum
                }
                *self.balances.get_mut(&src).unwrap() -= amount;
                *self.balances.entry(dst).or_insert(0) += amount;
                true
            }
            Action::Withdraw { user, amount } => {
                let min = match self.config_min_withdrawal {
                    Some(m) => m,
                    None => return false,
                };
                let bal = match self.balances.get(&user) {
                    Some(b) => *b,
                    None => return false,
                };
                if amount < min || bal < amount {
                    return false;
                }
                *self.balances.get_mut(&user).unwrap() -= amount;
                self.exits += 1;
                true
            }
        }
    }
}

/// All user seeds referenced by a scenario (so we can assert absent users read back as `None`).
fn referenced_users(actions: &[Action]) -> BTreeSet<u64> {
    let mut s = BTreeSet::new();
    for a in actions {
        match *a {
            Action::Deposit { user, .. } | Action::Withdraw { user, .. } => {
                s.insert(user);
            }
            Action::Transfer { src, dst, .. } | Action::TransferCreate { src, dst, .. } => {
                s.insert(src);
                s.insert(dst);
            }
            Action::InitConfig { .. } | Action::UpdateConfig { .. } => {}
        }
    }
    s
}

/// Builds the unfunded carrier tx for `action` (genesis owns the config).
fn build_tx(action: &Action) -> L1Transaction {
    let cov = EXAMPLE_DEPOSIT_COVENANT_ID;
    let genesis = RuntimeSigner::genesis();
    match *action {
        Action::InitConfig { min_withdrawal } => init_config_tx(min_withdrawal, cov, &genesis),
        Action::UpdateConfig { new_min_withdrawal } => {
            update_config_tx(new_min_withdrawal, cov, &genesis)
        }
        Action::Deposit { user, value } => deposit_tx(cov, &RuntimeSigner::user(user), value),
        Action::Transfer { src, dst, amount } => {
            transfer_tx(&RuntimeSigner::user(src), &RuntimeSigner::user(dst), amount)
        }
        Action::TransferCreate { src, dst, amount } => {
            transfer_create_tx(&RuntimeSigner::user(src), &RuntimeSigner::user(dst), amount)
        }
        Action::Withdraw { user, amount } => {
            withdraw_tx(&RuntimeSigner::user(user), amount, &[0xAA; 32])
        }
    }
}

type FlowVm = Vm<Backend, RocksDbStore>;

/// Runs `actions` in order through the real runtime via the scheduler (one batch each), then
/// asserts the committed balances match the model, including that rejected actions left no trace.
fn run_scenario(actions: &[Action]) {
    let temp = TempDir::new().unwrap();
    let storage: RocksDbStore = RocksDbStore::open(temp.path());
    let backend = Backend::new(
        &runtime_processor_elf(),
        &batch_processor_elf(),
        &batch_aggregator_elf(),
        ProofType::Succinct,
    );
    let vm = Vm::new(backend, ProvingPipeline::None);
    let mut scheduler: Scheduler<RocksDbStore, FlowVm> = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );

    let mut model = Model::new();
    let mut accepted_count = 0usize;
    let verbose = std::env::var("FLOW_SCENARIO_VERBOSE").is_ok();
    let started = Instant::now();
    for (i, action) in actions.iter().enumerate() {
        let accepted = model.predict(action);
        accepted_count += accepted as usize;
        // Hash varies with the index so each batch has a distinct checkpoint (8-byte LE is plenty).
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
        let meta = ChainBlockMetadata {
            hash: Hash::from_bytes(hash),
            blue_score: i as u64 + 1,
            ..Default::default()
        };
        let batch = scheduler.schedule(meta, vec![build_tx(action).into_scheduler_tx(0)]);
        batch.wait_committed_blocking();
        if verbose {
            eprintln!("[scenario] #{i} {action:?} -> accepted={accepted}");
        }
    }
    let elapsed = started.elapsed();

    // Cross-check every referenced user against the model (absent users must read back as None).
    let users = referenced_users(actions);
    for &user in &users {
        let id = RuntimeSigner::user(user).user_id();
        let data = StateVersion::from_latest_data(&storage, id).data().clone();
        let got = if data.is_empty() { None } else { user_balance(&data) };
        let expected = model.balances.get(&user).copied();
        assert_eq!(got, expected, "user {user}: balance mismatch (model vs runtime)");
    }

    let total_balance: u64 = model.balances.values().sum();
    eprintln!(
        "[scenario] {} actions ({} accepted / {} rejected) over {} users in {:.1?} ({:.1?}/action); \
         live users={}, total L2 balance={}, exits emitted={}",
        actions.len(),
        accepted_count,
        actions.len() - accepted_count,
        users.len(),
        elapsed,
        elapsed / actions.len().max(1) as u32,
        model.balances.len(),
        total_balance,
        model.exits,
    );

    scheduler.shutdown();
}

/// Deterministic LCG (Knuth MMIX constants) so a seed reproduces the exact same scenario.
struct Lcg(u64);
impl Lcg {
    /// Advances the generator and returns the next pseudo-random value.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 16
    }
    /// The next value reduced to `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Generates `n` deterministic actions over `n_users` users: an `InitConfig` first, then a weighted
/// mix of deposits / transfers / transfer-creates / withdraws / config updates. Amounts are chosen
/// to keep plenty of accepted activity (deposits fund users that transfers/withdraws then move),
/// while the boundary rules still reject premature / over-balance / below-min actions, all of which
/// the `Model` predicts and the runtime must match.
fn generate(seed: u64, n: usize, n_users: u64) -> Vec<Action> {
    let mut rng = Lcg(seed);
    let mut actions = Vec::with_capacity(n);
    actions.push(Action::InitConfig { min_withdrawal: 100 });
    for _ in 1..n {
        let kind = rng.below(100);
        let action = match kind {
            0..=37 => {
                Action::Deposit { user: rng.below(n_users), value: 1_000 + rng.below(20_000) }
            }
            38..=62 => {
                let (src, dst) = distinct_pair(&mut rng, n_users);
                Action::Transfer { src, dst, amount: 1 + rng.below(5_000) }
            }
            63..=77 => {
                let (src, dst) = distinct_pair(&mut rng, n_users);
                Action::TransferCreate { src, dst, amount: 1 + rng.below(5_000) }
            }
            78..=96 => Action::Withdraw { user: rng.below(n_users), amount: 1 + rng.below(4_000) },
            _ => Action::UpdateConfig { new_min_withdrawal: 50 + rng.below(500) },
        };
        actions.push(action);
    }
    actions
}

/// Two distinct user indices (a self-transfer would build a payload with duplicate access metadata,
/// which the ABI rejects at decode, not an interesting runtime case to fuzz).
fn distinct_pair(rng: &mut Lcg, n_users: u64) -> (u64, u64) {
    assert!(n_users >= 2, "need at least 2 users for transfers");
    let src = rng.below(n_users);
    let dst = (src + 1 + rng.below(n_users - 1)) % n_users;
    (src, dst)
}

/// Large randomized-but-deterministic stress run. Default 150 actions over 8 users; override with
/// `FLOW_SCENARIO_ACTIONS` / `FLOW_SCENARIO_USERS` / `FLOW_SCENARIO_SEED`. The expected-state model
/// validates every action (accepted or rejected) against the real runtime.
#[test]
fn large_random_scenario() {
    let n = env_usize("FLOW_SCENARIO_ACTIONS", 150);
    let users = env_usize("FLOW_SCENARIO_USERS", 8) as u64;
    let seed = env_usize("FLOW_SCENARIO_SEED", 0xC0FFEE) as u64;
    let actions = generate(seed, n, users);
    run_scenario(&actions);
}

/// Reads env var `key` as a `usize`, falling back to `default` when unset or unparseable.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// A comprehensive deterministic scenario: every action, out-of-order submission, and the boundary
/// rejections (premature transfer, missing config, below-minimum create, over-balance withdraw,
/// below-min withdraw).
#[test]
fn comprehensive_deterministic_scenario() {
    use Action::*;
    let actions = vec![
        // Out-of-order #1: deposit before config exists -> rejected (config absent).
        Deposit { user: 1, value: 5_000 },
        // Out-of-order #2: transfer before any deposit -> rejected (source not live).
        Transfer { src: 1, dst: 2, amount: 100 },
        // Bootstrap config.
        InitConfig { min_withdrawal: 100 },
        // A second Init -> rejected (already exists).
        InitConfig { min_withdrawal: 999 },
        // Now deposits work.
        Deposit { user: 1, value: 5_000 },
        Deposit { user: 2, value: 3_000 },
        // Below creation-minimum deposit to a NEW user -> rejected (no trace for user 9).
        Deposit { user: 9, value: 10 },
        // Transfer to a live dest.
        Transfer { src: 1, dst: 2, amount: 1_000 }, // u1: 4000, u2: 4000
        // Transfer-create a new dest.
        TransferCreate { src: 2, dst: 3, amount: 2_000 }, // u2: 2000, u3: 2000
        // Over-balance transfer -> rejected.
        Transfer { src: 3, dst: 1, amount: 999_999 },
        // Below-create-minimum transfer-create -> rejected (user 8 never created).
        TransferCreate { src: 1, dst: 8, amount: 10 },
        // Withdraws.
        Withdraw { user: 1, amount: 2_000 },   // u1: 2000
        Withdraw { user: 2, amount: 50 },      // rejected: below min_withdrawal (100)
        Withdraw { user: 3, amount: 999_999 }, // rejected: over balance
        // Raise the withdrawal minimum, then a withdraw that the new min rejects.
        UpdateConfig { new_min_withdrawal: 2_500 },
        Withdraw { user: 1, amount: 2_000 }, // rejected: below new min (2500)
        Withdraw { user: 1, amount: 2_000 }, /* accepted again? bal 2000 < 2500 min -> still
                                              * rejected */
        // Top up u3 and withdraw above the new min.
        Deposit { user: 3, value: 1_000 },   // u3: 3000
        Withdraw { user: 3, amount: 2_600 }, // u3: 400
    ];
    run_scenario(&actions);
}
