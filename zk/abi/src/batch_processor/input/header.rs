/// Decoded batch processor input header.
pub struct Header<'a> {
    pub image_id: &'a [u8; 32],
    pub batch_index: u64,
    pub prev_root: &'a [u8; 32],
    pub n_resources: u32,
    pub n_txs: u32,
}
