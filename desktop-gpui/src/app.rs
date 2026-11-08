use crate::{
    api::ApiClient,
    cache::CacheState,
    platform::{Platform, PlatformAction},
    records::RecordsState,
    service::ServiceState,
    settings::SettingsState,
    types::*,
};
use gpui_kit::component::{
    ActiveTheme, Disableable, Selectable, WindowExt,
    button::{Button, ButtonCustomVariant, ButtonVariants},
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::Value;
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

actions!(swiftcache_gpui, [SaveSettings, Refresh]);

pub struct Workspace {
    pub api: Arc<ApiClient>,
    pub status: Option<Status>,
    pub selected_id: Option<String>,
    pub active_page: PageKind,
    pub service: ServiceState,
    pub records: RecordsState,
    pub cache: CacheState,
    pub settings: SettingsState,
    pub platform: Platform,
    pub busy: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub generation: u64,
    pub window: AnyWindowHandle,
    pub job: Option<Task<()>>,
    status_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    notice_task: Option<Task<()>>,
    status_loading: bool,
    initialized: bool,
    connected: bool,
    last_tick: Instant,
    last_usage_total: Instant,
    tray_total: u64,
    pending_selection: Option<Option<String>>,
    focus: FocusHandle,
    epoch: u64,
    usage_task: Option<Task<()>>,
    usage_loading: bool,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = SettingsState::new();
        settings.apply(cx);
        let selected_id = crate::api::data_dir()
            .ok()
            .and_then(|p| std::fs::read(p.join("gpui-selected.json")).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            api: Arc::new(ApiClient::new().expect("initialize daemon client")),
            status: None,
            selected_id,
            active_page: PageKind::Service,
            service: ServiceState::new(window, cx),
            records: RecordsState::new(window, cx),
            cache: CacheState::new(),
            settings,
            platform: Platform::new(),
            busy: false,
            error: None,
            notice: None,
            generation: 0,
            window: window.window_handle(),
            job: None,
            status_task: None,
            poll_task: None,
            notice_task: None,
            status_loading: false,
            initialized: false,
            connected: false,
            last_tick: Instant::now(),
            last_usage_total: Instant::now() - Duration::from_secs(30),
            tray_total: 0,
            pending_selection: None,
            focus,
            epoch: 0,
            usage_task: None,
            usage_loading: false,
        }
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        if let Some(error) = self.platform.error() {
            self.error = Some(format!(
                "{}{error}",
                self.t("無法建立選單列圖示：", "Couldn't create the menu bar item: ")
            ));
        }
        self.refresh_status(cx);
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                smol::Timer::after(Duration::from_millis(250)).await;
                if this.update(cx, |view, cx| view.tick(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        for action in self.platform.poll() {
            match action {
                PlatformAction::Show => {
                    self.show_window(cx);
                }
                PlatformAction::Preferences => {
                    self.show_window(cx);
                    let entity = cx.entity();
                    let handle = self.window;
                    cx.defer(move |cx| {
                        let _ = handle.update(cx, |_, window, cx| {
                            window.activate_window();
                            entity.update(cx, |view, cx| view.open_preferences(window, cx));
                        });
                    });
                }
                PlatformAction::Quit => cx.quit(),
                PlatformAction::Start(id) => self.act_service(id, true, cx),
                PlatformAction::Stop(id) => self.act_service(id, false, cx),
            }
        }
        if self.last_tick.elapsed() >= Duration::from_secs(2) {
            self.last_tick = Instant::now();
            if !self.busy {
                self.refresh_status(cx);
                if self.selected_id.is_some() {
                    self.load_records(cx);
                    self.load_usage(cx);
                }
            }
        }
    }

    fn show_window(&self, cx: &mut App) {
        cx.activate(true);
        let _ = self.window.update(cx, |_, window, _| window.activate_window());
    }

    pub fn selected_service(&self) -> Option<&ServiceStatus> {
        let id = self.selected_id.as_ref()?;
        self.status.as_ref()?.services.iter().find(|s| &s.id == id)
    }

    pub fn t(&self, zh: &'static str, en: &'static str) -> &'static str {
        self.settings.prefs.t(zh, en)
    }

    pub fn perform<T, F, D>(&mut self, cx: &mut Context<Self>, future: F, done: D)
    where
        T: 'static,
        F: Future<Output = Result<T, String>> + 'static,
        D: FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    {
        if self.busy {
            return;
        }
        self.busy = true;
        self.epoch = self.epoch.wrapping_add(1);
        self.status_task = None;
        self.status_loading = false;
        self.error = None;
        cx.notify();
        self.job = Some(cx.spawn(async move |this, cx| {
            let result = future.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = false;
                view.epoch = view.epoch.wrapping_add(1);
                if let Err(error) = &result {
                    view.error = Some(error.clone());
                    view.pending_selection = None;
                }
                done(view, result, cx);
                if view.pending_selection.is_some() && !view.service_dirty(cx) {
                    let target = view.pending_selection.take().unwrap();
                    view.defer_selection(target, cx);
                }
                cx.notify();
            });
        }));
    }

    pub fn notice(&mut self, message: String, cx: &mut Context<Self>) {
        self.notice = Some(message);
        self.notice_task = Some(cx.spawn(async move |this, cx| {
            smol::Timer::after(Duration::from_secs(4)).await;
            let _ = this.update(cx, |view, cx| {
                view.notice = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub fn refresh_status(&mut self, cx: &mut Context<Self>) {
        if self.status_loading || self.busy {
            return;
        }
        self.status_loading = true;
        let api = self.api.clone();
        let epoch = self.epoch;
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let result = api.request::<Status>("GET", "/api/status", None).await;
            let _ = this.update(cx, |view, cx| {
                if view.epoch != epoch {
                    return;
                }
                view.status_loading = false;
                match result {
                    Err(error) => {
                        view.connected = false;
                        view.platform.offline(&error, &view.settings.prefs);
                        view.error = Some(error);
                    }
                    Ok(status) => {
                        view.connected = true;
                        let first = !view.initialized;
                        let missing = !view.service.is_new
                            && view
                                .selected_id
                                .as_ref()
                                .is_none_or(|id| !status.services.iter().any(|s| &s.id == id));
                        if first || missing {
                            if view
                                .selected_id
                                .as_ref()
                                .is_none_or(|id| !status.services.iter().any(|s| &s.id == id))
                            {
                                view.selected_id = status.services.first().map(|s| s.id.clone());
                            }
                            let config = view
                                .selected_id
                                .as_ref()
                                .and_then(|id| status.services.iter().find(|s| &s.id == id))
                                .map(|s| s.config.clone());
                            view.status = Some(status);
                            view.initialized = true;
                            let config = config.unwrap_or_else(|| view.new_service_config());
                            view.load_editor_deferred(config, view.selected_id.is_none(), false, cx);
                            view.cache.reset();
                            view.records.reset();
                            view.load_usage(cx);
                        } else {
                            let external = (!view.service.is_new)
                                .then(|| {
                                    view.selected_id
                                        .as_ref()
                                        .and_then(|id| status.services.iter().find(|s| &s.id == id))
                                })
                                .flatten()
                                .filter(|s| !view.editor_matches(&s.config) && !view.service_dirty(cx))
                                .map(|s| s.config.clone());
                            view.status = Some(status);
                            if let Some(config) = external {
                                view.load_editor_deferred(config, false, true, cx);
                            }
                        }
                        if let Some(status) = &view.status {
                            view.platform.refresh(status, view.tray_total, &view.settings.prefs);
                        }
                        view.scan_tray_usage(cx);
                        if view.pending_selection.is_some() && !view.service_dirty(cx) {
                            let target = view.pending_selection.take().unwrap();
                            view.defer_selection(target, cx);
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Sums disk usage of running services for the tray, outside the status refresh so slow
    /// scans never delay status publication.
    fn scan_tray_usage(&mut self, cx: &mut Context<Self>) {
        if self.usage_loading || self.last_usage_total.elapsed() < Duration::from_secs(15) {
            return;
        }
        let ids: Vec<String> = self
            .status
            .iter()
            .flat_map(|s| s.services.iter())
            .filter(|s| s.running)
            .map(|s| s.id.clone())
            .collect();
        self.usage_loading = true;
        let api = self.api.clone();
        self.usage_task = Some(cx.spawn(async move |this, cx| {
            let mut bytes = 0;
            let mut complete = true;
            for id in ids {
                match api
                    .request::<Usage>("GET", &crate::api::query("/api/usage", &[("service", id)]), None)
                    .await
                {
                    Ok(usage) => bytes += usage.total_bytes,
                    Err(_) => {
                        complete = false;
                        break;
                    }
                }
            }
            let _ = this.update(cx, |view, cx| {
                view.usage_loading = false;
                if !complete {
                    return;
                }
                view.tray_total = bytes;
                view.last_usage_total = Instant::now();
                if view.connected {
                    if let Some(status) = &view.status {
                        view.platform.refresh(status, view.tray_total, &view.settings.prefs);
                    }
                }
                cx.notify();
            });
        }));
    }

    fn load_editor_deferred(&self, config: Config, is_new: bool, only_clean: bool, cx: &mut Context<Self>) {
        let entity = cx.entity();
        let handle = self.window;
        let generation = self.generation;
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                entity.update(cx, |view, cx| {
                    if view.generation == generation && !(only_clean && view.service_dirty(cx)) {
                        view.load_service(config, is_new, window, cx);
                        cx.notify();
                    }
                });
            });
        });
    }

    fn defer_selection(&self, id: Option<String>, cx: &mut Context<Self>) {
        let entity = cx.entity();
        let handle = self.window;
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                entity.update(cx, |view, cx| view.commit_selection(id, window, cx));
            });
        });
    }

    pub fn select_service(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || (id == self.selected_id && !self.service.is_new && id.is_some()) {
            return;
        }
        if !self.service_dirty(cx) {
            self.commit_selection(id, window, cx);
            return;
        }
        let entity = cx.entity();
        let title = self.t("要儲存設定變更嗎？", "Save your changes?");
        let body = self.t(
            "切換連線前，請選擇儲存、捨棄或取消。",
            "Save or discard your changes before switching connections.",
        );
        let save_label = self.t("儲存並切換", "Save and switch");
        let discard_label = self.t("捨棄變更", "Discard changes");
        let cancel_label = self.t("取消", "Cancel");
        window.open_dialog(cx, move |dialog, _, cx| {
            let save_entity = entity.clone();
            let discard_entity = entity.clone();
            let save_target = id.clone();
            let discard_target = id.clone();
            let can_save = entity.read(cx).can_save(cx);
            dialog.title(title).child(body).footer(
                div()
                    .flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel-navigation")
                            .label(cancel_label)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("discard-navigation")
                            .label(discard_label)
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                discard_entity
                                    .update(cx, |view, cx| view.commit_selection(discard_target.clone(), window, cx));
                            }),
                    )
                    .child(
                        Button::new("save-navigation")
                            .primary()
                            .label(save_label)
                            .disabled(!can_save)
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                save_entity.update(cx, |view, cx| {
                                    view.pending_selection = Some(save_target.clone());
                                    view.save_service(false, cx);
                                });
                            }),
                    ),
            )
        });
    }

    fn commit_selection(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.selected_id = id;
        self.active_page = PageKind::Service;
        self.error = None;
        self.notice = None;
        self.pending_selection = None;
        self.records.reset();
        self.cache.reset();
        let config = self
            .selected_service()
            .map(|s| s.config.clone())
            .unwrap_or_else(|| self.new_service_config());
        self.load_service(config, self.selected_id.is_none(), window, cx);
        if let Ok(dir) = crate::api::data_dir() {
            if let Ok(data) = serde_json::to_vec(&self.selected_id) {
                let _ = std::fs::write(dir.join("gpui-selected.json"), data);
            }
        }
        self.load_usage(cx);
        cx.notify();
    }

    pub fn act_service(&mut self, id: String, start: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let api = self.api.clone();
        let kind = if start { "start" } else { "stop" };
        self.perform(
            cx,
            async move {
                api.request::<Status>("POST", &format!("/api/services/{id}/{kind}"), None)
                    .await
            },
            move |view, result, cx| {
                if let Ok(status) = result {
                    view.status = Some(status);
                    view.notice(
                        view.t(
                            if start {
                                "服務已啟動。"
                            } else {
                                "服務已停止。"
                            },
                            if start { "Service started." } else { "Service stopped." },
                        )
                        .into(),
                        cx,
                    );
                    view.refresh_status(cx);
                    view.load_usage(cx);
                }
            },
        );
    }

    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(service) = self.selected_service().cloned() else {
            return;
        };
        if self.busy || service.running || service.busy.is_some() {
            return;
        }
        let entity = cx.entity();
        let id = service.id;
        let title = self.t("刪除此連線？", "Delete this connection?");
        let body = self.t(
            "只移除連線設定；快取與歷史紀錄會保留。",
            "Only the connection settings are removed. Cache and history are kept.",
        );
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let entity = entity.clone();
            let id = id.clone();
            dialog
                .title(title)
                .description(format!("{}\n{body}", service_label(&service.config)))
                .confirm()
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |view, cx| {
                        let api = view.api.clone();
                        let id = id.clone();
                        view.perform(
                            cx,
                            async move {
                                api.request::<Value>("DELETE", &format!("/api/services/{id}"), None)
                                    .await
                            },
                            |view, result, cx| {
                                if result.is_ok() {
                                    view.selected_id = None;
                                    view.service.is_new = false;
                                    view.initialized = false;
                                    view.records.reset();
                                    view.cache.reset();
                                    view.refresh_status(cx);
                                    view.notice(view.t("連線設定已刪除。", "Connection deleted.").into(), cx);
                                }
                            },
                        );
                    });
                    true
                })
        });
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = crate::ui::palette(cx);
        let services = self.status.as_ref().map(|s| s.services.as_slice()).unwrap_or_default();
        let mut profiles = div().id("connections").flex().flex_col().gap(px(2.));
        for service in services {
            let id = service.id.clone();
            let active = self.selected_id.as_ref() == Some(&id) && !self.service.is_new;
            let name = service_label(&service.config);
            let state = if service.busy.is_some() {
                self.t("處理中…", "Working…")
            } else if service.running {
                self.t("執行中", "Running")
            } else {
                self.t("已停止", "Stopped")
            };
            let role = if service.config.mode == "serve" {
                "Server"
            } else {
                "Proxy"
            };
            let style = ButtonCustomVariant::new(cx)
                .color(if active { p.subtle } else { transparent_black() })
                .foreground(p.text)
                .hover(p.subtle)
                .active(p.subtle);
            profiles = profiles.child(
                Button::new(SharedString::from(format!("service-{id}")))
                    .custom(style)
                    .w_full()
                    .h(px(44.))
                    .px(px(12.))
                    .rounded(px(6.))
                    .justify_start()
                    .when(active, |b| b.bg(p.subtle))
                    .disabled(self.busy)
                    .accessibility_label(format!("{name}, {role}, {state}"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .w_full()
                            .min_w_0()
                            .child(status_dot(
                                service.running,
                                px(8.),
                                if service.running { p.live } else { p.idle },
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .flex_1()
                                    .gap(px(1.))
                                    .child(
                                        div()
                                            .text_size(px(12.5))
                                            .font_weight(if active {
                                                FontWeight::SEMIBOLD
                                            } else {
                                                FontWeight::NORMAL
                                            })
                                            .truncate()
                                            .child(name),
                                    )
                                    .child(div().text_size(px(10.5)).text_color(p.muted).child(format!(
                                        "{role} · {state}{}",
                                        if service.error.is_some() {
                                            self.t(" · 錯誤", " · Error")
                                        } else {
                                            ""
                                        }
                                    ))),
                            ),
                    )
                    .on_click(
                        cx.listener(move |view, _, window, cx| view.select_service(Some(id.clone()), window, cx)),
                    ),
            );
        }
        if self.service.is_new {
            profiles = profiles.child(
                Button::new("new-draft")
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(p.subtle)
                            .foreground(p.text)
                            .hover(p.subtle)
                            .active(p.subtle),
                    )
                    .w_full()
                    .h(px(36.))
                    .px(px(12.))
                    .rounded(px(6.))
                    .justify_start()
                    .bg(p.subtle)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .w_full()
                            .min_w_0()
                            .child(status_dot(false, px(8.), p.idle))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .truncate()
                                    .child(self.t("新連線（未儲存）", "New connection (unsaved)")),
                            ),
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.active_page = PageKind::Service;
                        cx.notify();
                    })),
            );
        }
        let inset = crate::platform::TITLEBAR_INSET;
        div()
            .flex()
            .flex_col()
            .w(px(216.))
            .flex_none()
            .h_full()
            .pt(px(if inset > 0. { 0. } else { 22. }))
            .px(px(12.))
            .pb(px(16.))
            .gap(px(6.))
            .bg(p.panel)
            .border_r_1()
            .border_color(p.border)
            // Traffic-light row (macOS): empty, moves the window.
            .when(inset > 0., |side| {
                side.child(crate::platform::window_drag(
                    div().flex_none().h(px(inset)).mx(px(-12.)),
                ))
            })
            .child(
                crate::platform::window_drag(div().flex().items_center().gap(px(11.)).px(px(10.)).pb(px(14.)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(34.))
                            .rounded(px(10.))
                            .bg(p.accent)
                            .text_color(p.on_accent)
                            .text_size(px(22.))
                            .font_weight(FontWeight::BOLD)
                            .child("S"),
                    )
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::BOLD)
                            .child("SwiftCache"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pr(px(4.))
                    .child(
                        div()
                            .px(px(12.))
                            .py(px(4.))
                            .text_size(px(10.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(p.muted)
                            .child(self.t("連線", "Connections")),
                    )
                    .child(
                        Button::new("new-connection")
                            .ghost()
                            .w(px(22.))
                            .h(px(22.))
                            .px(px(0.))
                            .rounded(px(5.))
                            .text_color(p.muted)
                            .child(div().text_size(px(16.)).line_height(px(16.)).child("+"))
                            .accessibility_label(self.t("新增連線", "New connection"))
                            .disabled(self.busy || !self.connected)
                            .on_click(cx.listener(|view, _, window, cx| view.select_service(None, window, cx))),
                    ),
            )
            .child(profiles)
            .child(div().flex_1().min_h(px(12.)))
            .child(
                Button::new("preferences")
                    .ghost()
                    .h(px(34.))
                    .w_full()
                    .px(px(12.))
                    .rounded(px(6.))
                    .justify_start()
                    .text_color(p.muted)
                    .accessibility_label(self.t("設定", "Settings"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .w_full()
                            .gap(px(8.))
                            .text_size(px(12.5))
                            .child("⚙")
                            .child(self.t("設定", "Settings")),
                    )
                    .on_click(cx.listener(|view, _, window, cx| view.open_preferences(window, cx))),
            )
            .into_any_element()
    }

    fn render_header(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let p = crate::ui::palette(cx);
        let service = self.selected_service();
        let running = service.is_some_and(|s| s.running);
        let locked = self.busy || service.is_some_and(|s| s.busy.is_some()) || !self.connected;
        let draft = self.current_config(cx).ok();
        let config = draft.as_ref().or_else(|| service.map(|s| &s.config));
        let dirty = self.service_dirty(cx);
        let valid = draft.is_some();
        let name = if self.service.is_new {
            self.t("新連線（未儲存）", "New connection (unsaved)").to_string()
        } else {
            service.map(|s| service_label(&s.config)).unwrap_or_default()
        };
        let mode = config
            .map(|c| if c.mode == "serve" { "Server" } else { "Proxy" })
            .unwrap_or("Server");
        let mut state = if service.and_then(|s| s.busy.as_ref()).is_some() {
            self.t("處理中…", "Working…").to_string()
        } else if running {
            self.t("執行中", "Running").to_string()
        } else {
            self.t("已停止", "Stopped").to_string()
        };
        if let Some(started) = service.filter(|s| s.running).and_then(|s| s.started_at) {
            let seconds = ((chrono::Utc::now().timestamp_millis() - started).max(0) / 1000) as u64;
            state.push_str(&format!(
                " · {} {:02}:{:02}:{:02}",
                self.t("已執行", "Up"),
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            ));
        }
        let title = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .min_w_0()
            .flex_wrap()
            .child(
                div()
                    .text_size(px(17.))
                    .font_weight(FontWeight(650.))
                    .truncate()
                    .child(name),
            )
            .child(
                div()
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .bg(if running { p.accent_bg } else { p.chip })
                    .text_color(if running { p.accent } else { p.muted })
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(mode),
            );
        let listener = config
            .map(|c| match (&c.tcp, &c.socket) {
                (Some(tcp), Some(socket)) if c.mode == "serve" => format!("{tcp} · {socket}"),
                (Some(tcp), _) if c.mode == "serve" => tcp.clone(),
                (_, Some(socket)) => socket.clone(),
                _ => self.t("未設定", "Not set").to_string(),
            })
            .unwrap_or_else(|| self.t("未設定", "Not set").to_string());
        let fact = |label: &'static str, value: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .min_w_0()
                .max_w(px(260.))
                .child(
                    div()
                        .text_size(px(10.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.muted)
                        .child(label),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(px(12.5))
                        .truncate()
                        .child(value),
                )
        };
        let mut facts = div()
            .flex()
            .items_center()
            .gap(px(24.))
            .flex_1()
            .min_w_0()
            .child(fact("LISTENER", listener))
            .child(fact(
                "STORE",
                config
                    .map(|c| c.store.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| self.t("未設定", "Not set").to_string()),
            ));
        if let Some(c) = config.filter(|c| c.mode == "proxy") {
            facts = facts.child(fact("UPSTREAM", c.upstream.clone()));
        }
        facts = facts.child(
            fact(
                "USAGE",
                self.cache
                    .usage
                    .as_ref()
                    .map(|u| format_bytes(u.total_bytes))
                    .unwrap_or_else(|| "—".to_string()),
            )
            .flex_none(),
        );
        let button = |id: &'static str, label: &'static str| {
            Button::new(id)
                .label(label)
                .h(px(36.))
                .px(px(16.))
                .rounded(px(7.))
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
        };
        let actions = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .flex_none()
            .when(service.is_some(), |row| {
                row.child(
                    button("delete-connection", self.t("刪除連線", "Delete connection"))
                        .danger()
                        .outline()
                        .border_color(p.danger_border)
                        .text_color(p.danger)
                        .disabled(locked || running)
                        .when(locked || running, |b| b.opacity(0.5))
                        .on_click(cx.listener(|view, _, window, cx| view.delete_selected(window, cx))),
                )
            })
            // Always present (disabled while clean) so the actions row never shifts.
            .when(service.is_some(), |row| {
                row.child(
                    button("revert-changes", self.t("還原變更", "Revert changes"))
                        .disabled(locked || running || !dirty)
                        .on_click(cx.listener(|view, _, window, cx| {
                            if let Some(config) = view.selected_service().map(|s| s.config.clone()) {
                                view.load_service(config, false, window, cx);
                            }
                        })),
                )
            })
            .child(
                button("save-settings", self.t("儲存設定", "Save settings"))
                    .disabled(locked || running || !dirty || !valid)
                    .on_click(cx.listener(|view, _, _, cx| view.save_service(false, cx))),
            )
            .child(
                button(
                    "toggle-service",
                    if running {
                        self.t("■ 停止服務", "■ Stop service")
                    } else if dirty {
                        self.t("儲存並啟動", "Save and start")
                    } else {
                        self.t("▶ 啟動服務", "▶ Start service")
                    },
                )
                .when(!running, |button| button.primary())
                .disabled(locked || (!running && !valid))
                .on_click(cx.listener(|view, _, _, cx| {
                    if view.service.is_new
                        || (view.selected_service().is_some_and(|s| !s.running) && view.service_dirty(cx))
                    {
                        view.save_service(true, cx);
                    } else if let Some(service) = view.selected_service() {
                        let id = service.id.clone();
                        let start = !service.running;
                        view.act_service(id, start, cx);
                    }
                })),
            );
        let narrow = window.viewport_size().width < px(1180.);
        let header = div()
            .flex()
            .items_center()
            .gap(px(if narrow { 14. } else { 20. }))
            .px(px(18.))
            .py(px(14.))
            .min_h(px(68.))
            .rounded(px(10.))
            .border_1()
            .border_color(p.border)
            .bg(p.panel)
            .when(narrow, |header| header.flex_wrap())
            .child(status_dot(running, px(12.), if running { p.live } else { p.idle }))
            .child(crate::platform::window_drag(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .min_w_0()
                    .max_w(px(320.))
                    .child(title)
                    .child(div().text_size(px(12.)).text_color(p.muted).child(state)),
            ))
            .child(crate::platform::window_drag(facts))
            .child(
                div()
                    .when(narrow, |row| row.w_full().flex().justify_end())
                    .child(actions),
            );
        let mut root = div().flex().flex_col().gap(px(14.)).child(header);
        if !self.service.is_new {
            root = root.child(
                div().flex().gap(px(2.)).border_b_1().border_color(p.border).children(
                    [
                        (PageKind::Service, self.t("組態", "Configuration")),
                        (PageKind::Transfers, self.t("傳輸明細", "Transfers")),
                        (PageKind::Events, self.t("事件紀錄", "Events")),
                        (PageKind::Cache, self.t("快取", "Cache")),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, (page, label))| {
                        let active = self.active_page == page;
                        let count = service
                            .filter(|_| page == PageKind::Transfers)
                            .map(|s| s.summary.transfers)
                            .unwrap_or(0);
                        Button::new(("page", i))
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .foreground(if active { p.accent } else { p.muted })
                                    .hover(transparent_black())
                                    .active(transparent_black()),
                            )
                            .role(Role::Tab)
                            .selected(active)
                            .h(px(34.))
                            .px(px(10.))
                            .py(px(8.))
                            .rounded_none()
                            .border_0()
                            .border_b_2()
                            .border_color(if active { p.accent } else { transparent_black() })
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .accessibility_label(if count > 0 {
                                format!("{label} {count}")
                            } else {
                                label.to_string()
                            })
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(label)
                                    .when(count > 0, |row| {
                                        row.child(
                                            div()
                                                .text_size(px(12.))
                                                .font_weight(FontWeight::NORMAL)
                                                .text_color(p.muted)
                                                .child(count.to_string()),
                                        )
                                    }),
                            )
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.active_page = page;
                                view.load_records(cx);
                                view.load_usage(cx);
                                cx.notify();
                            }))
                    }),
                ),
            );
        }
        root.into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.settings.apply(cx);
        let sidebar = self.render_sidebar(cx);
        let header = self.render_header(window, cx);
        let top = if window.viewport_size().width < px(1180.) {
            18.
        } else {
            22.
        };
        let content = if self.status.is_none() {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_6()
                .child(self.t("正在連線管理程序…", "Connecting to daemon…"))
                .child(
                    Button::new("retry-connection")
                        .label(self.t("重試", "Retry"))
                        .on_click(cx.listener(|view, _, _, cx| view.refresh_status(cx))),
                )
                .into_any_element()
        } else {
            match self.active_page {
                PageKind::Service => self.render_service(window, cx),
                PageKind::Transfers => self.render_transfers(window, cx),
                PageKind::Events => self.render_events(window, cx),
                PageKind::Cache => self.render_cache(window, cx),
            }
        };
        div()
            .key_context("SwiftCache")
            .track_focus(&self.focus)
            .on_action(cx.listener(|view, _: &SaveSettings, window, cx| {
                if !window.has_active_dialog(cx) && view.can_save(cx) {
                    view.save_service(false, cx);
                }
            }))
            .on_action(cx.listener(|view, _: &Refresh, _, cx| {
                view.refresh_status(cx);
                view.load_records(cx);
                view.load_usage(cx);
            }))
            .flex()
            .size_full()
            .font_family(cx.theme().font_family.clone())
            .text_size(px(14.))
            .line_height(relative(1.25))
            .bg(crate::ui::palette(cx).bg)
            .text_color(crate::ui::palette(cx).text)
            .child(sidebar)
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .gap(px(14.))
                    .pt(px(top))
                    .px(px(if window.viewport_size().width < px(1180.) {
                        20.
                    } else {
                        32.
                    }))
                    .pb(px(if window.viewport_size().width < px(1180.) {
                        24.
                    } else {
                        28.
                    }))
                    // The top padding shares the traffic-light row: drag strip above the header card.
                    .child(crate::platform::window_drag(
                        div().absolute().top_0().left_0().right_0().h(px(top)),
                    ))
                    .child(header)
                    .when_some(self.error.clone(), |body, error| {
                        body.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(12.))
                                .px(px(14.))
                                .py(px(10.))
                                .rounded(px(8.))
                                .border_1()
                                .border_color(crate::ui::palette(cx).danger_border)
                                .bg(crate::ui::palette(cx).danger_bg)
                                .text_color(crate::ui::palette(cx).danger_text)
                                .child(div().flex_1().child(error))
                                .child(Button::new("dismiss-error").ghost().label("×").on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.error = None;
                                        cx.notify();
                                    },
                                ))),
                        )
                    })
                    .child(div().flex_1().min_h_0().min_w_0().child(content))
                    .when_some(self.notice.clone(), |body, message| {
                        body.child(
                            div()
                                .absolute()
                                .bottom(px(24.))
                                .right(px(28.))
                                .rounded(px(8.))
                                .px(px(16.))
                                .py(px(12.))
                                .border_1()
                                .border_color(crate::ui::palette(cx).accent_border)
                                .bg(crate::ui::palette(cx).accent_bg)
                                .text_color(crate::ui::palette(cx).accent)
                                .child(message),
                        )
                    }),
            )
    }
}

fn status_dot(running: bool, size: Pixels, color: Hsla) -> Div {
    div()
        .relative()
        .size(size)
        .flex_none()
        .when(running, |dot| {
            dot.child(
                div()
                    .absolute()
                    .left(px(-3.))
                    .top(px(-3.))
                    .size(size + px(6.))
                    .rounded_full()
                    .bg(color.opacity(0.22)),
            )
        })
        .child(div().relative().size(size).rounded_full().bg(color))
}
