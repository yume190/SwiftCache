//! Desktop-only preferences (appearance, language, menu bar title).
//!
//! Persisted as `desktop.json` next to the daemon's data; the daemon never reads it.
//! `language` is the user's choice; `locale` is that choice resolved (so "follow system"
//! is looked up from the OS) and persisted for consumers that cannot resolve it.

use std::fs;

use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme, Theme, ThemeMode, WindowExt};
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use crate::api::data_dir;
use crate::app::Workspace;
use crate::platform;

const FILE: &str = "desktop.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "zh-TW")]
    ZhTw,
    #[serde(rename = "en")]
    En,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Locale {
    #[default]
    #[serde(rename = "zh-TW")]
    ZhTw,
    #[serde(rename = "en")]
    En,
}

impl Locale {
    pub fn pick(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Locale::ZhTw => zh,
            Locale::En => en,
        }
    }

    /// Identifier understood by the component library's own strings.
    pub fn code(self) -> &'static str {
        match self {
            Locale::ZhTw => "zh-TW",
            Locale::En => "en",
        }
    }
}

/// The OS preferred language: any Chinese variant maps to zh-TW, everything else to English.
fn system_locale() -> Locale {
    match sys_locale::get_locales().next() {
        Some(tag) if tag.to_ascii_lowercase().starts_with("zh") => Locale::ZhTw,
        _ => Locale::En,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub theme: ThemePref,
    pub language: Language,
    /// `language` resolved.
    pub locale: Locale,
    /// Show the store size (or service state) next to the menu bar icon.
    pub menu_bar_title: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { theme: ThemePref::System, language: Language::System, locale: system_locale(), menu_bar_title: true }
    }
}

impl Prefs {
    pub fn t(&self, zh: &'static str, en: &'static str) -> &'static str {
        self.locale.pick(zh, en)
    }

    fn resolve(&mut self) {
        self.locale = match self.language {
            Language::System => system_locale(),
            Language::ZhTw => Locale::ZhTw,
            Language::En => Locale::En,
        };
    }
}

/// Reads `desktop.json`; a missing or unreadable file yields the defaults.
pub fn load_prefs() -> Prefs {
    let mut prefs: Prefs = data_dir()
        .ok()
        .and_then(|dir| fs::read(dir.join(FILE)).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    prefs.resolve();
    prefs
}

fn save_prefs(prefs: &Prefs) -> Result<(), String> {
    let dir = data_dir()?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_vec_pretty(prefs).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("{FILE}.tmp"));
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, dir.join(FILE)).map_err(|e| e.to_string())
}

pub struct SettingsState {
    pub prefs: Prefs,
    /// Last failure to persist; shown in the preferences dialog.
    pub save_error: Option<String>,
}

impl SettingsState {
    pub fn new() -> Self {
        SettingsState { prefs: load_prefs(), save_error: None }
    }

    /// Applies the preferences to the process: Kit theme, component locale, native app menu.
    /// Cheap and idempotent — the root calls it before every render.
    pub fn apply(&self, cx: &mut App) {
        let mode = match self.prefs.theme {
            ThemePref::Light => ThemeMode::Light,
            ThemePref::Dark => ThemeMode::Dark,
            ThemePref::System => ThemeMode::from(cx.window_appearance()),
        };
        if !cx.has_global::<Theme>() || Theme::global(cx).mode != mode {
            Theme::change(mode, None, cx);
        }
        crate::ui::apply(cx);
        let code = self.prefs.locale.code();
        if &*gpui_kit::component::locale() != code {
            gpui_kit::component::set_locale(code);
        }
        platform::set_app_menus(cx, self.prefs.locale);
    }
}

impl Default for SettingsState {
    fn default() -> Self {
        Self::new()
    }
}

const THEMES: [ThemePref; 3] = [ThemePref::System, ThemePref::Light, ThemePref::Dark];
const LANGUAGES: [Language; 3] = [Language::System, Language::ZhTw, Language::En];

impl Workspace {
    /// Edits the preferences, persists them to `desktop.json` and applies them at once:
    /// theme, component/menu locale and the menu bar item.
    pub fn set_prefs(&mut self, change: impl FnOnce(&mut Prefs), cx: &mut Context<Self>) {
        let mut next = self.settings.prefs.clone();
        change(&mut next);
        next.resolve();
        self.settings.save_error = save_prefs(&next).err();
        self.settings.prefs = next;
        self.settings.apply(cx);
        self.platform.relabel(&self.settings.prefs);
        cx.notify();
    }

