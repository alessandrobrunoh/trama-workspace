//! Tool call views in the transcript, drawn as document lines.
//!
//! Runs of thinking and tool calls fold into one muted summary line
//! ("Thought 3 times, ran 2 commands, read 3 files ›") that opens to a
//! line per step ("Thinking: <first line> ›", "Ran <command> ›", "Read
//! <path> ›"); a step opens to the thought, the command and the tail of
//! its output ("Show N more lines"), or the call's input and output. File
//! edits (Edit, MultiEdit, Write, NotebookEdit) are "Apply patch <path> ⌄"
//! lines over the diff built from the tool input, whose lines can be picked
//! and commented on. Trama subagent calls are "Agent spawned: <title>"
//! lines that open the subagent next to the transcript. All their text is
//! selectable and commentable ([`crate::review::doc_text`]). The pure helpers come
//! first and are tested; the renderers below them only lay out what they
//! compute.

mod ansi;
mod classify;
mod diff;
mod patch;
mod render;
#[cfg(test)]
mod tests;
#[allow(unused_imports)]
pub use self::ansi::*;
#[allow(unused_imports)]
pub use self::classify::*;
#[allow(unused_imports)]
pub use self::diff::*;
#[allow(unused_imports)]
pub use self::patch::*;
#[allow(unused_imports)]
pub use self::render::*;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, spinner::Spinner, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;
use trama_domain::ids::ThreadId;

use crate::app::theme::{self, Palette};
use crate::conversation::chat::{ChatEntry, ChatPanel};
use crate::review::comments;
use crate::review::diff::{DiffLine, Hunk, LineKind};
use crate::review::doc_text::{BlockTexts, Mark, code_block, escape_markdown};
