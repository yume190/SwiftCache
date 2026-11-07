//! Cache page: local usage breakdown, guarded clear (plan → review → confirm) and cleanup history.
//!
//! Serve and Proxy share the same flow. Both only ever clear *their own local store*
//! (plus registered local build paths the user ticks); the backend plans the exact paths,
//! drains/stops the service, deletes, records the result, and optionally restarts it.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{ActiveTheme, Disableable, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Value, json};

use crate::api::query;
use crate::app::Workspace;
use crate::types::{AppEvent, ClearPlan, ClearResult, Page, PageKind, Usage, format_bytes, format_int, format_time, service_label};

const USAGE_TTL: Duration = Duration::from_secs(30);
const USAGE_RETRY: Duration = Duration::from_secs(5);
const HISTORY_TTL: Duration = Duration::from_secs(10);
const HISTORY_LIMIT: usize = 50;
const COLLAPSED_HISTORY: usize = 3;

/// One in-progress clear review. Exists from "Review scope…" until cancel/success.
struct Review {
    service_id: String,
    plan: Option<ClearPlan>,
    plan_error: Option<String>,
    /// Local paths ticked when the review opened; what the plan is confirmed against.
    paths: Vec<String>,
    restart: bool,
    /// Last failed clear attempt (kept while the plan is refetched for the retry).
    error: Option<String>,
    seq: u64,
    task: Option<Task<()>>,
}

pub struct CacheState {
    pub usage: Option<Usage>,
    pub usage_error: Option<String>,
    pub scanning: bool,
    usage_seq: u64,
    usage_at: Option<(Instant, Duration)>,
    usage_task: Option<Task<()>>,
    picked: Vec<String>,
    review: Option<Review>,
    plan_seq: u64,
    /// True while a clear request is in flight.
    clearing: bool,
    last_result: Option<ClearResult>,
    history: Option<Vec<AppEvent>>,
    history_error: Option<String>,
    history_seq: u64,
    history_at: Option<Instant>,
    history_task: Option<Task<()>>,
    show_all: bool,
    expanded: HashSet<i64>,
}

impl CacheState {
    pub fn new() -> Self {
        Self {
            usage: None,
            usage_error: None,
            scanning: false,
            usage_seq: 0,
            usage_at: None,
            usage_task: None,
            picked: Vec::new(),
            review: None,
            plan_seq: 0,
            clearing: false,
            last_result: None,
            history: None,
            history_error: None,
            history_seq: 0,
            history_at: None,
            history_task: None,
            show_all: false,
            expanded: HashSet::new(),
        }
    }

    /// Drops everything tied to the previous selection; in-flight loads are cancelled with their tasks.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for CacheState {
    fn default() -> Self {
        Self::new()
    }
}

fn field<'a>(ev: &'a AppEvent, key: &str) -> Option<&'a Value> {
    ev.fields.as_object().and_then(|o| o.get(key))
}

/// Colors copied out of the shared palette so rendering can use `cx` mutably afterwards.
#[derive(Clone)]
struct Pal {
    panel: Hsla,
    text: Hsla,
    muted: Hsla,
    border: Hsla,
    accent: Hsla,
    accent_bg: Hsla,
    accent_soft: Hsla,
    accent_border: Hsla,
    purple: Hsla,
    blue: Hsla,
    danger: Hsla,
    danger_bg: Hsla,
    danger_border: Hsla,
    danger_text: Hsla,
    warn_bg: Hsla,
    track: Hsla,
    info_bg: Hsla,
    info_border: Hsla,
    info_text: Hsla,
    on_accent: Hsla,
    mono: SharedString,
    v_plain: ButtonCustomVariant,
    v_danger: ButtonCustomVariant,
    v_solid: ButtonCustomVariant,
    v_link: ButtonCustomVariant,
}

