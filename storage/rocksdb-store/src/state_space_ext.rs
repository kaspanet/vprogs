use rocksdb::ColumnFamilyDescriptor;
use vprogs_storage_types::StateSpace;

use crate::config::{Config, DefaultConfig};

pub trait StateSpaceExt<C: Config = DefaultConfig> {
    fn cf_name(&self) -> &'static str;
    fn all_descriptors() -> Vec<ColumnFamilyDescriptor>;
}

impl<C: Config> StateSpaceExt<C> for StateSpace {
    fn cf_name(&self) -> &'static str {
        match self {
            StateSpace::StateVersion => "data",
            StateSpace::StatePtrLatest => "latest_ptr",
            StateSpace::StatePtrRollback => "rollback_ptr",
            StateSpace::BatchMetadata => "batch_metadata",
            StateSpace::LaneTip => "lane_tip",
            StateSpace::Metadata => "metas",
            StateSpace::SmtNode => "smt_node",
            StateSpace::SmtStale => "smt_stale",
        }
    }

    fn all_descriptors() -> Vec<ColumnFamilyDescriptor> {
        use StateSpace::*;
        let cf_name = <StateSpace as StateSpaceExt<C>>::cf_name;
        vec![
            ColumnFamilyDescriptor::new(cf_name(&StateVersion), C::cf_data_opts()),
            ColumnFamilyDescriptor::new(cf_name(&StatePtrLatest), C::cf_latest_ptr_opts()),
            ColumnFamilyDescriptor::new(cf_name(&StatePtrRollback), C::cf_rollback_ptr_opts()),
            ColumnFamilyDescriptor::new(cf_name(&BatchMetadata), C::cf_batch_metadata_opts()),
            ColumnFamilyDescriptor::new(cf_name(&LaneTip), C::cf_lane_tip_opts()),
            ColumnFamilyDescriptor::new(cf_name(&Metadata), C::cf_metas_opts()),
            ColumnFamilyDescriptor::new(cf_name(&SmtNode), C::cf_smt_node_opts()),
            ColumnFamilyDescriptor::new(cf_name(&SmtStale), C::cf_smt_stale_opts()),
        ]
    }
}
