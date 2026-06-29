use rocksdb::{Options, SliceTransform, WriteOptions};
use tap::Tap;
use vprogs_core_types::{CanonicalChain, NoOpCanonicalChain};

/// Prefix length for keys that start with a u64 (batch_index, version, or a proof-receipt
/// checkpoint_index).
const U64_PREFIX_LEN: usize = size_of::<u64>();

/// RocksDB tuning and per-column-family options for the store.
pub trait Config: Send + Sync + 'static {
    /// Canonical chain for fork-aware SMT reads; `NoOpCanonicalChain` disables filtering.
    type CanonicalChain: CanonicalChain + Default;

    fn db_opts() -> Options {
        Options::default().tap_mut(|o| {
            // --- Parallelism & background work -----------------------------------
            // Compactions/flushes can still run in parallel even with a single write thread.
            o.increase_parallelism(num_cpus::get() as i32); // scale background threads to CPU
            o.set_max_background_jobs(8); // compaction + flush threads
            o.set_max_subcompactions(2); // bigger L0->L1 compactions benefit

            // --- Write path semantics --------------------------------------------
            // One writer worker: pipelined writes only help contending writers, so off.
            o.set_enable_pipelined_write(false);

            // Unordered writes only help many writers and complicate cross-CF snapshots, so off.
            o.set_unordered_write(false);

            // A no-op with one writer; left on so scaling to >1 writer needs no reopen.
            o.set_allow_concurrent_memtable_write(true);

            // --- I/O smoothing ----------------------------------------------------
            // Throttle background I/O to avoid bursty stalls; 1 MiB is conservative for NVMe.
            o.set_bytes_per_sync(1 << 20); // fsync data file every ~1 MiB written
            o.set_wal_bytes_per_sync(1 << 20); // fdatasync WAL every ~1 MiB appended

            // --- Compaction policy ------------------------------------------------
            // Let RocksDB auto-size levels based on data volume to reduce write amp.
            o.set_level_compaction_dynamic_level_bytes(true);

            // --- Safety/robustness ------------------------------------------------
            o.set_paranoid_checks(true); // verify checksums, fail fast on corruption
        })
    }

    fn write_opts() -> WriteOptions {
        WriteOptions::default().tap_mut(|o| {
            o.set_sync(false); // no fsync on each write (group commit FTW)
            o.disable_wal(false); // keep WAL (crash replay is our durability)
        })
    }

    fn cf_data_opts() -> Options {
        Options::default().tap_mut(|o| {
            // Data keys are: version (u64 big-endian) || resource_id
            // Enable prefix iteration by version.
            o.set_prefix_extractor(SliceTransform::create_fixed_prefix(U64_PREFIX_LEN));
        })
    }

    fn cf_latest_ptr_opts() -> Options {
        Options::default()
    }

    fn cf_rollback_ptr_opts() -> Options {
        Options::default().tap_mut(|o| {
            // RollbackPtr keys are: batch_index (u64 big-endian) || resource_id
            // Enable prefix iteration by batch_index for reorg rollback.
            o.set_prefix_extractor(SliceTransform::create_fixed_prefix(U64_PREFIX_LEN));
        })
    }

    fn cf_batch_metadata_opts() -> Options {
        Options::default()
    }

    fn cf_metas_opts() -> Options {
        Options::default()
    }

    fn cf_smt_node_opts() -> Options {
        Options::default().tap_mut(|o| {
            // SmtNode keys are: path(32) || level(2 BE) || !version(8 BE)
            // 34-byte prefix groups all versions of the same node for prefix iteration.
            o.set_prefix_extractor(SliceTransform::create_fixed_prefix(34));
        })
    }

    fn cf_smt_stale_opts() -> Options {
        Options::default().tap_mut(|o| {
            // SmtStale keys are: stale_since_version(8 BE) || path(32) || level(2 BE)
            // 8-byte prefix groups all stale markers for the same version.
            o.set_prefix_extractor(SliceTransform::create_fixed_prefix(8));
        })
    }

    fn cf_proof_receipt_opts() -> Options {
        Options::default().tap_mut(|o| {
            // ProofReceipt keys all start with the checkpoint_index (a u64), grouping every
            // program's receipts for one checkpoint -- across image ids and block hashes alike --
            // so the reorg orchestrator can prune a reverted checkpoint with a single prefix scan.
            o.set_prefix_extractor(SliceTransform::create_fixed_prefix(U64_PREFIX_LEN));
        })
    }
}

/// Default store configuration, with fork-awareness disabled.
pub struct DefaultConfig;
impl Config for DefaultConfig {
    type CanonicalChain = NoOpCanonicalChain;
}