    /// Opens the preferences dialog (changes apply and persist immediately).
    pub fn open_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let workspace = cx.entity();
        let view = cx.new(|cx| PreferencesView::new(workspace.clone(), cx));
        window.open_dialog(cx, move |dialog, _window, cx| {
            let title = workspace.read(cx).settings.prefs.t("設定", "Settings");
            dialog.title(title).w(px(520.)).child(view.clone())
        });
    }
}

struct PreferencesView {
    workspace: Entity<Workspace>,
    _observe: Subscription,
}

impl PreferencesView {
    fn new(workspace: Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&workspace, |_, _, cx| cx.notify());
        PreferencesView { workspace, _observe }
    }
}

impl Render for PreferencesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (prefs, save_error) = {
            let ws = self.workspace.read(cx);
            (ws.settings.prefs.clone(), ws.settings.save_error.clone())
        };
        let t = |zh: &'static str, en: &'static str| prefs.t(zh, en);
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;

        let theme_group = {
            let workspace = self.workspace.clone();
            RadioGroup::horizontal("pref-theme")
                .selected_index(THEMES.iter().position(|v| *v == prefs.theme))
                .children([
                    Radio::new("theme-system").label(t("跟隨系統", "System")),
                    Radio::new("theme-light").label(t("淺色", "Light")),
                    Radio::new("theme-dark").label(t("深色", "Dark")),
                ])
                .on_change(move |ix, _, cx| {
                    let theme = THEMES[*ix];
                    workspace.update(cx, |ws, cx| ws.set_prefs(|p| p.theme = theme, cx));
                })
        };

        let language_group = {
            let workspace = self.workspace.clone();
            RadioGroup::horizontal("pref-language")
                .selected_index(LANGUAGES.iter().position(|v| *v == prefs.language))
                .children([
                    Radio::new("lang-system").label(t("跟隨系統", "System")),
                    // Language names are shown in their own language, as macOS does.
                    Radio::new("lang-zh").label("繁體中文"),
                    Radio::new("lang-en").label("English"),
                ])
                .on_change(move |ix, _, cx| {
                    let language = LANGUAGES[*ix];
                    workspace.update(cx, |ws, cx| ws.set_prefs(|p| p.language = language, cx));
                })
        };

        let title_switch = {
            let workspace = self.workspace.clone();
            Switch::new("pref-menu-title")
                .checked(prefs.menu_bar_title)
                .label(t("在圖示旁顯示快取用量", "Show cache usage next to the icon"))
                .on_click(move |checked, _, cx| {
                    let shown = *checked;
                    workspace.update(cx, |ws, cx| ws.set_prefs(|p| p.menu_bar_title = shown, cx));
                })
        };

        let row = |label: &'static str, control: AnyElement, note: Option<&'static str>| {
            h_flex()
                .gap_4()
                .items_start()
                .child(div().w(px(80.)).pt_1().text_sm().font_weight(FontWeight::MEDIUM).child(label))
                .child(
                    v_flex()
                        .gap_1()
                        .flex_1()
                        .child(control)
                        .children(note.map(|note| div().text_xs().text_color(muted).child(note))),
                )
        };

        v_flex()
            .gap_4()
            .children(save_error.map(|e| {
                div().text_sm().text_color(danger).child(format!("{}{e}", t("儲存設定失敗：", "Couldn't save settings: ")))
            }))
            .child(div().text_xs().text_color(muted).child(t(
                "⌘, 開啟 · Esc 關閉 · 變更立即生效",
                "⌘, to open · Esc to close · Changes apply immediately",
            )))
            .child(row(t("外觀", "Appearance"), theme_group.into_any_element(), None))
            .child(row(
                t("語言", "Language"),
                language_group.into_any_element(),
                Some(t(
                    "套用到視窗與選單列。服務端產生的訊息（事件內容、錯誤）維持原文。",
                    "Applies to the window and the menu bar. Messages produced by the service (event text, errors) stay as written.",
                )),
            ))
            .child(row(
                t("選單列", "Menu bar"),
                title_switch.into_any_element(),
                Some(t(
                    "關閉後只顯示圖示；用量與狀態仍可在下拉選單查看。",
                    "When off, only the icon is shown; usage and status stay in the dropdown.",
                )),
            ))
    }
}
