import { useState } from 'react';
import { ApiState } from './api';

export function StateView({ state }: { state: Pick<ApiState<unknown>, 'loading' | 'error'> }) {
  if (state.loading) return <p className="state">Loading…</p>;
  if (state.error) return <p className="state error">{state.error}</p>;
  return null;
}

export function ErrorLine({ error }: { error?: string }) {
  return error ? <p className="state error" role="alert">{error}</p> : null;
}

/** Runs an async action, exposing busy/error state for the button that triggers it. */
export function useAction() {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const run = async (action: () => Promise<unknown>) => {
    setBusy(true); setError(undefined);
    try { await action(); } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  return { busy, error, run, fail: setError };
}
