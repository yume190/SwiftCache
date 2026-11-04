import { invoke } from '@tauri-apps/api/core';
import { useEffect } from 'react';
import { inTauri } from './api';
import { resolveLocale, type LanguagePref, type Locale } from './i18n';

export type Theme = 'system' | 'light' | 'dark';

// Mirrors `Prefs` in src-tauri/src/prefs.rs (persisted as desktop.json in the data dir).
export interface Prefs {
  theme: Theme;
  language: LanguagePref;
  /** `language` resolved here; the menu bar (Rust) labels itself from it. */
  locale: Locale;
  menu_bar_title: boolean;
}

const BROWSER_KEY = 'swiftcache.prefs';

export function defaultPrefs(): Prefs {
  return { theme: 'system', language: 'system', locale: resolveLocale('system'), menu_bar_title: true };
}

export async function loadPrefs(): Promise<Prefs> {
  const stored = inTauri
    ? await invoke<Prefs>('get_prefs')
    : (JSON.parse(localStorage.getItem(BROWSER_KEY) ?? 'null') as Partial<Prefs> | null);
  const prefs = { ...defaultPrefs(), ...stored };
  // The system language may have changed since the last launch.
  return { ...prefs, locale: resolveLocale(prefs.language) };
}

export async function savePrefs(prefs: Prefs): Promise<void> {
  if (inTauri) await invoke('set_prefs', { prefs });
  else localStorage.setItem(BROWSER_KEY, JSON.stringify(prefs));
}

const darkQuery = () => window.matchMedia('(prefers-color-scheme: dark)');

/** Sets `<html data-theme>` (styles.css keys the dark palette off it), tracking the OS for "system". */
export function applyTheme(theme: Theme) {
  document.documentElement.dataset.theme = theme === 'system' ? (darkQuery().matches ? 'dark' : 'light') : theme;
}

export function useTheme(theme: Theme) {
  useEffect(() => {
    applyTheme(theme);
    if (theme !== 'system') return;
    const query = darkQuery();
    const onChange = () => applyTheme('system');
    query.addEventListener('change', onChange);
    return () => query.removeEventListener('change', onChange);
  }, [theme]);
}
