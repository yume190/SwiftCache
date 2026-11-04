import { createContext, useContext, type ReactNode } from 'react';

export type Locale = 'zh-TW' | 'en';
export type LanguagePref = 'system' | Locale;

type Message = string | ((...args: never[]) => string);
type Catalog = Record<string, Message>;
export type Messages<M extends Catalog> = Record<Locale, M>;

/**
 * Per-module message catalog. `en` must carry exactly the keys (and
 * interpolation signatures) of `zh`, so a missing translation is a type error.
 */
export function defineMessages<M extends Catalog>(zh: M, en: { [K in keyof M]: M[K] }): Messages<M> {
  return { 'zh-TW': zh, en };
}

function systemLocale(): Locale {
  const langs = typeof navigator === 'undefined' ? [] : navigator.languages?.length ? navigator.languages : [navigator.language];
  return langs[0]?.toLowerCase().startsWith('zh') ? 'zh-TW' : 'en';
}

export const resolveLocale = (pref: LanguagePref): Locale => (pref === 'system' ? systemLocale() : pref);

// Mirrors the provider value for code outside React (api.ts error text, validate()).
let current: Locale = systemLocale();

const LocaleContext = createContext<Locale>(current);

export function LocaleProvider({ locale, children }: { locale: Locale; children: ReactNode }) {
  current = locale;
  return <LocaleContext.Provider value={locale}>{children}</LocaleContext.Provider>;
}

export function useMessages<M extends Catalog>(messages: Messages<M>): M {
  return messages[useContext(LocaleContext)];
}

/** Non-hook access for plain functions; components should use `useMessages`. */
export const pick = <M extends Catalog>(messages: Messages<M>): M => messages[current];
