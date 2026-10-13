//! The Share/Join flow behind the protocol-14 IPC messages: preflight,
//! create, join (MLS key package → Welcome), snapshots, leases, and the
//! mapping of received events into local history.

mod access;
mod content;
mod flow;
mod inbound;
mod leases;
mod members;
mod objects;
mod outbound;
mod outgoing;
mod preflight;
mod retry;
mod scan;
mod slices;
mod snapshot;
#[cfg(test)]
mod tests;
