//! Anchored review comments.
//!
//! [`CommentStore`] holds the selected thread's comments as the daemon
//! reports them (`Comments`, `CommentCreated`, `CommentUpdated`,
//! `CommentDeleted`) and sends the comment commands. The transcript and the
//! Changes drawer both read it. The pure helpers here group snapshots into
//! threads, place them on messages and diff lines, map text selections to
//! `Message::text()` byte ranges, and split the `<review-comments>` block the
//! daemon prepends to delivered prompts. The thread card and the inline
//! composer are shared by both views.

mod anchoring;
mod card;
mod draft;
mod placement;
mod store;
#[cfg(test)]
mod tests;
#[allow(unused_imports)]
pub use self::anchoring::*;
#[allow(unused_imports)]
pub use self::card::*;
#[allow(unused_imports)]
pub use self::draft::*;
#[allow(unused_imports)]
pub use self::placement::*;
#[allow(unused_imports)]
pub use self::store::*;

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Escape, InputEvent, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::{CommentId, MessageId, ThreadId};
use trama_protocol::{
    Actor, AnchorStatus, ClientMessage, CodeAnchor, CommentDelivery, CommentSnapshot, CommentState,
    CommentTarget, DaemonMessage,
};

use crate::app::theme::{self, Palette};
use crate::review::diff::Hunk;
use crate::system::net::{DaemonClient, SendStatus};
