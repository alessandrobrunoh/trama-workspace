//! Share and Join wizards for vNext shares (plan 01 §3–4, 02 §4–6),
//! driven by the protocol-14 Share/Join IPC messages.
//!
//! Each wizard is a pure state machine ([`share_flow`], [`join_flow`]) whose
//! actions return the [`trama_protocol::ClientMessage`] to send, so it is
//! tested without a window; the GPUI views only render and forward clicks,
//! emitting [`WizardRequest`] for the owner to dispatch.

// Views are not yet mounted in the workspace (see docs/plan/README.md).
#![allow(dead_code)]

pub(crate) mod blockers;
pub(crate) mod join_flow;
pub(crate) mod join_view;
pub(crate) mod share_flow;
pub(crate) mod share_view;
mod widgets;

#[cfg(test)]
mod tests;

/// A message a wizard wants sent to the daemon.
#[derive(Debug, Clone)]
pub struct WizardRequest(pub trama_protocol::ClientMessage);
