import { useEffect, useState } from 'react';
import { api, EnvLayer, LayerVariables, useApi, VariableInput } from './api';

/** A row of the editable layer. `value` empty on a stored key keeps the stored value. */
type Row = { key: string; value: string; stored: boolean };

const LAYER_LABEL: Record<EnvLayer, string> = { project: 'project', environment: 'environment', service: 'service' };

/**
 * Layered, write-only variable editor for one layer (project, environment,
 * or service). It shows every effective key with the layer it comes from and
 * the layers it overrides, so inherited and overridden variables are visible;
 * values are never loaded back, because the panel does not return them.
 */
export function LayeredVariables({ path, layer, title, hint }: {
  /** API path of the layer: GET lists keys, PUT replaces the layer. */
  path: string; layer: EnvLayer; title: string; hint?: string;
}) {
  const info = useApi<LayerVariables>(path);
  const [rows, setRows] = useState<Row[]>([]);
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [saved, setSaved] = useState(false);

  const own = info.data?.layers.find((l) => l.layer === layer)?.keys;
  useEffect(() => {
    if (own && !dirty) setRows(own.map((key) => ({ key, value: '', stored: true })));
    // Reset the rows only when the stored key list changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [own?.join('\n'), dirty]);

  const edit = (next: Row[]) => { setRows(next); setDirty(true); setSaved(false); };
  const update = (index: number, patch: Partial<Row>) => edit(rows.map((r, i) => (i === index ? { ...r, ...patch } : r)));
  const override = (key: string) => edit([...rows, { key, value: '', stored: false }]);

  const save = async () => {
    setBusy(true); setError(undefined);
    const variables: VariableInput[] = rows
      .filter((r) => r.key.trim())
      .map((r) => (r.stored && r.value === '' ? { key: r.key } : { key: r.key.trim(), value: r.value }));
    try {
      await api<LayerVariables>(path, { method: 'PUT', body: { variables } });
      setDirty(false); setSaved(true); info.reload();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const ownKeys = new Set(rows.map((r) => r.key.trim()));
  const inherited = (info.data?.variables ?? []).filter((v) => v.source !== layer || v.overrides.length > 0);

  return <>
    <h3>{title}</h3>
    {hint && <p className="hint">{hint}</p>}
    {info.loading && <p className="state">Loading…</p>}
    {info.error && <p className="state error">{info.error}</p>}
    {info.data?.layered === false && <p className="state warn">
      This service predates environment layering: its current variables live only on the server, so nothing is inherited yet.
      Save its variables here once (re-enter the values) to start inheriting from the environment and project.
    </p>}
    {info.data && <>
      {inherited.length > 0 && <table className="layers">
        <thead><tr><th>Variable</th><th>Value from</th><th>Overrides</th><th /></tr></thead>
        <tbody>{inherited.map((v) => <tr key={v.key}>
          <td className="mono">{v.key}</td>
          <td><span className={`badge layer-${v.source}`}>{LAYER_LABEL[v.source]}</span>{v.source !== layer && ' (inherited)'}</td>
          <td>{v.overrides.length === 0 ? '—' : v.overrides.map((o) => <span key={o} className="badge overridden">{LAYER_LABEL[o]}</span>)}</td>
          <td>{v.source !== layer && !ownKeys.has(v.key) &&
            <button type="button" className="link" onClick={() => override(v.key)}>override here</button>}</td>
        </tr>)}</tbody>
      </table>}
      <div className="env-editor">
        {rows.map((row, index) => <div className="env-row" key={index}>
          <input aria-label="Variable name" placeholder="NAME" className="mono" value={row.key} readOnly={row.stored}
            onChange={(e) => update(index, { key: e.target.value })} />
          <input aria-label="Variable value" type="password" autoComplete="off" value={row.value}
            placeholder={row.stored ? 'stored · leave empty to keep' : 'value'}
            onChange={(e) => update(index, { value: e.target.value })} />
          <button type="button" className="link" onClick={() => edit(rows.filter((_, i) => i !== index))}>remove</button>
        </div>)}
        <button type="button" className="link" onClick={() => edit([...rows, { key: '', value: '', stored: false }])}>+ add variable</button>
      </div>
      <div className="toolbar">
        <button className="ghost" disabled={busy || !dirty} onClick={save}>{busy ? 'Saving…' : 'Save variables'}</button>
        {saved && <span className="state">Saved. Deploy to apply.</span>}
      </div>
      {error && <p className="state error" role="alert">{error}</p>}
    </>}
  </>;
}
