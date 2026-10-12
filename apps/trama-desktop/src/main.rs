mod agent;
mod changes;
mod chat;
mod comments;
mod composer;
mod copies;
mod devhooks;
mod dialog;
mod diff;
mod editor;
mod keymap;
mod net;
mod onboarding;
mod projects;
mod sidebar;
mod startup;
mod subthreads;
mod theme;
mod workspace;

use std::borrow::Cow;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use workspace::Workspace;

// Icons beyond GPUI Kit's default bundle.
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        CircleCheckBig,
        Command,
        FileDiff,
        FolderGit2,
        GitBranch,
        GitMerge,
        KeyRound,
        Keyboard,
        ListChecks,
        MessageSquare,
        MessageSquareDiff,
        MessageSquarePlus,
        MessageSquareText,
        Pencil,
        PlaneLanding,
        ShieldAlert,
        ShieldCheck,
        Split,
        SquarePen,
        UserCheck,
        Wrench,
    ]
);

/// The Kit's default icons plus [`ExtraIcons`].
struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut items = gpui_kit::assets::Assets.list(path)?;
        items.extend(ExtraIcons.list(path)?);
        Ok(items)
    }
}

fn main() {
    application().with_assets(AppAssets).run(|cx| {
        gpui_kit::init(cx);
        keymap::bind(cx);
        let mut mode = theme::mode_for(cx.window_appearance());
        if let Some(forced) = devhooks::appearance() {
            mode = forced;
        }
        theme::apply(mode, cx);

        let bounds = Bounds::centered(None, size(px(1320.), px(840.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(860.), px(560.))),
            kind: WindowKind::Normal,
            ..TitleBar::window_options()
        };

        if let Err(err) = gpui_kit::open_window(options, cx, |window, cx| {
            window.activate_window();
            cx.new(|cx| Workspace::new(window, cx))
        }) {
            eprintln!("failed to open TramaWorkspace window: {err}");
        }
    });
}
