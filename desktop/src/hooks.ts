import { useEffect, useRef, useState } from 'react';

// True while the window is on screen. The menu bar app hides its window instead of closing it,
// so anything that polls the daemon must stop while this is false.
export function useVisible(): boolean {
  const [visible, setVisible] = useState(() => document.visibilityState === 'visible');
  useEffect(() => {
    const update = () => setVisible(document.visibilityState === 'visible');
    update();
    document.addEventListener('visibilitychange', update);
    return () => document.removeEventListener('visibilitychange', update);
  }, []);
  return visible;
}

// Returns `prev` when `next` serializes identically, so setState bails out of a re-render.
export function keepIfSame<T>(prev: T, next: T): T {
  return prev === next || JSON.stringify(prev) === JSON.stringify(next) ? prev : next;
}

// Runs `fn` immediately and every `ms` while `active` and the window is visible; re-runs at once
// when `deps` change or the window becomes visible again.
// Overlapping runs are skipped so slow backends are never flooded.
export function usePoll(fn: () => Promise<void>, ms: number, active: boolean, deps: readonly unknown[]) {
  const latest = useRef(fn);
  latest.current = fn;
  const visible = useVisible();
  const running = active && visible;
  useEffect(() => {
    if (!running) return;
    let busy = false;
    const tick = async () => {
      if (busy) return;
      busy = true;
      try {
        await latest.current();
      } finally {
        busy = false;
      }
    };
    void tick();
    const timer = setInterval(() => void tick(), ms);
    return () => clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ms, running, ...deps]);
}

export function useDebounced<T>(value: T, ms: number, onSettled: (v: T) => void) {
  const cb = useRef(onSettled);
  cb.current = onSettled;
  useEffect(() => {
    const t = setTimeout(() => cb.current(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
}
