//! Native directory prompt and the isolated-checkout confirmation.
//!
//! The picker asks the platform for one existing directory. The path is returned
//! as a [`PathBuf`] and is never passed to a shell.

use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::WindowExt as _;
use gpui_kit::{App, PathPromptOptions, SharedString, Window};

pub const MIGRATION_TITLE: &str = "Prepare isolated checkout";

pub const MIGRATION_BODY: &str = "The transcript is preserved. Your original files are left untouched. Trama opens a new Claude session on an isolated checkout. This does not rewind agent memory. After it finishes, send the prompt again yourself. Nothing is sent automatically.";

pub struct MigrationConfirmCopy {
    pub title: &'static str,
    pub body: &'static str,
    pub confirm: &'static str,
    pub cancel: &'static str,
}

pub fn migration_confirm_copy() -> MigrationConfirmCopy {
    MigrationConfirmCopy {
        title: MIGRATION_TITLE,
        body: MIGRATION_BODY,
        confirm: MIGRATION_TITLE,
        cancel: "Cancel",
    }
}

/// Platform prompt that selects a single directory, not a file and not a command.
pub fn directory_prompt_options() -> PathPromptOptions {
    PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(SharedString::from(
            "Choose an existing local Git repository",
        )),
    }
}

/// The one directory the user picked. Cancel, an empty result, and a multi-select
/// are all `None` so the caller does not guess or execute anything.
pub fn picked_directory(paths: Option<Vec<PathBuf>>) -> Option<PathBuf> {
    let mut paths = paths?;
    if paths.len() != 1 {
        return None;
    }
    paths.pop()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    /// `None` is a cancel. `Some` is the directory root the platform returned.
    Picked(Option<PathBuf>),
    Failed(String),
    Cancelled,
}

pub fn prompt_for_repository_directory(cx: &App) -> impl Future<Output = PickerOutcome> + 'static {
    let receiver = cx.prompt_for_paths(directory_prompt_options());
    async move {
        match receiver.await {
            Ok(Ok(paths)) => PickerOutcome::Picked(picked_directory(paths)),
            Ok(Err(err)) => PickerOutcome::Failed(format!(
                "The folder picker could not be opened ({err}). Paste the existing local repository path instead."
            )),
            Err(_) => PickerOutcome::Cancelled,
        }
    }
}

pub fn open_migration_confirm(
    window: &mut Window,
    cx: &mut App,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
) {
    let copy = migration_confirm_copy();
    let on_confirm = Rc::new(on_confirm);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_confirm = Rc::clone(&on_confirm);
        alert
            .confirm()
            .title(copy.title)
            .description(copy.body)
            .ok_text(copy.confirm)
            .cancel_text(copy.cancel)
            .on_ok(move |_event, window, cx| {
                on_confirm(window, cx);
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_prompt_selects_one_existing_directory() {
        let options = directory_prompt_options();
        assert!(options.directories);
        assert!(!options.files);
        assert!(!options.multiple);
        let prompt = options.prompt.unwrap();
        assert!(prompt.contains("existing local"));
    }

    #[test]
    fn picked_directory_returns_only_a_single_path() {
        assert_eq!(picked_directory(None), None);
        assert_eq!(picked_directory(Some(Vec::new())), None);
        let root = PathBuf::from("/tmp/repo");
        assert_eq!(
            picked_directory(Some(vec![root.clone()])),
            Some(root.clone())
        );
        assert_eq!(
            picked_directory(Some(vec![root, PathBuf::from("/tmp/other")])),
            None
        );
    }

    #[test]
    fn migration_copy_does_not_promise_a_memory_rewind() {
        let copy = migration_confirm_copy();
        assert_eq!(copy.title, "Prepare isolated checkout");
        assert!(copy.body.contains("transcript is preserved"));
        assert!(copy.body.contains("original files are left untouched"));
        assert!(copy.body.contains("new Claude session"));
        assert!(copy.body.contains("does not rewind agent memory"));
        assert!(copy.body.contains("Nothing is sent automatically"));
    }
}
