use rocksdb::ColumnFamilyDescriptor;
use vprogs_state_space::StateSpace;

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
            StateSpace::Metadata => "metas",
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
            ColumnFamilyDescriptor::new(cf_name(&Metadata), C::cf_metas_opts()),
        ]
    }
}
