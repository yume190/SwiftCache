//! Native desktop integration: menu bar item (macOS status item; system tray elsewhere),
//! the application menu and ⌘, / ⌘Q shortcuts, hide-on-close and Dock reopen.
//!
//! The daemon is independent of this process: closing the window or quitting only ends the UI.
//!
//! Menu bar events, application-menu actions and the Dock reopen are collected into
//! [`PlatformAction`]s; the workspace drains them through [`Platform::poll`].

use std::cell::{Cell, RefCell};
use std::io::Cursor;

use gpui_kit::{Menu as AppMenu, MenuItem as AppMenuItem};
use gpui_kit::*;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::settings::{load_prefs, Locale, Prefs};
use crate::types::{format_bytes, service_label, ServiceStatus, Status};

const ICON: &[u8] = include_bytes!("../assets/tray/tray.png");

/// `cargo run` launches a bare executable, so macOS shows the generic one in the Dock; give it the
/// app icon the bundle declares (`CFBundleIconFile`). A bundled launch keeps the bundle's own icon,
/// and an unreadable icon file leaves the default.
#[cfg(target_os = "macos")]
pub fn set_dev_dock_icon() {
    use objc2::MainThreadMarker;
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSString;

    let bundled = std::env::current_exe().is_ok_and(|exe| exe.ancestors().any(|p| p.extension().is_some_and(|e| e == "app")));
    if bundled {
        return;
    }
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon.png");
    let Some(mtm) = MainThreadMarker::new() else { return };
    let Some(image) = NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(path)) else { return };
    // SAFETY: on the main thread (checked above) with a valid image.
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}

#[cfg(not(target_os = "macos"))]
pub fn set_dev_dock_icon() {}

/// Height of the top row the macOS traffic lights sit in. The titlebar is transparent and the
/// content runs up under it: the sidebar leaves this row free for the lights, the main column
/// starts its header in it. Elsewhere the native titlebar stays, so there is no inset.
pub const TITLEBAR_INSET: f32 = if cfg!(target_os = "macos") { 46. } else { 0. };

/// macOS: no visible titlebar or title; traffic lights inset into the sidebar's top row.
/// The app owns dragging ([`window_drag`]) so clicks near the top edge are not delayed by
/// AppKit's titlebar double-click detection.
pub fn titlebar_options() -> (Option<TitlebarOptions>, bool) {
    let title = Some(SharedString::from("SwiftCache"));
    if cfg!(target_os = "macos") {
        (Some(TitlebarOptions { title, appears_transparent: true, traffic_light_position: Some(point(px(18.), px(16.))) }), true)
    } else {
        (Some(TitlebarOptions { title, appears_transparent: false, traffic_light_position: None }), false)
    }
}

