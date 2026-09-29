// Live event stream (SSE) shared by every view.

import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { eventsUrl, type BusEvent } from "./api";

type Listener = (event: BusEvent) => void;

interface Bus {
  connected: boolean;
  subscribe: (listener: Listener) => () => void;
}

const BusContext = createContext<Bus>({ connected: false, subscribe: () => () => {} });

export function BusProvider({ children, token }: { children: ReactNode; token: string }) {
  const listeners = useRef(new Set<Listener>());
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    let source: EventSource | null = null;
    let lastSeq: number | undefined;
    let retry: ReturnType<typeof setTimeout> | undefined;
    let closed = false;

    const onMessage = (msg: MessageEvent) => {
      let event: BusEvent;
      try {
        event = JSON.parse(msg.data);
      } catch {
        return;
      }
      lastSeq = event.seq;
      listeners.current.forEach((l) => l(event));
    };
    const connect = () => {
      // Resume after the last event seen so a reconnect replays what was missed.
      source = new EventSource(eventsUrl(lastSeq));
      source.addEventListener("host", onMessage);
      source.addEventListener("server", onMessage);
      source.onopen = () => setConnected(true);
      source.onerror = () => {
        setConnected(false);
        source?.close();
        if (!closed) retry = setTimeout(connect, 2000);
      };
    };
    connect();
    return () => {
      closed = true;
      clearTimeout(retry);
      source?.close();
    };
  }, [token]);

  const subscribe = useCallback((listener: Listener) => {
    listeners.current.add(listener);
    return () => {
      listeners.current.delete(listener);
    };
  }, []);
  const bus = useMemo<Bus>(() => ({ connected, subscribe }), [connected, subscribe]);
  return <BusContext.Provider value={bus}>{children}</BusContext.Provider>;
}

export function useConnected(): boolean {
  return useContext(BusContext).connected;
}

/** Call `handler` for every event; the latest handler is always used. */
export function useBusEvents(handler: Listener) {
  const { subscribe } = useContext(BusContext);
  const ref = useRef(handler);
  ref.current = handler;
  useEffect(() => subscribe((e) => ref.current(e)), [subscribe]);
}

/** The agent an event concerns, when any. */
export function eventAgentId(e: BusEvent): string | undefined {
  const d = e.data as Record<string, any>;
  return d.agent_id ?? d.parent_id ?? d.address?.id ?? d.request?.agent_id ?? d.agentId ?? (e.kind === "a2a-queued" ? d.to : undefined);
}

/** Run `f` at most once per `ms`, trailing. */
export function useDebounced(f: () => void, ms: number): () => void {
  const ref = useRef(f);
  ref.current = f;
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  const [trigger] = useState(() => () => {
    if (timer.current) return;
    timer.current = setTimeout(() => {
      timer.current = undefined;
      ref.current();
    }, ms);
  });
  return trigger;
}
