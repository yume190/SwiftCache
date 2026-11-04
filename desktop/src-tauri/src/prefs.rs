//! Desktop-only preferences (appearance, language, menu bar title).
//!
//! Persisted next to the daemon's data as `desktop.json`; the daemon never
//! reads it. The webview resolves "follow system" language (it sees the
//! user's preferred languages) and hands the result back as `locale`, which
//! is also persisted so the menu bar is labelled correctly at the next launch
//! before the webview has loaded.

use std::fs;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::data_dir;
use crate::tray::{self, MAIN_WINDOW};

#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "zh-TW")]
    ZhTw,
    #[serde(rename = "en")]
    En,
}

#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
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
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub theme: Theme,
    pub language: Language,
    /// `language` resolved by the webview.
    pub locale: Locale,
    /// Show the store size (or service state) next to the menu bar icon.
    pub menu_bar_title: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { theme: Theme::System, language: Language::System, locale: Locale::ZhTw, menu_bar_title: true }
    }
}

pub struct PrefsState(pub Mutex<Prefs>);

const FILE: &str = "desktop.json";

pub fn load() -> Prefs {
    data_dir()
        .ok()
        .and_then(|dir| fs::read(dir.join(FILE)).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn current(app: &AppHandle) -> Prefs {
    app.state::<PrefsState>().0.lock().unwrap().clone()
}

/// Pushes preferences to the native surfaces: window chrome and menus.
pub fn apply(app: &AppHandle, prefs: &Prefs) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.set_theme(match prefs.theme {
            Theme::System => None,
            Theme::Light => Some(tauri::Theme::Light),
            Theme::Dark => Some(tauri::Theme::Dark),
        });
    }
    crate::relabel_app_menu(app, prefs.locale);
    tray::relabel(app, prefs.locale);
}

#[tauri::command]
pub fn get_prefs(app: AppHandle) -> Prefs {
    current(&app)
}

#[tauri::command]
pub async fn set_prefs(app: AppHandle, prefs: Prefs) -> Result<(), String> {
    let dir = data_dir()?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(dir.join(FILE), serde_json::to_vec_pretty(&prefs).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    *app.state::<PrefsState>().0.lock().unwrap() = prefs.clone();
    apply(&app, &prefs);
    tray::refresh_now(&app).await;
    Ok(())
}