/// Makes a non-interactive area move the window (double-click zooms, like a titlebar).
/// Never put it on an element containing controls: their mouse-down would start a move too.
pub fn window_drag<E: InteractiveElement>(element: E) -> E {
    if !cfg!(target_os = "macos") {
        return element;
    }
    element.on_mouse_down(MouseButton::Left, |event: &MouseDownEvent, window, _| {
        if event.click_count >= 2 { window.titlebar_double_click() } else { window.start_window_move() }
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlatformAction {
    /// Bring the window to the front.
    Show,
    /// Show the window with the preferences dialog open.
    Preferences,
    /// Quit the UI (services keep running).
    Quit,
    Start(String),
    Stop(String),
}

actions!(swiftcache, [OpenPreferences, QuitApp, HideApp, HideOthers, ShowAll, CloseWindow]);

thread_local! {
    /// Actions raised by native hooks outside the workspace (application menu, shortcuts).
    /// Both producers and `Platform::poll` run on the main thread.
    static HOOK_QUEUE: RefCell<Vec<PlatformAction>> = const { RefCell::new(Vec::new()) };
}

fn raise(action: PlatformAction) {
    HOOK_QUEUE.with_borrow_mut(|queue| queue.push(action));
}

/// Brings the app and `window` forward, un-hiding the app first. For use from inside a window update.
pub fn show_window(window: &mut Window, cx: &mut App) {
    cx.activate(true);
    window.activate_window();
}

/// Same as [`show_window`] from a context that is not inside that window's update.
fn show_handle(handle: AnyWindowHandle, cx: &mut App) {
    cx.activate(true);
    // Fails only while the window is already being updated, in which case the caller is
    // in that window and activation above is all that is needed.
    let _ = handle.update(cx, |_, window, _| window.activate_window());
}

/// Closing the window hides the app on macOS (menu bar app); the status item and the daemon stay.
pub fn configure_window(window: &mut Window, cx: &mut App) {
    window.on_window_should_close(cx, |_, cx| {
        if cfg!(target_os = "macos") {
            cx.hide();
            false
        } else {
            true
        }
    });
}

/// Registers shortcuts and application-menu actions; the application builder owns Dock reopen.
pub fn install_app_hooks(cx: &mut App, window: AnyWindowHandle) {
    cx.bind_keys([
        KeyBinding::new(if cfg!(target_os = "macos") { "cmd-," } else { "ctrl-," }, OpenPreferences, None),
        KeyBinding::new(if cfg!(target_os = "macos") { "cmd-q" } else { "ctrl-q" }, QuitApp, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-h", HideApp, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-w", CloseWindow, None),
    ]);
    cx.on_action(move |_: &OpenPreferences, cx: &mut App| {
        raise(PlatformAction::Preferences);
        show_handle(window, cx);
    });
    cx.on_action(|_: &QuitApp, cx: &mut App| cx.quit());
    cx.on_action(|_: &HideApp, cx: &mut App| cx.hide());
    cx.on_action(|_: &HideOthers, cx: &mut App| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx: &mut App| cx.unhide_other_apps());
    cx.on_action(|_: &CloseWindow, cx: &mut App| cx.hide());
    set_app_menus(cx, load_prefs().locale);
}

thread_local! {
    static MENU_LOCALE: Cell<Option<Locale>> = const { Cell::new(None) };
}

/// (Re)installs the application menu when the locale changed since the last call.
pub fn set_app_menus(cx: &mut App, locale: Locale) {
    if MENU_LOCALE.with(|l| l.replace(Some(locale))) == Some(locale) {
        return;
    }
    let l = |zh: &'static str, en: &'static str| locale.pick(zh, en);
    cx.set_menus([
        AppMenu::new("SwiftCache").items([
            AppMenuItem::action(l("設定…", "Settings…"), OpenPreferences),
            AppMenuItem::separator(),
            AppMenuItem::os_submenu(l("服務", "Services"), SystemMenuType::Services),
            AppMenuItem::separator(),
            AppMenuItem::action(l("隱藏 SwiftCache", "Hide SwiftCache"), HideApp),
            AppMenuItem::action(l("隱藏其他", "Hide Others"), HideOthers),
            AppMenuItem::action(l("全部顯示", "Show All"), ShowAll),
            AppMenuItem::separator(),
            AppMenuItem::action(l("結束 SwiftCache", "Quit SwiftCache"), QuitApp),
        ]),
        AppMenu::new(l("視窗", "Window")).items([AppMenuItem::action(l("關閉視窗", "Close Window"), CloseWindow)]),
    ]);
}

struct ServiceMenu {
    id: String,
    running: bool,
    submenu: Submenu,
    toggle: tray_icon::menu::MenuItem,
    /// Present only while the service reports an error.
    error: Option<tray_icon::menu::MenuItem>,
}

/// Fixed items of the status menu.
struct Items {
    status: tray_icon::menu::MenuItem,
    usage: tray_icon::menu::MenuItem,
    traffic: tray_icon::menu::MenuItem,
    health: tray_icon::menu::MenuItem,
    open: tray_icon::menu::MenuItem,
    settings: tray_icon::menu::MenuItem,
    quit: tray_icon::menu::MenuItem,
    services: Vec<ServiceMenu>,
}

enum Snapshot {
    Online { status: Status, local_store_bytes: u64 },
    Offline(String),
}

pub struct Platform {
    tray: Option<TrayIcon>,
    items: Option<Items>,
    error: Option<String>,
    last: Option<Snapshot>,
}

fn decode_icon() -> Result<Icon, String> {
    let mut reader = png::Decoder::new(Cursor::new(ICON)).read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err("tray icon must be 8-bit RGBA".into());
    }
    buf.truncate(info.buffer_size());
    Icon::from_rgba(buf, info.width, info.height).map_err(|e| e.to_string())
}

fn item(id: &str, enabled: bool) -> MenuItem {
    MenuItem::with_id(id, "…", enabled, None)
}

fn put(item: &MenuItem, text: &str) {
    if item.text() != text {
        item.set_text(text);
    }
}

impl Platform {
    /// Creates the menu bar item. Must run on the main thread inside the running app.
    /// A failure is kept in [`Platform::error`]; the rest of the app still works.
    pub fn new() -> Self {
        let mut platform = Platform { tray: None, items: None, error: None, last: None };
        match Self::build() {
            Ok((tray, items)) => {
                platform.tray = Some(tray);
                platform.items = Some(items);
            }
            Err(e) => platform.error = Some(e),
        }
        platform.render(&load_prefs());
        platform
    }

    fn build() -> Result<(TrayIcon, Items), String> {
        let items = Items {
            status: item("status", false),
            usage: item("usage", false),
            traffic: item("traffic", false),
            health: item("health", false),
            open: item("open", true),
            settings: item("settings", true),
            quit: item("quit", true),
            services: Vec::new(),
        };
        let menu = Self::menu(&items).map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_id("swiftcache")
            .with_icon(decode_icon()?)
            .with_icon_as_template(true)
            .with_tooltip("SwiftCache")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .build()
            .map_err(|e| e.to_string())?;
        Ok((tray, items))
    }

    /// The whole menu around the current service submenus.
    fn menu(items: &Items) -> tray_icon::menu::Result<Menu> {
        let menu = Menu::new();
        menu.append(&items.status)?;
        menu.append(&items.usage)?;
        menu.append(&items.traffic)?;
        menu.append(&items.health)?;
        if !items.services.is_empty() {
            menu.append(&PredefinedMenuItem::separator())?;
            for s in &items.services {
                menu.append(&s.submenu)?;
            }
        }
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&items.open)?;
        menu.append(&items.settings)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&items.quit)?;
        Ok(menu)
    }

    /// Why the menu bar item is unavailable, if it could not be created.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Native events since the last call.
    pub fn poll(&mut self) -> Vec<PlatformAction> {
        let mut actions = HOOK_QUEUE.with_borrow_mut(std::mem::take);
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let id: &str = event.id().as_ref();
            match id {
                "open" => actions.push(PlatformAction::Show),
                "settings" => actions.push(PlatformAction::Preferences),
                "quit" => actions.push(PlatformAction::Quit),
                _ => {
                    let Some(service) = id.strip_prefix("toggle:") else { continue };
                    let Some(menu) = self.items.as_ref().and_then(|i| i.services.iter().find(|s| s.id == service)) else { continue };
                    // Locked until the next refresh reports the new state.
                    menu.toggle.set_enabled(false);
                    actions.push(if menu.running { PlatformAction::Stop(service.to_owned()) } else { PlatformAction::Start(service.to_owned()) });
                }
            }
        }
        actions
    }

    /// Shows the daemon's current state. `local_store_bytes` is the summed size of the distinct
    /// local stores of the running services (Serve and Proxy alike).
    pub fn refresh(&mut self, status: &Status, local_store_bytes: u64, prefs: &Prefs) {
        self.last = Some(Snapshot::Online { status: status.clone(), local_store_bytes });
        self.render(prefs);
    }

    /// The daemon cannot be reached: title `!`, message in the menu, service toggles disabled.
    pub fn offline(&mut self, error: &str, prefs: &Prefs) {
        self.last = Some(Snapshot::Offline(error.to_owned()));
        self.render(prefs);
    }

    /// Re-renders the last known state, e.g. after the language or title preference changed.
    pub fn relabel(&mut self, prefs: &Prefs) {
        self.render(prefs);
    }

    fn render(&mut self, prefs: &Prefs) {
        let (Some(tray), Some(items)) = (&self.tray, &mut self.items) else { return };
        let l = prefs.locale;
        put(&items.open, l.pick("開啟 SwiftCache…", "Open SwiftCache…"));
        put(&items.settings, l.pick("設定…", "Settings…"));
        put(&items.quit, l.pick("結束介面（服務繼續執行）", "Quit window (services keep running)"));
        let set_title = |title: &str| tray.set_title(prefs.menu_bar_title.then_some(title));

        let (status, bytes) = match &self.last {
            None => {
                put(&items.status, l.pick("連線中…", "Connecting…"));
                set_title("…");
                return;
            }
            Some(Snapshot::Offline(err)) => {
                set_title("!");
                put(&items.status, &format!("{}{err}", l.pick("無法連線管理程序：", "Cannot reach the daemon: ")));
                for s in &items.services {
                    s.toggle.set_enabled(false);
                }
                return;
            }
            Some(Snapshot::Online { status, local_store_bytes }) => (status, *local_store_bytes),
        };

        let total = status.services.len();
        let running_count = status.services.iter().filter(|s| s.running).count();
        put(&items.status, &if running_count == 0 {
            l.pick("○ 服務已停止", "○ All services stopped").to_owned()
        } else {
            match l {
                Locale::ZhTw => format!("● {running_count} 個服務執行中 · 共 {total} 個"),
                Locale::En => format!("● {running_count} of {total} services running"),
            }
        });

        // Service submenus: rebuild the tray menu only when the ordered id list changes.
        if items.services.iter().map(|s| s.id.as_str()).ne(status.services.iter().map(|s| s.id.as_str())) {
            let mut fresh = Vec::with_capacity(total);
            for svc in &status.services {
                let toggle = MenuItem::with_id(format!("toggle:{}", svc.id), "…", true, None);
                let submenu = Submenu::with_id(format!("service:{}", svc.id), "…", true);
                if submenu.append(&toggle).is_err() {
                    return;
                }
                fresh.push(ServiceMenu { id: svc.id.clone(), running: false, submenu, toggle, error: None });
            }
            let previous = std::mem::replace(&mut items.services, fresh);
            match Self::menu(items) {
                Ok(menu) => tray.set_menu(Some(Box::new(menu))),
                Err(_) => {
                    items.services = previous;
                    return;
                }
            }
        }
        for (menu, svc) in items.services.iter_mut().zip(&status.services) {
            Self::render_service(menu, svc, l);
        }

        let n = &status.summary;
        let (up, down) = (format_bytes(n.up_bytes), format_bytes(n.down_bytes));
        put(&items.traffic, &match l {
            Locale::ZhTw => format!("上傳 {up} · 下載 {down} · {} 筆", n.transfers),
            Locale::En => format!("Up {up} · Down {down} · {} transfers", n.transfers),
        });
        let rate = if n.kv_hit + n.kv_miss == 0 { "—".to_owned() } else { format!("{:.1}%", n.kv_hit as f64 * 100.0 / (n.kv_hit + n.kv_miss) as f64) };
        put(&items.health, &match l {
            Locale::ZhTw => format!("KV 命中率 {rate} · 錯誤 {}", n.errors),
            Locale::En => format!("KV hit rate {rate} · {} errors", n.errors),
        });

        if running_count == 0 {
            put(&items.usage, l.pick("快取用量：—", "Cache usage: —"));
            set_title(l.pick("停止", "Stopped"));
        } else {
            let size = format_bytes(bytes);
            put(&items.usage, &format!("{}{size}", l.pick("快取用量：", "Cache usage: ")));
            set_title(&size);
        }
    }

    fn render_service(menu: &mut ServiceMenu, svc: &ServiceStatus, l: Locale) {
        let dot = if svc.running { "●" } else { "○" };
        let label = format!("{dot} {}", service_label(&svc.config));
        if menu.submenu.text() != label {
            menu.submenu.set_text(&label);
        }
        put(&menu.toggle, if svc.running { l.pick("停止服務", "Stop service") } else { l.pick("啟動服務", "Start service") });
        menu.toggle.set_enabled(true);
        menu.running = svc.running;
        let text = svc.error.as_deref().filter(|e| !e.is_empty()).map(|e| format!("{}{e}", l.pick("錯誤：", "Error: ")));
        match (text, menu.error.take()) {
            (Some(text), Some(item)) => {
                put(&item, &text);
                menu.error = Some(item);
            }
            (Some(text), None) => {
                let item = MenuItem::with_id(format!("error:{}", svc.id), &text, false, None);
                if menu.submenu.append(&item).is_ok() {
                    menu.error = Some(item);
                }
            }
            (None, Some(item)) => {
                let _ = menu.submenu.remove(&item);
            }
            (None, None) => {}
        }
    }
}
