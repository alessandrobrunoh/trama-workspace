//! Share and Join wizards for vNext shares (plan 01 §3–4, 02 §4–6),
//! driven by the protocol-14 Share/Join IPC messages.
//!
//! Each wizard is a pure state machine ([`share_flow`], [`join_flow`]) whose
//! actions return the [`trama_protocol::ClientMessage`] to send, so it is
//! tested without a window; the GPUI views only render and forward clicks,
//! sending through the collab store (`collab::vnext`).

pub(crate) mod blockers;
pub(crate) mod join_flow;
pub(crate) mod join_view;
pub(crate) mod route;
pub(crate) mod share_flow;
pub(crate) mod share_view;
mod widgets;

#[cfg(test)]
mod tests;
