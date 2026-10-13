//! The Share/Join flow behind the protocol-14 IPC messages: preflight,
//! create, join (MLS key package → Welcome), snapshots, leases, and the
//! mapping of received events into local history.

mod flow;
mod inbound;
mod leases;
mod outbound;
mod preflight;
mod scan;
mod snapshot;
#[cfg(test)]
mod tests;