impl Pal {
    fn new(cx: &App) -> Self {
        let c = crate::ui::palette(cx);
        Self {
            panel: c.panel,
            text: c.text,
            muted: c.muted,
            border: c.border,
            accent: c.accent,
            accent_bg: c.accent_bg,
            accent_soft: c.accent_soft,
            accent_border: c.accent_border,
            purple: c.purple,
            blue: c.blue,
            danger: c.danger,
            danger_bg: c.danger_bg,
            danger_border: c.danger_border,
            danger_text: c.danger_text,
            warn_bg: c.warn_bg,
            track: c.track,
            info_bg: c.info_bg,
            info_border: c.info_border,
            info_text: c.info_text,
            on_accent: c.on_accent,
            mono: cx.theme().mono_font_family.clone(),
            v_plain: ButtonCustomVariant::new(cx).color(c.panel).foreground(c.text).hover(c.subtle).active(c.subtle),
            v_danger: ButtonCustomVariant::new(cx).color(c.panel).foreground(c.danger).hover(c.subtle).active(c.subtle),
            v_solid: ButtonCustomVariant::new(cx).color(c.danger).foreground(c.on_accent).hover(c.danger_hover).active(c.danger_hover),
            v_link: ButtonCustomVariant::new(cx).foreground(c.accent),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum BtnKind {
    /// `.btn`
    Plain,
    /// `.btn.danger`
    Danger,
    /// `.btn.danger.solid`
    Solid,
}

/// `.card.panel`: 1px border, radius 10, padding 18/20, gap 14.
fn card(p: &Pal) -> Div {
    v_flex()
        .w_full()
        .min_w(px(0.))
        .px(px(20.))
        .py(px(18.))
        .gap(px(14.))
        .rounded(px(10.))
        .border_1()
        .border_color(p.border)
        .bg(p.panel)
}

/// `h2`: 16px / 650.
fn h2(p: &Pal, text: impl Into<SharedString>) -> Div {
    div().text_size(px(16.)).font_weight(FontWeight(650.)).text_color(p.text).child(text.into())
}

/// `p.muted.small`: 12px muted.
fn note(p: &Pal, text: impl Into<SharedString>) -> Div {
    div().text_size(px(12.)).text_color(p.muted).child(text.into())
}

/// `.empty.small`: centred 13px muted, padding 18/8.
fn empty_small(p: &Pal, text: impl Into<SharedString>) -> Div {
    div().px(px(8.)).py(px(18.)).flex().justify_center().text_size(px(13.)).text_color(p.muted).child(text.into())
}

/// `.mono`: 12.5px monospace (use 12 for `.mono.small`).
fn mono(p: &Pal, size: f32) -> Div {
    div().font_family(p.mono.clone()).text_size(px(size))
}

/// `dl.kv` container: row gap and font size (`.kv` 9/13, `.kv.wide` 14/13, `.kv.tight` 5/12.5).
fn kv_box(row_gap: f32, size: f32) -> Div {
    v_flex().gap(px(row_gap)).text_size(px(size))
}

/// One `dt`/`dd` pair; label column `dt_w` wide (96, or 84 for `.kv.wide`), column gap 12.
fn kv(p: &Pal, dt_w: f32, key: impl Into<SharedString>, value: impl IntoElement) -> Div {
    h_flex()
        .gap(px(12.))
        .items_start()
        .child(div().w(px(dt_w)).flex_none().text_color(p.muted).child(key.into()))
        .child(div().flex_1().min_w(px(0.)).child(value))
}

/// `ul.plain.mono.small`: 12px monospace lines, 4px above.
fn mono_list(p: &Pal, items: &[String]) -> Div {
    v_flex().mt(px(4.)).children(items.iter().map(|s| mono(p, 12.).child(s.clone())))
}

/// `.banner`: padding 10/14, radius 8, 13px, 1px border. `error` selects the danger palette, otherwise info.
fn banner(p: &Pal, error: bool, text: impl Into<SharedString>) -> Div {
    let (bg, border, fg) = if error { (p.danger_bg, p.danger_border, p.danger_text) } else { (p.info_bg, p.info_border, p.info_text) };
    div()
        .w_full()
        .px(px(14.))
        .py(px(10.))
        .rounded(px(8.))
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_size(px(13.))
        .child(text.into())
}

/// `.scope` (`.scope.on` when `on`): padding 14/16, gap 8, radius 8.
fn scope_box(p: &Pal, on: bool) -> Div {
    v_flex()
        .gap(px(8.))
        .px(px(16.))
        .py(px(14.))
        .rounded(px(8.))
        .border_1()
        .border_color(if on { p.accent_border } else { p.border })
        .when(on, |d| d.bg(p.accent_soft))
}

/// `.btn` (36px) / `.btn.small` (28px): native gpui-kit `Button` (focusable, Enter/Space, a11y role/label) with a
/// custom palette variant and CSS geometry; label is a child so its 13px/12px size overrides the Button's own.
fn btn(
    p: &Pal,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: BtnKind,
    small: bool,
    disabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let label: SharedString = label.into();
    let (variant, border) = match kind {
        BtnKind::Plain => (p.v_plain, p.border),
        BtnKind::Danger => (p.v_danger, p.danger_border),
        BtnKind::Solid => (p.v_solid, p.danger),
    };
    Button::new(id)
        .custom(variant)
        .accessibility_label(label.clone())
        .h(px(if small { 28. } else { 36. }))
        .px(px(if small { 10. } else { 16. }))
        .rounded(px(7.))
        .border_1()
        .border_color(border)
        .disabled(disabled)
        // A custom variant paints only 20% of its color at rest; the solid button needs the full fill.
        .when(matches!(kind, BtnKind::Solid) && !disabled, |b| b.bg(p.danger))
        .on_click(on_click)
        .child(div().text_size(px(if small { 12. } else { 13. })).font_weight(FontWeight::SEMIBOLD).child(label))
}

/// `.link`: native `Button` with a transparent accent-text variant, 12.5px / 600.
fn link(p: &Pal, id: impl Into<ElementId>, label: impl Into<SharedString>, on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Button {
    let label: SharedString = label.into();
    Button::new(id)
        .custom(p.v_link)
        .accessibility_label(label.clone())
        .h(px(20.))
        .px(px(0.))
        .on_click(on_click)
        .child(div().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).child(label))
}

/// `.check`: native gpui-kit `Checkbox` (focusable, Space toggles, checked/disabled a11y) with CSS gap 10 / weight 500.
/// `on_click` receives the new checked value; `a11y` is the accessible name (the visible label is a child element).
fn check(
    _p: &Pal,
    id: impl Into<ElementId>,
    checked: bool,
    disabled: bool,
    a11y: impl Into<SharedString>,
    label: impl IntoElement,
    on_click: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Checkbox {
    Checkbox::new(id)
        .checked(checked)
        .disabled(disabled)
        .accessibility_label(a11y)
        .gap(px(10.))
        .font_weight(FontWeight::MEDIUM)
        .child(label)
        .on_click(on_click)
}

/// `.check.disabled` with a checked box: the whole row at 0.7 opacity and the box still full accent. gpui-kit's
/// disabled `Checkbox` halves the box and glyph instead, which leaves the always-included scope looking unticked.
/// Not focusable or clickable; exposed as a checked checkbox.
fn locked_check(p: &Pal, id: impl Into<ElementId>, a11y: impl Into<SharedString>, label: impl IntoElement) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::CheckBox)
        .aria_toggled(Toggled::True)
        .aria_label(a11y)
        .flex()
        .items_center()
        .gap(px(10.))
        .opacity(0.7)
        .cursor_not_allowed()
        .font_weight(FontWeight::MEDIUM)
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(16.))
                .rounded(px(4.))
                .border_1()
                .border_color(p.accent)
                .bg(p.accent)
                .text_color(p.on_accent)
                .text_size(px(11.))
                .font_weight(FontWeight::BOLD)
                .child("✓"),
        )
        .child(label)
}

impl Workspace {
    /// The saved service the cache page applies to; `None` while nothing (or an unsaved draft) is selected.
    fn cache_target(&self) -> Option<String> {
        if self.service.is_new {
            return None;
        }
        let id = self.selected_id.as_ref()?;
        self.status.as_ref()?.services.iter().any(|s| &s.id == id).then(|| id.clone())
    }

