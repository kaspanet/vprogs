use rocksdb::{Options, SliceTransform, WriteOptions};
use tap::Tap;

/// Prefix length for keys that start with a u64 (batch_index or version).
const U64_PREFIX_LEN: usize = size_of::<u64>();

pub trait Config: Send + Sync + 'static {
    fn db_opts() -> Options {
        Options::default().tap_mut(|o| {
            // --- Parallelism & background work -----------------------------------
            // Compactions/flushes can still run in parallel even with a single write thread.
            o.increase_parallelism(num_cpus::get() as i32); // scale background threads to CPU
            o.set_max_background_jobs(8); // compaction + flush threads
            o.set_max_subcompactions(2); // bigger L0->L1 compactions benefit

            // --- Write path semantics --------------------------------------------
            // We have exactly ONE writer worker that issues large WriteBatches.
            // Pipelined writes help when multiple writers contend (WAL vs memtable).
            // With a single writer they add overhead but no benefit—turn them off.
            o.set_enable_pipelined_write(false);

            // Unordered writes relax memtable insert order (WAL order still serialized).
            // That’s great for many concurrent writers, but unnecessary here and can
            // complicate iterator/snapshot semantics across CFs—so keep it off.
            o.set_unordered_write(false);

            // Allow concurrent memtable writes is a no-op with one writer, but harmless.
            // Leave it on so scaling to >1 writer later won’t require a RocksDB reopen.
            o.set_allow_concurrent_memtable_write(true);

            // --- I/O smoothing ----------------------------------------------------
            // Throttle background I/O to avoid bursty stalls under heavy load.
            // 1 MiB is a good, conservative starting point for NVMe.
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
        Options::default().tap_mut(|o| {
            // BatchMetadata keys are: batch_index (u64 big-endian) || field_name
            // Enable prefix iteration by batch_index.
            o.set_prefix_extractor(SliceTransform::create_fixed_prefix(U64_PREFIX_LEN));
        })
    }

    fn cf_metas_opts() -> Options {
        Options::default()
    }
}

pub struct DefaultConfig;
impl Config for DefaultConfig {}
