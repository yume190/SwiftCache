mod smoke;
mod api;
mod app;
mod cache;
mod platform;
mod records;
mod service;
mod settings;
mod types;
mod ui;

use gpui_kit::*;
use app::Workspace;

fn main() {
    let application = application().with_assets(assets::Assets);
    application.on_reopen(|cx| {
            cx.activate(true);
            if let Some(handle) = cx.windows().into_iter().next() {
                let _ = handle.update(cx, |_, window, cx| platform::show_window(window, cx));
            }
        });
    application.run(|cx| {
            init(cx);
            platform::set_dev_dock_icon();
            cx.bind_keys([
                KeyBinding::new(if cfg!(target_os = "macos") { "cmd-s" } else { "ctrl-s" }, app::SaveSettings, Some("SwiftCache")),
                KeyBinding::new(if cfg!(target_os = "macos") { "cmd-r" } else { "ctrl-r" }, app::Refresh, Some("SwiftCache")),
            ]);
            let bounds = Bounds::centered(None, size(px(1600.), px(1000.)), cx);
            let (titlebar, app_owns_titlebar_drag) = platform::titlebar_options();
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(1000.), px(720.))),
                titlebar,
                app_owns_titlebar_drag,
                ..Default::default()
            };
            let (window, workspace) = open_window(options, cx, |window, cx| {
                platform::configure_window(window, cx);
                cx.new(|cx| Workspace::new(window, cx))
            }).expect("open SwiftCache GPUI window");
            platform::install_app_hooks(cx, window);
            workspace.update(cx, |view, cx| view.start(cx));
            cx.activate(true);
            smoke::start(window, workspace.clone(), cx);
            println!("SWIFTCACHE_GPUI_READY pid={}", std::process::id());
        });
}
