import { invoke } from '@tauri-apps/api/core';
import { errorMessage } from './format';
import { defineMessages, pick } from './i18n';

const M = defineMessages(
  {
    devProxy: (e: string) => `無法連線到開發代理：${e}`,
    notJson: '伺服器回應不是 JSON',
    http: (status: number, body: string) => `HTTP ${status}：${body}`,
  },
  {
    devProxy: (e: string) => `Cannot reach the dev proxy: ${e}`,
    notJson: 'The server response is not JSON',
    http: (status: number, body: string) => `HTTP ${status}: ${body}`,
  },
);

export const inTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

type Method = 'GET' | 'POST' | 'PUT' | 'DELETE';

// Desktop: generic `api` Tauri command (bearer token stays in Rust).
// Browser dev: Vite /api proxy (token injected by the proxy).
export async function api<T>(method: Method, path: string, body?: unknown): Promise<T> {
  if (inTauri) {
    try {
      return await invoke<T>('api', { method, path, body: body ?? null });
    } catch (e) {
      throw new Error(errorMessage(e));
    }
  }
  const hasBody = body !== undefined;
  let res: Response;
  try {
    res = await fetch(path, {
      method,
      headers: hasBody ? { 'content-type': 'application/json' } : undefined,
      body: hasBody ? JSON.stringify(body) : undefined,
    });
  } catch (e) {
    throw new Error(pick(M).devProxy(errorMessage(e)));
  }
  const text = await res.text();
  let data: unknown = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      throw new Error(res.ok ? pick(M).notJson : pick(M).http(res.status, text.slice(0, 200)));
    }
  }
  if (!res.ok) {
    throw new Error(data && typeof data === 'object' && 'error' in data ? String(data.error) : `HTTP ${res.status}`);
  }
  return data as T;
}

export function query(params: Record<string, string | number | undefined | null>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== '') q.set(k, String(v));
  }
  const s = q.toString();
  return s ? `?${s}` : '';
}

// Returns false when the user cancels the native save dialog.
export async function saveExport(suggestedName: string, contents: string): Promise<boolean> {
  if (inTauri) {
    try {
      return await invoke<boolean>('save_export', {
        suggested_name: suggestedName,
        suggestedName,
        contents,
      });
    } catch (e) {
      throw new Error(errorMessage(e));
    }
  }
  const url = URL.createObjectURL(new Blob([contents], { type: 'application/json' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = suggestedName;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
  return true;
}
