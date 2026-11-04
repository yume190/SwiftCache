import { Check, Modal } from './components';
import { defineMessages, useMessages, type LanguagePref } from './i18n';
import type { Prefs, Theme } from './prefs';

const M = defineMessages(
  {
    title: '設定',
    hint: '⌘, 開啟 · Esc 或 × 關閉 · 變更立即生效',
    close: '關閉設定',
    appearance: '外觀',
    system: '跟隨系統',
    light: '淺色',
    dark: '深色',
    language: '語言',
    languageNote: '套用到視窗與選單列。服務端產生的訊息（事件內容、錯誤）維持原文。',
    menuBar: '選單列',
    menuBarTitle: '在圖示旁顯示快取用量',
    menuBarNote: '關閉後只顯示圖示；用量與狀態仍可在下拉選單查看。',
    saveFailed: (e: string) => `儲存設定失敗：${e}`,
  },
  {
    title: 'Settings',
    hint: '⌘, to open · Esc or × to close · Changes apply immediately',
    close: 'Close settings',
    appearance: 'Appearance',
    system: 'System',
    light: 'Light',
    dark: 'Dark',
    language: 'Language',
    languageNote: 'Applies to the window and the menu bar. Messages produced by the service (event text, errors) stay as written.',
    menuBar: 'Menu bar',
    menuBarTitle: 'Show cache usage next to the icon',
    menuBarNote: 'When off, only the icon is shown; usage and status stay in the dropdown.',
    saveFailed: (e: string) => `Couldn't save settings: ${e}`,
  },
);

const THEMES: Theme[] = ['system', 'light', 'dark'];
// Language names are shown in their own language, as macOS does.
const LANGUAGES: { id: LanguagePref; name?: string }[] = [{ id: 'system' }, { id: 'zh-TW', name: '繁體中文' }, { id: 'en', name: 'English' }];

export function Preferences({ prefs, error, onChange, onClose }: { prefs: Prefs; error: string | null; onChange: (patch: Partial<Prefs>) => void; onClose: () => void }) {
  const t = useMessages(M);
  return (
    <Modal title={t.title} onClose={onClose}>
      <div className="sheet-head">
        <div>
          <h2>{t.title}</h2>
          <p className="muted small">{t.hint}</p>
        </div>
        <button type="button" className="icon-btn" aria-label={t.close} onClick={onClose}>
          ×
        </button>
      </div>
      <div className="modal-body prefs">
        {error && <p className="danger-text small">{t.saveFailed(error)}</p>}
        <div className="pref-row">
          <span className="label">{t.appearance}</span>
          <div className="seg">
            {THEMES.map((theme) => (
              <button type="button" key={theme} className={prefs.theme === theme ? 'on' : ''} aria-pressed={prefs.theme === theme} onClick={() => onChange({ theme })}>
                {t[theme]}
              </button>
            ))}
          </div>
        </div>
        <div className="pref-row">
          <span className="label">{t.language}</span>
          <div>
            <div className="seg">
              {LANGUAGES.map((l) => (
                <button type="button" key={l.id} className={prefs.language === l.id ? 'on' : ''} aria-pressed={prefs.language === l.id} onClick={() => onChange({ language: l.id })}>
                  {l.name ?? t.system}
                </button>
              ))}
            </div>
            <p className="muted small">{t.languageNote}</p>
          </div>
        </div>
        <div className="pref-row">
          <span className="label">{t.menuBar}</span>
          <div>
            <Check checked={prefs.menu_bar_title} onChange={(menu_bar_title) => onChange({ menu_bar_title })}>
              {t.menuBarTitle}
            </Check>
            <p className="muted small">{t.menuBarNote}</p>
          </div>
        </div>
      </div>
    </Modal>
  );
}
