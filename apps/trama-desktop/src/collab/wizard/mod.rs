//! Share and Join wizards for vNext shares (plan 01 §3–4, 02 §4–6).
//!
//! Each wizard is a pure state machine ([`share_flow`], [`join_flow`]) that
//! emits [`model::WizardCommand`]s through a [`model::WizardBackend`]; the
//! GPUI views only render state and forward clicks. The model types are a
//! local stand-in for the daemon IPC messages and map 1:1 onto them.

// Not wired to the workspace until the daemon IPC messages land.
#![allow(dead_code)]

pub(crate) mod join_flow;
pub(crate) mod join_view;
pub(crate) mod model;
pub(crate) mod share_flow;
pub(crate) mod share_view;
mod widgets;

#[cfg(test)]
mod tests;
