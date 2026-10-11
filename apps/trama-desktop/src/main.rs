mod agent;
mod chat;
mod composer;
mod net;
mod workspace;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use workspace::Workspace;

fn main() {
    application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);

            let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(800.), px(560.))),
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
