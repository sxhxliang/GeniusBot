// Small shared UI pieces.

import { useCallback, useEffect, useId, useRef, useState, type ReactNode } from "react";

export function Icon({ name, className = "size-4" }: { name: "grid" | "plus" | "settings" | "broadcast" | "check" | "logs" | "events" | "menu" | "close" | "arrow" | "agent"; className?: string }) {
  const paths = {
    grid: "M3 3h7v7H3z M14 3h7v7h-7z M3 14h7v7H3z M14 14h7v7h-7z",
    plus: "M12 5v14 M5 12h14",
    settings: "M4 7h16 M4 17h16 M8 4v6 M16 14v6",
    broadcast: "M4 10v4h4l10 5V5L8 10H4z M8 14l2 6 M21 9v6",
    check: "M9 12l2 2 4-4 M12 3l8 3v6c0 5-8 9-8 9s-8-4-8-9V6z",
    logs: "M5 3h10l4 4v14H5z M14 3v5h5 M8 12h8 M8 16h6",
    events: "M3 12h4l3-8 4 16 3-8h4",
    menu: "M4 6h16 M4 12h16 M4 18h16",
    close: "M6 6l12 12 M6 18L18 6",
    arrow: "M5 12h14 M13 6l6 6-6 6",
    agent: "M5 7h14v13H5z M12 3v4 M9 12h.01 M15 12h.01 M9 16h6 M2 11v5 M22 11v5",
  };
  return <svg className={`shrink-0 ${className}`} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d={paths[name]} /></svg>;
}

export function fmtTime(ms: number | null | undefined, withDate = false): string {
  if (!ms) return "—";
  const d = new Date(ms);
  const time = d.toLocaleTimeString(undefined, { hour12: false });
  if (!withDate && d.toDateString() === new Date().toDateString()) return time;
  return `${d.toLocaleDateString()} ${time}`;
}

export function fmtNum(n: number | null | undefined): string {
  return n === null || n === undefined ? "—" : n.toLocaleString();
}

export function fmtDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "…";
  return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
}

export function pretty(value: unknown): string {
  if (typeof value === "string") {
    try {
      return JSON.stringify(JSON.parse(value), null, 2);
    } catch {
      return value;
    }
  }
  return JSON.stringify(value, null, 2);
}

export function JsonView({ value, maxHeight }: { value: unknown; maxHeight?: number }) {
  return (
    <pre className="code" style={maxHeight ? { maxHeight } : undefined}>
      {pretty(value)}
    </pre>
  );
}

export function Collapsible({
  title,
  children,
  defaultOpen = false,
  className = "",
  right,
}: {
  title: ReactNode;
  children: ReactNode;
  defaultOpen?: boolean;
  className?: string;
  right?: ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const bodyId = useId();
  return (
    <div className={`collapsible ${open ? "open" : ""} ${className}`}>
      <div className="collapsible-head">
        <button type="button" className="flex min-w-0 flex-1 items-center gap-2" aria-expanded={open} aria-controls={bodyId} onClick={() => setOpen(!open)}>
          <span className="chevron" aria-hidden="true">{open ? "▾" : "▸"}</span>
          <span className="collapsible-title">{title}</span>
        </button>
        {right && <span className="shrink-0">{right}</span>}
      </div>
      {open && <div id={bodyId} className="collapsible-body">{children}</div>}
    </div>
  );
}

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      className="btn small ghost"
      onClick={async (e) => {
        e.stopPropagation();
        try {
          await navigator.clipboard.writeText(text);
          setDone(true);
          setTimeout(() => setDone(false), 1200);
        } catch {
          // clipboard unavailable (insecure context)
        }
      }}
    >
      {done ? "Copied" : label}
    </button>
  );
}

export function Modal({ title, onClose, children, wide }: { title: string; onClose: () => void; children: ReactNode; wide?: boolean }) {
  const titleId = useId();
  const dialog = useRef<HTMLDivElement>(null);
  const returnFocus = useRef(document.activeElement instanceof HTMLElement ? document.activeElement : null);
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    const previous = returnFocus.current;
    const root = dialog.current;
    const focusable = () => Array.from(root?.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [tabindex="0"]') ?? []).filter((el) => el.getClientRects().length > 0);
    if (!root?.contains(document.activeElement)) (focusable()[0] ?? root)?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); close.current(); }
      if (e.key !== "Tab") return;
      const items = focusable();
      const first = items[0];
      const last = items[items.length - 1];
      if (!first) { e.preventDefault(); root?.focus(); }
      else if (e.shiftKey && (document.activeElement === first || document.activeElement === root)) { e.preventDefault(); last.focus(); }
      else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
    };
    root?.addEventListener("keydown", onKey);
    return () => { root?.removeEventListener("keydown", onKey); if (previous?.isConnected) previous.focus(); };
  }, []);
  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div ref={dialog} role="dialog" aria-modal="true" aria-labelledby={titleId} tabIndex={-1} className={`modal ${wide ? "wide" : ""}`} onMouseDown={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h2 id={titleId}>{title}</h2>
          <button type="button" className="btn ghost" onClick={onClose} aria-label="Close">
            <Icon name="close" />
          </button>
        </div>
        <div className="modal-body">{children}</div>
      </div>
    </div>
  );
}

export function ErrorText({ error }: { error: unknown }) {
  if (!error) return null;
  return <div className="error-text">{error instanceof Error ? error.message : String(error)}</div>;
}

/** Load data with `load`, reload on `deps` change or when `reload()` is called. */
export function useLoad<T>(load: () => Promise<T>, deps: unknown[]) {
  const [data, setData] = useState<T | undefined>(undefined);
  const [error, setError] = useState<unknown>(undefined);
  const [loading, setLoading] = useState(false);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const run = useCallback(load, deps);
  const reload = useCallback(() => {
    setLoading(true);
    run()
      .then((d) => {
        setData(d);
        setError(undefined);
      })
      .catch(setError)
      .finally(() => setLoading(false));
  }, [run]);
  useEffect(() => {
    setData(undefined);
    reload();
  }, [reload]);
  return { data, error, loading, reload, setData };
}

/** Wrap an async action with a busy flag and an error. */
export function useAction() {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(undefined);
  const run = useCallback(async <T,>(f: () => Promise<T>): Promise<T | undefined> => {
    setBusy(true);
    setError(undefined);
    try {
      return await f();
    } catch (e) {
      setError(e);
      return undefined;
    } finally {
      setBusy(false);
    }
  }, []);
  return { busy, error, run, setError };
}

export function laneLabel(lane: string | null | undefined): string | null {
  if (!lane) return null;
  return lane === "automation" ? "background" : lane;
}

export function StatusDot({ lane, awaiting }: { lane: string | null; awaiting?: boolean }) {
  const cls = lane ? "busy" : awaiting ? "waiting" : "idle";
  const title = lane ? `running (${laneLabel(lane)} lane)` : awaiting ? "waiting for your answer" : "idle";
  return <span className={`dot ${cls}`} role="img" aria-label={title} title={title} />;
}