    fn cache_local_paths(&self) -> Vec<String> {
        let Some(id) = self.cache_target() else { return Vec::new() };
        let Some(svc) = self.status.as_ref().and_then(|s| s.services.iter().find(|s| s.id == id)) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        svc.config
            .local_cache_paths
            .iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty() && seen.insert(p.clone()))
            .collect()
    }

    /// Ticked paths that are still configured, in configuration order.
    fn cache_effective_picked(&self) -> Vec<String> {
        self.cache_local_paths().into_iter().filter(|p| self.cache.picked.contains(p)).collect()
    }

    /// Loads usage (and, on the Cache page, cleanup history) when stale. Safe to call from every poll.
    pub fn load_usage(&mut self, cx: &mut Context<Self>) {
        self.scan_usage(false, cx);
        if self.active_page == PageKind::Cache {
            self.load_cleanup_history(false, cx);
        }
    }

    /// Forces a fresh cleanup-history fetch (e.g. after the Records page cleared events).
    pub fn refresh_cleanup_history(&mut self, cx: &mut Context<Self>) {
        self.load_cleanup_history(true, cx);
    }

    fn scan_usage(&mut self, fresh: bool, cx: &mut Context<Self>) {
        let Some(id) = self.cache_target() else { return };
        if self.cache.clearing {
            return;
        }
        if !fresh {
            if self.cache.scanning {
                return;
            }
            if let Some((at, ttl)) = self.cache.usage_at {
                if at.elapsed() < ttl {
                    return;
                }
            }
        }
        self.cache.usage_seq += 1;
        let seq = self.cache.usage_seq;
        self.cache.scanning = true;
        let generation = self.generation;
        let api = self.api.clone();
        let path = query("/api/usage", &[("service", id.clone()), ("fresh", if fresh { "1".to_string() } else { String::new() })]);
        self.cache.usage_task = Some(cx.spawn(async move |this, cx| {
            let result = api.request::<Usage>("GET", &path, None).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.selected_id.as_deref() != Some(id.as_str()) || this.cache.usage_seq != seq {
                    return;
                }
                this.cache.scanning = false;
                this.cache.usage_task = None;
                match result {
                    Ok(usage) => {
                        this.cache.usage = Some(usage);
                        this.cache.usage_error = None;
                        this.cache.usage_at = Some((Instant::now(), USAGE_TTL));
                    }
                    Err(e) => {
                        this.cache.usage_error = Some(e);
                        this.cache.usage_at = Some((Instant::now(), USAGE_RETRY));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn load_cleanup_history(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(id) = self.cache_target() else { return };
        if !force {
            if self.cache.history_task.is_some() {
                return;
            }
            if self.cache.history_at.is_some_and(|at| at.elapsed() < HISTORY_TTL) {
                return;
            }
        }
        self.cache.history_seq += 1;
        let seq = self.cache.history_seq;
        let generation = self.generation;
        let api = self.api.clone();
        let path = query(
            "/api/events",
            &[("kind", "cleanup".to_string()), ("service_id", id.clone()), ("limit", HISTORY_LIMIT.to_string())],
        );
        self.cache.history_task = Some(cx.spawn(async move |this, cx| {
            let result = api.request::<Page<AppEvent>>("GET", &path, None).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.selected_id.as_deref() != Some(id.as_str()) || this.cache.history_seq != seq {
                    return;
                }
                this.cache.history_task = None;
                this.cache.history_at = Some(Instant::now());
                match result {
                    Ok(page) => {
                        this.cache.history = Some(page.items);
                        this.cache.history_error = None;
                    }
                    Err(e) => this.cache.history_error = Some(e),
                }
                cx.notify();
            });
        }));
    }

    fn cache_toggle_path(&mut self, path: String, on: bool, cx: &mut Context<Self>) {
        if self.cache.review.is_some() || self.cache.clearing {
            return;
        }
        self.cache.picked.retain(|p| p != &path);
        if on {
            self.cache.picked.push(path);
        }
        cx.notify();
    }

    fn open_clear_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.cache.clearing || self.cache.review.is_some() || window.has_active_dialog(cx) {
            return;
        }
        let Some(id) = self.cache_target() else { return };
        let Some(svc) = self.status.as_ref().and_then(|s| s.services.iter().find(|s| s.id == id)) else { return };
        if svc.busy.is_some() {
            return;
        }
        let running = svc.running;
        self.cache.review = Some(Review {
            service_id: id,
            plan: None,
            plan_error: None,
            paths: self.cache_effective_picked(),
            restart: running,
            error: None,
            seq: 0,
            task: None,
        });
        self.fetch_clear_plan(cx);
        // Native dialog: focus-trapped, registers with `has_active_dialog` (so window shortcuts stay inert behind it),
        // Escape/backdrop go through `on_cancel`, and the Root restores focus when it closes.
        let workspace = cx.entity();
        let view = cx.new(|cx| ClearReviewView::new(workspace.clone(), window, cx));
        let title = self.t("清除確認", "Confirm clear");
        let accent = Pal::new(cx).accent;
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let ws = workspace.clone();
            dialog
                .w(px(640.))
                .px(px(26.))
                .py(px(24.))
                .rounded(px(12.))
                .close_button(false)
                .title(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(accent).child(title))
                .child(view.clone())
                .on_cancel(move |_, _, cx| {
                    // Never close here: `cancel_clear_review` ends the review (refused while clearing) and the
                    // view's observer then closes the dialog exactly once.
                    ws.update(cx, |this, cx| this.cancel_clear_review(cx));
                    false
                })
        });
        cx.notify();
    }

    fn fetch_clear_plan(&mut self, cx: &mut Context<Self>) {
        let Some(review) = self.cache.review.as_mut() else { return };
        self.cache.plan_seq += 1;
        let seq = self.cache.plan_seq;
        review.seq = seq;
        review.plan = None;
        review.plan_error = None;
        let id = review.service_id.clone();
        let generation = self.generation;
        let api = self.api.clone();
        let path = query("/api/clear-plan", &[("service", id.clone())]);
        review.task = Some(cx.spawn(async move |this, cx| {
            let result = api.request::<ClearPlan>("GET", &path, None).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.selected_id.as_deref() != Some(id.as_str()) {
                    return;
                }
                let Some(review) = this.cache.review.as_mut() else { return };
                if review.seq != seq {
                    return;
                }
                review.task = None;
                match result {
                    Ok(plan) => {
                        review.plan = Some(plan);
                        review.plan_error = None;
                    }
                    Err(e) => review.plan_error = Some(e),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn cancel_clear_review(&mut self, cx: &mut Context<Self>) {
        if self.cache.clearing {
            return;
        }
        // Dropping the review cancels its plan task; nothing was sent to the backend.
        self.cache.review = None;
        cx.notify();
    }

    fn confirm_clear(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.cache.clearing {
            return;
        }
        let Some(id) = self.cache_target() else { return };
        if self.status.as_ref().and_then(|s| s.services.iter().find(|s| s.id == id)).is_none_or(|s| s.busy.is_some()) {
            return;
        }
        let Some(review) = self.cache.review.as_mut() else { return };
        let Some(plan) = review.plan.as_ref() else { return };
        let body = json!({
            "service": id,
            "restart": review.restart,
            "local_paths": review.paths,
            "plan_id": plan.plan_id,
        });
        review.error = None;
        self.cache.clearing = true;
        // A scan running across the clear would publish pre-clear numbers.
        self.cache.usage_seq += 1;
        self.cache.scanning = false;
        self.cache.usage_task = None;
        let generation = self.generation;
        let api = self.api.clone();
        self.perform(
            cx,
            async move { api.request::<ClearResult>("POST", "/api/clear", Some(body)).await },
            move |this, result, cx| {
                this.cache.clearing = false;
                let same = this.generation == generation && this.selected_id.as_deref() == Some(id.as_str());
                match result {
                    Ok(r) => {
                        if same {
                            let message = format!(
                                "{} {} · {} {}",
                                this.t("已回收", "Reclaimed"),
                                format_bytes(r.reclaimed_bytes),
                                format_int(r.removed_files),
                                this.t("個檔案", "files"),
                            );
                            this.cache.usage = Some(r.usage.clone());
                            this.cache.usage_error = None;
                            this.cache.usage_at = Some((Instant::now(), USAGE_TTL));
                            this.cache.review = None;
                            this.cache.last_result = Some(r);
                            this.notice(message, cx);
                        }
                    }
                    Err(e) => {
                        // 409 (plan out of date / service busy) and every other failure: show the reason, then
                        // re-review against a freshly computed plan so a retry confirms the current paths.
                        if same {
                            if let Some(review) = this.cache.review.as_mut() {
                                review.error = Some(e);
                            }
                            this.fetch_clear_plan(cx);
                        }
                    }
                }
                this.refresh_status(cx);
                this.load_cleanup_history(true, cx);
            },
        );
        cx.notify();
    }

    fn cache_busy_text(&self, busy: &str) -> String {
        let state = match busy {
            "starting" => self.t("啟動中", "starting"),
            "stopping" => self.t("停止中", "stopping"),
            _ => self.t("清除中", "clearing"),
        };
        format!(
            "{}{}{}",
            self.t("服務", "The service is "),
            state,
            self.t("，完成前無法使用清除操作。", "; clear actions are unavailable until it finishes.")
        )
    }

    pub fn render_cache(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pal = Pal::new(cx);
        let viewport = window.viewport_size();
        let narrow = viewport.width < px(1320.);
        let root = v_flex().id("cache-page").size_full().overflow_y_scroll().text_size(px(14.)).text_color(pal.text);

        let Some(id) = self.cache_target() else {
            return root
                .child(card(&pal).child(empty_small(
                    &pal,
                    self.t("請先儲存服務，並在側欄選擇後再檢視其快取。", "Save a service first, then pick it in the sidebar to see its cache."),
                )))
                .into_any_element();
        };
        let Some(svc) = self.status.as_ref().and_then(|s| s.services.iter().find(|s| s.id == id)).cloned() else {
            return root.into_any_element();
        };
        let serve = svc.config.mode == "serve";
        let service_busy = svc.busy.clone();
        let busy_lock = service_busy.is_some() || self.busy || self.cache.clearing;
        let local_paths = self.cache_local_paths();
        let picked = self.cache_effective_picked();
        let reviewing = self.cache.review.is_some();

        // ── Distribution ───────────────────────────────────────────────────────────────────────
        let scanning = self.cache.scanning;
        let mut dist = card(&pal).child(
            h_flex()
                .justify_between()
                .items_center()
                .gap(px(12.))
                .child(h2(&pal, self.t("快取資料分布", "Cache distribution")))
                .child(btn(
                    &pal,
                    "cache-rescan",
                    if scanning { self.t("掃描中…", "Scanning…") } else { self.t("重新掃描用量", "Rescan usage") },
                    BtnKind::Plain,
                    true,
                    scanning || busy_lock,
                    cx.listener(|this, _, _, cx| this.scan_usage(true, cx)),
                )),
        );
        if let Some(err) = &self.cache.usage_error {
            dist = dist.child(banner(&pal, true, format!("{}{}", self.t("無法掃描用量：", "Couldn’t scan usage: "), err)));
        }
        match &self.cache.usage {
            None if self.cache.usage_error.is_none() => {
                dist = dist.child(empty_small(&pal, self.t("掃描用量中…", "Scanning usage…")));
            }
            None => {}
            Some(u) => {
                let total = u.total_bytes.max(1) as f32;
                let parts = [
                    (self.t("CAS objects", "CAS objects"), "objects/", &u.objects, pal.accent),
                    (self.t("Action cache", "Action cache"), "kv/", &u.kv, pal.purple),
                    (self.t("暫存寫入", "Temporary writes"), "tmp/", &u.tmp, pal.blue),
                ];
                dist = dist
                    .child(
                        h_flex()
                            .items_baseline()
                            .child(div().text_size(px(26.)).font_weight(FontWeight(650.)).child(format_bytes(u.total_bytes)))
                            .child(div().text_color(pal.muted).child(format!(" · {} {}", format_int(u.total_files), self.t("個檔案", "files")))),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .h(px(12.))
                            .rounded(px(6.))
                            .overflow_hidden()
                            .bg(pal.track)
                            .children(parts.iter().map(|(_, _, part, color)| div().h_full().w(relative(part.bytes as f32 / total)).bg(*color))),
                    )
                    .child(v_flex().gap(px(10.)).children(parts.iter().map(|(name, dir, part, color)| {
                        h_flex()
                            .gap(px(12.))
                            .items_center()
                            .px(px(12.))
                            .py(px(10.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(pal.border)
                            .child(div().size(px(10.)).rounded(px(3.)).flex_none().bg(*color))
                            .child(div().font_weight(FontWeight::BOLD).child(*name))
                            .child(mono(&pal, 12.5).text_color(pal.muted).child(*dir))
                            .child(div().flex_1())
                            .child(div().child(format!("{} {}", format_int(part.files), self.t("個", "files"))))
                            .child(mono(&pal, 12.5).min_w(px(90.)).text_right().child(format_bytes(part.bytes)))
                    })))
                    .child(
                        kv_box(9., 13.)
                            .child(kv(
                                &pal,
                                96.,
                                self.t("紀錄（獨立保存）", "Records (stored separately)"),
                                format!("{} · {} {}", format_bytes(u.records.bytes), format_int(u.records.files), self.t("個檔案", "files")),
                            ))
                            .child(kv(
                                &pal,
                                96.,
                                self.t("磁碟可用", "Disk available"),
                                format!("{} / {}", format_bytes(u.disk_available), format_bytes(u.disk_total)),
                            ))
                            .child(kv(&pal, 96., self.t("掃描時間", "Scanned at"), format_time(u.scanned_at))),
                    );
            }
        }
        dist = dist.child(note(
            &pal,
            self.t(
                "用量以掃描時的磁碟佔用為準；與累計上傳／下載流量分別計算。",
                "Usage reflects disk space at scan time and is counted separately from cumulative upload/download traffic.",
            ),
        ));

        // ── Manual clear ───────────────────────────────────────────────────────────────────────
        let usage_text = match &self.cache.usage {
            Some(u) => format!("{}{}", self.t("約 ", "About "), format_bytes(u.total_bytes)),
            None => self.t("用量掃描中", "Scanning usage").to_string(),
        };
        let mut clear = card(&pal)
            .child(
                v_flex()
                    .child(h2(&pal, self.t("手動清除", "Manual clear")))
                    .child(note(&pal, self.t("先選範圍，再查看完整路徑與影響；不會在此直接刪除。", "Choose a scope, then review full paths and impact. Nothing is deleted here."))),
            )
            .child(note(
                &pal,
                if serve {
                    self.t("清除會刪除此 Server 的 store。", "Clearing deletes this Server’s store.")
                } else {
                    self.t(
                        "清除只影響此 Proxy 的本機快取 store；上游資料保留。遠端管理目前不提供。",
                        "Clearing affects only this Proxy’s local cache store; the upstream keeps its data. Remote management isn’t available.",
                    )
                },
            ))
            .child(
                scope_box(&pal, true)
                    .child(locked_check(
                        &pal,
                        "cache-scope-store",
                        if serve { self.t("Server 快取", "Server cache") } else { self.t("Proxy 本機快取", "Proxy local cache") },
                        div().font_weight(FontWeight::BOLD).child(if serve { self.t("Server 快取", "Server cache") } else { self.t("Proxy 本機快取", "Proxy local cache") }),
                    ))
                    .child(note(
                        &pal,
                        format!("objects/ + kv/ + tmp/ · {} · {}", usage_text, self.t("CAS 與 KV 一併清空", "CAS and KV are cleared together")),
                    )),
            );
        let mut paths_box = scope_box(&pal, false).child(
            div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(pal.muted).child(self.t("本機建置快取與產物（選填）", "Local build cache and artifacts (optional)")),
        );
        if local_paths.is_empty() {
            paths_box = paths_box.child(note(
                &pal,
                self.t(
                    "尚未設定本機路徑。可到「服務設定」的「本機建置快取路徑」新增。",
                    "No local paths configured. Add one under “Service settings” › “Local build cache paths”.",
                ),
            ));
        } else {
            for (i, path) in local_paths.iter().enumerate() {
                let p = path.clone();
                let on = picked.contains(path);
                paths_box = paths_box.child(check(
                    &pal,
                    SharedString::from(format!("cache-path-{i}")),
                    on,
                    busy_lock || reviewing,
                    path.clone(),
                    mono(&pal, 12.5).child(path.clone()),
                    cx.listener(move |this, v: &bool, _, cx| this.cache_toggle_path(p.clone(), *v, cx)),
                ));
            }
        }
        clear = clear.child(paths_box).child(note(
            &pal,
            self.t(
                "保留：事件紀錄、傳輸明細、已保存的 payload 與服務設定。若勾選本機建置項目，下次建置會重新產生對應產物。",
                "Kept: event log, transfer details, saved payloads, and service settings. If you select local build items, the next build regenerates them.",
            ),
        ));
        if let Some(busy) = &service_busy {
            clear = clear.child(banner(&pal, false, self.cache_busy_text(busy)));
        }
        if let Some(r) = &self.cache.last_result {
            clear = clear.child(banner(
                &pal,
                false,
                format!(
                    "{}{}{}{}{}{}{}",
                    self.t("上次清除：回收 ", "Last clear: reclaimed "),
                    format_bytes(r.reclaimed_bytes),
                    self.t("（", " ("),
                    format_int(r.removed_files),
                    self.t(" 個檔案），", " files); service "),
                    if r.restarted { self.t("服務已重啟", "restarted") } else { self.t("服務保持停止", "left stopped") },
                    self.t("。", "."),
                ),
            ));
        }
        clear = clear.child(h_flex().justify_end().child(btn(
            &pal,
            "cache-review",
            self.t("檢視範圍並清除…", "Review scope and clear…"),
            BtnKind::Danger,
            false,
            busy_lock || reviewing,
            cx.listener(|this, _, window, cx| this.open_clear_review(window, cx)),
        )));

        // ── Mechanism ──────────────────────────────────────────────────────────────────────────
        let unimplemented = self.t("尚未實作", "Not implemented yet");
        let mechanism = card(&pal)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap(px(12.))
                    .child(h2(&pal, self.t("清除機制", "Clearing mechanism")))
                    .child(
                        div()
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(6.))
                            .whitespace_nowrap()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .bg(pal.accent_bg)
                            .text_color(pal.accent)
                            .child(self.t("目前：手動清除", "Current: manual clear")),
                    ),
            )
            .child(
                kv_box(9., 13.)
                    .child(kv(&pal, 96., self.t("自動清理", "Automatic cleanup"), self.t("未提供", "Not available")))
                    .child(kv(&pal, 96., self.t("容量上限 / quota", "Size limit / quota"), unimplemented))
                    .child(kv(&pal, 96., self.t("過期時間 / TTL", "Expiry / TTL"), unimplemented))
                    .child(kv(&pal, 96., self.t("淘汰策略 / LRU、GC", "Eviction policy / LRU, GC"), unimplemented))
                    .child(kv(
                        &pal,
                        96.,
                        self.t("遠端管理", "Remote management"),
                        self.t("不提供（gRPC 僅含 CAS / KV）", "Not available (gRPC covers CAS / KV only)"),
                    )),
            )
            .child(note(
                &pal,
                self.t(
                    "UI 清除流程：暫停服務、等待請求完成後再清空。可選擇完成後重啟，恢復接收快取請求。",
                    "Clearing from the UI: pause the service, wait for requests to finish, then clear. You can restart afterward to resume accepting cache requests.",
                ),
            ));

        // ── Layout: .cache-grid (left fluid, right 400px, gap 18; single column below 1320px) ───
        let left = v_flex().gap(px(18.)).min_w(px(0.)).child(dist).child(clear);
        let right = v_flex().gap(px(18.)).min_w(px(0.)).child(mechanism).child(self.render_cleanup_history(&pal, cx));
        let grid = if narrow {
            v_flex().gap(px(18.)).child(left).child(right)
        } else {
            h_flex().items_start().gap(px(18.)).child(left.flex_1()).child(right.w(px(400.)).flex_none())
        };
        root.child(grid).into_any_element()
    }

    /// Body of the clear-confirmation dialog (Tauri `ClearDialog`), rendered by `ClearReviewView` inside a native
    /// focus-trapped dialog (Escape/backdrop → `on_cancel`, which refuses while a clear is in flight).
    fn render_clear_review(&self, pal: &Pal, ws: &Entity<Workspace>) -> Div {
        let Some(review) = self.cache.review.as_ref() else { return div() };
        let svc = self.cache_target().and_then(|id| self.status.as_ref().and_then(|s| s.services.iter().find(|s| s.id == id)));
        let serve = svc.is_none_or(|s| s.config.mode == "serve");
        let running = svc.is_some_and(|s| s.running);
        let service_busy = svc.and_then(|s| s.busy.as_deref());
        let clearing = self.cache.clearing;
        let is_running = review.plan.as_ref().map_or(running, |p| p.running);
        let act = |f: fn(&mut Workspace, &mut Context<Workspace>)| {
            let w = ws.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                w.update(cx, |this, cx| f(this, cx));
            }
        };
        let mut body = v_flex()
            .gap(px(14.))
            .child(
                div().text_size(px(22.)).font_weight(FontWeight(650.)).child(if serve {
                    self.t("清空 Server 快取？", "Clear the Server cache?")
                } else {
                    self.t("清空 Proxy 本機快取？", "Clear the Proxy local cache?")
                }),
            )
            .child(div().text_color(pal.muted).child(if serve {
                self.t("將刪除 CAS 物件與 action cache。下次查詢可能未命中並重新上傳。", "This deletes CAS objects and the action cache. The next lookup may miss and re-upload.")
            } else {
                self.t(
                    "僅刪除此 Proxy 的本機 CAS 物件與 action cache；上游資料保留，之後的查詢會再從上游讀取。",
                    "This deletes only this Proxy’s local CAS objects and action cache. The upstream keeps its data; later lookups are read through from it again.",
                )
            }));
        if let Some(err) = &review.plan_error {
            body = body
                .child(banner(pal, true, format!("{}{}", self.t("無法取得清除計畫：", "Couldn’t get the clear plan: "), err)))
                .child(h_flex().child(btn(
                    pal,
                    "cache-plan-retry",
                    self.t("重新載入計畫", "Reload plan"),
                    BtnKind::Plain,
                    true,
                    clearing,
                    act(Workspace::fetch_clear_plan),
                )));
        }
        match &review.plan {
            None if review.plan_error.is_none() => {
                body = body.child(empty_small(pal, self.t("計算清除範圍…", "Calculating scope…")));
            }
            None => {}
            Some(plan) => {
                let kept = self.t("事件紀錄、傳輸明細、已保存的 payload、設定", "Event log, transfer details, saved payloads, settings");
                body = body.child(
                    kv_box(14., 13.)
                        .child(kv(
                            pal,
                            84.,
                            self.t("刪除範圍", "To be deleted"),
                            v_flex().child(mono(pal, 12.5).child(plan.store.clone())).child(mono_list(pal, &plan.paths)),
                        ))
                        .child(kv(
                            pal,
                            84.,
                            self.t("預計回收", "Estimated reclaim"),
                            v_flex()
                                .child(format!("{}{}", self.t("約 ", "About "), format_bytes(plan.estimated_bytes)))
                                .child(note(
                                    pal,
                                    self.t(
                                        "此為目前快照估計；最終回收容量以服務停止後的清除結果為準。",
                                        "Based on a current snapshot; the final reclaimed size is the result reported after the service stops.",
                                    ),
                                )),
                        ))
                        .child(kv(
                            pal,
                            84.,
                            self.t("本機路徑", "Local paths"),
                            if review.paths.is_empty() {
                                div().text_color(pal.muted).child(self.t("未選擇（不會刪除任何本機建置快取）", "None selected (no local build cache will be deleted)"))
                            } else {
                                mono_list(pal, &review.paths)
                            },
                        ))
                        .child(kv(pal, 84., self.t("保留資料", "Kept data"), kept)),
                );
            }
        }
        let step = |n: usize, text: String| {
            h_flex()
                .items_start()
                .child(div().w(px(20.)).flex_none().child(format!("{n}.")))
                .child(div().flex_1().min_w(px(0.)).child(text))
        };
        let mut steps = v_flex().mt(px(6.));
        let mut n = 1;
        if is_running {
            steps = steps.child(step(n, self.t("停止接收新請求，等待正在處理的請求完成", "Stop accepting new requests and wait for in-flight requests to finish").to_string()));
            n += 1;
        }
        steps = steps.child(step(
            n,
            format!(
                "{}{}",
                self.t("一併清空 objects、kv 與暫存資料", "Clear objects, kv, and temporary data"),
                if review.paths.is_empty() { "" } else { self.t("，以及所選本機路徑", ", plus the selected local paths") },
            ),
        ));
        steps = steps.child(step(
            n + 1,
            if review.restart {
                self.t("寫入清除結果；依下方選項重新啟動服務", "Record the result; then restart the service per the option below").to_string()
            } else {
                self.t("寫入清除結果；依下方選項保持服務停止", "Record the result; then leave the service stopped per the option below").to_string()
            },
        ));
        body = body
            .child(
                v_flex().px(px(14.)).py(px(12.)).rounded(px(8.)).bg(pal.warn_bg).text_size(px(13.)).child(
                    div().font_weight(FontWeight::BOLD).child(if is_running {
                        self.t("清除期間此服務暫時無法處理快取請求", "This service can’t handle cache requests while clearing")
                    } else {
                        self.t("服務目前已停止", "The service is currently stopped")
                    }),
                )
                .child(steps),
            )
            .child(check(
                pal,
                "cache-restart",
                review.restart,
                clearing,
                self.t("完成後重新啟動服務", "Restart the service when done"),
                self.t("完成後重新啟動服務", "Restart the service when done"),
                {
                    let w = ws.clone();
                    move |_: &bool, _, cx| {
                        w.update(cx, |this, cx| {
                            if this.cache.clearing {
                                return;
                            }
                            if let Some(review) = this.cache.review.as_mut() {
                                review.restart = !review.restart;
                                cx.notify();
                            }
                        });
                    }
                },
            ))
            .child(note(pal, self.t("取消不會修改資料。清除結果與回收容量會保留在事件紀錄中。", "Canceling doesn’t change any data. The result and reclaimed size are kept in the event log.")));
        if let Some(busy) = service_busy.filter(|_| !clearing) {
            body = body.child(banner(pal, false, self.cache_busy_text(busy)));
        }
        if let Some(err) = &review.error {
            let state = if review.plan.as_ref().map_or(running, |p| p.running) {
                self.t("服務目前運行中。", "The service is currently running.")
            } else {
                self.t("服務目前已停止，詳情見事件紀錄。", "The service is currently stopped; see the event log for details.")
            };
            body = body.child(banner(pal, true, format!("{}{}{} {}", self.t("清除失敗：", "Clear failed: "), err, self.t("。", "."), state)));
        }
        let blocked = service_busy.is_some() && !clearing;
        let label = if clearing {
            self.t("清除中…", "Clearing…")
        } else if review.error.is_some() {
            self.t("重試清除", "Retry clear")
        } else if is_running {
            self.t("停止服務並清空快取", "Stop service and clear cache")
        } else {
            self.t("清空快取", "Clear cache")
        };
        body = body.child(
            h_flex()
                .gap(px(8.))
                .justify_end()
                .child(btn(pal, "cache-cancel", self.t("取消", "Cancel"), BtnKind::Plain, false, clearing, act(Workspace::cancel_clear_review)))
                .child(btn(
                    pal,
                    "cache-confirm",
                    label,
                    BtnKind::Solid,
                    false,
                    clearing || review.plan.is_none() || blocked,
                    act(Workspace::confirm_clear),
                )),
        );
        body
    }

    fn render_cleanup_history(&self, pal: &Pal, cx: &mut Context<Self>) -> Div {
        let mut head = h_flex().justify_between().items_center().gap(px(12.)).child(h2(pal, self.t("最近清除紀錄", "Recent clears")));
        if let Some(history) = self.cache.history.as_ref().filter(|h| h.len() > COLLAPSED_HISTORY) {
            head = head.child(link(
                pal,
                "cache-history-all",
                if self.cache.show_all {
                    self.t("收合", "Collapse").to_string()
                } else {
                    format!("{}（{}）→", self.t("查看全部", "View all"), history.len())
                },
                cx.listener(|this, _, _, cx| {
                    this.cache.show_all = !this.cache.show_all;
                    cx.notify();
                }),
            ));
        }
        let mut section = card(pal).child(head);
        if let Some(err) = &self.cache.history_error {
            section = section.child(banner(pal, true, err.clone()));
        }
        match &self.cache.history {
            None if self.cache.history_error.is_none() => section = section.child(empty_small(pal, self.t("載入中…", "Loading…"))),
            None => {}
            Some(items) if items.is_empty() => section = section.child(empty_small(pal, self.t("尚無清除紀錄。", "No clears yet."))),
            Some(items) => {
                let shown = if self.cache.show_all { items.len() } else { items.len().min(COLLAPSED_HISTORY) };
                for ev in &items[..shown] {
                    section = section.child(self.render_cleanup_item(pal, ev, cx));
                }
            }
        }
        section.child(note(
            pal,
            self.t(
                "每次清除記錄時間、範圍、結果與回收容量。清除失敗時保留原因，並標示服務目前狀態。",
                "Each clear records its time, scope, result, and reclaimed size. If a clear fails, the reason is kept along with the current service state.",
            ),
        ))
    }

    fn render_cleanup_item(&self, pal: &Pal, ev: &AppEvent, cx: &mut Context<Self>) -> Div {
        let failed = ev.level == "ERROR" || ev.level == "WARN" || field(ev, "ok").and_then(Value::as_bool) == Some(false);
        let expanded = self.cache.expanded.contains(&ev.id);
        let event_id = ev.id;
        let reclaimed = field(ev, "reclaimed_bytes").and_then(Value::as_u64);
        let removed = field(ev, "removed_files").and_then(Value::as_u64);
        let restarted = field(ev, "restarted").and_then(Value::as_bool);
        let paths: Vec<String> = field(ev, "local_paths")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect())
            .unwrap_or_default();
        let error = field(ev, "error").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        let reclaimed_text = match (reclaimed, removed) {
            (Some(b), Some(f)) => format!("{} · {} {}", format_bytes(b), format_int(f), self.t("個檔案", "files")),
            (Some(b), None) => format_bytes(b),
            _ => "—".to_string(),
        };
        let outcome_text = match restarted {
            Some(true) => self.t("完成後已重啟服務", "Service restarted afterward"),
            Some(false) => self.t("完成後保持停止", "Service left stopped afterward"),
            None => "—",
        };
        let (tag_bg, tag_fg) = if failed { (pal.danger_bg, pal.danger) } else { (pal.accent_bg, pal.accent) };
        let tag = div()
            .flex_none()
            .px(px(8.))
            .py(px(2.))
            .rounded(px(5.))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .bg(tag_bg)
            .text_color(tag_fg)
            .child(if failed { self.t("失敗", "Failed") } else { self.t("成功", "Succeeded") });
        let item = v_flex().gap(px(8.)).p(px(12.)).rounded(px(8.)).border_1().border_color(pal.border).child(
            h_flex()
                .gap(px(8.))
                .items_center()
                .child(tag)
                .child(div().flex_1().min_w(px(0.)).text_size(px(12.)).text_color(pal.muted).child(format!("{} · {}", format_time(ev.time), ev.message)))
                .child(link(
                    pal,
                    SharedString::from(format!("cache-hist-toggle-{event_id}")),
                    if expanded { self.t("收合", "Collapse") } else { self.t("展開", "Details") },
                    cx.listener(move |this, _, _, cx| {
                        if !this.cache.expanded.remove(&event_id) {
                            this.cache.expanded.insert(event_id);
                        }
                        cx.notify();
                    }),
                )),
        );
        let mut rows = kv_box(5., 12.5)
            .child(kv(pal, 96., self.t("實際回收", "Reclaimed"), reclaimed_text))
            .child(kv(pal, 96., self.t("操作結果", "Result"), outcome_text));
        if !paths.is_empty() {
            rows = rows.child(kv(pal, 96., self.t("本機路徑", "Local paths"), mono(pal, 12.5).child(paths.join(self.t("、", ", ")))));
        }
        if let Some(err) = &error {
            rows = rows.child(kv(pal, 96., self.t("錯誤", "Error"), div().text_color(pal.danger).child(err.clone())));
        }
        if expanded {
            let scope = ev
                .service_id
                .as_ref()
                .map(|sid| {
                    self.status
                        .as_ref()
                        .and_then(|s| s.services.iter().find(|s| &s.id == sid))
                        .map(|s| format!("{} ({})", service_label(&s.config), sid))
                        .unwrap_or_else(|| sid.clone())
                })
                .unwrap_or_else(|| self.t("（未標示服務）", "(no service recorded)").to_string());
            rows = rows
                .child(kv(pal, 96., self.t("服務範圍", "Service scope"), scope))
                .child(kv(pal, 96., self.t("等級", "Level"), ev.level.clone()))
                .child(kv(pal, 96., self.t("事件", "Event"), format!("#{} · {}", ev.id, ev.kind)))
                .child(kv(
                    pal,
                    96.,
                    self.t("完整欄位", "All fields"),
                    mono(pal, 12.).child(serde_json::to_string_pretty(&ev.fields).unwrap_or_default()),
                ))
                .child(h_flex().child(btn(
                    pal,
                    SharedString::from(format!("cache-hist-open-{event_id}")),
                    self.t("在事件頁查看", "View in Events"),
                    BtnKind::Plain,
                    true,
                    false,
                    cx.listener(move |this, _, _, cx| this.open_event(event_id, cx)),
                )));
        }
        item.child(rows)
    }
}

/// Content of the native clear-confirmation dialog. Observes the workspace so plan/progress/error changes redraw it,
/// and closes the dialog (once) when the review ends, whichever path ended it (cancel, success, service switch).
struct ClearReviewView {
    workspace: Entity<Workspace>,
    closed: bool,
    _observe: Subscription,
}

impl ClearReviewView {
    fn new(workspace: Entity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe_in(&workspace, window, |this, ws, window, cx| {
            if this.closed || ws.read(cx).cache.review.is_some() {
                return;
            }
            this.closed = true;
            window.close_dialog(cx);
        });
        ClearReviewView { workspace, closed: false, _observe }
    }
}

impl Render for ClearReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = Pal::new(cx);
        self.workspace.read(cx).render_clear_review(&pal, &self.workspace)
    }
}

pub fn smoke_snapshot(state: &CacheState) -> Value {
    json!({"usage":state.usage,"error":state.usage_error,"clearing":state.clearing,"review":state.review.as_ref().map(|r|json!({"plan":r.plan,"plan_error":r.plan_error,"error":r.error,"restart":r.restart})),"result":state.last_result,"history":state.history})
}
