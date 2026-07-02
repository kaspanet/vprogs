/// Events emitted by the L1 bridge for observers.
#[derive(Clone, Debug)]
pub enum L1Event {
    /// Connection to the L1 node established.
    Connected,
    /// Connection to the L1 node lost.
    Disconnected,
    /// The bridge encountered a fatal error and stopped.
    Fatal {
        /// What went wrong.
        reason: String,
    },
}
