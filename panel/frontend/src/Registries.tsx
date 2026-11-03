import { FormEvent, useState } from 'react';
import { api, formatTime, useApi } from './api';

export type Registry = {
  id: string; name: string; server_url: string; username: string; has_credential: boolean;
  created_at: string; updated_at: string;
};

type TestResult = { ok: boolean; message: string };

/** Presets fill the server and a typical username; the password is always typed. */
const PRESETS: [string, string, string][] = [
  ['Docker Hub', 'docker.io', ''],
  ['GitHub (ghcr.io)', 'ghcr.io', ''],
  ['GitLab', 'registry.gitlab.com', ''],
  ['AWS ECR', '<account>.dkr.ecr.<region>.amazonaws.com', 'AWS'],
];

export function Registries() {
  const registries = useApi<Registry[]>('/registries');
  const [editing, setEditing] = useState<Registry | 'new' | null>(null);
  const [message, setMessage] = useState<string>();
  const [busy, setBusy] = useState(false);

  const run = async (action: () => Promise<string | undefined>) => {
    setBusy(true); setMessage(undefined);
    try { setMessage(await action()); } catch (e) { setMessage((e as Error).message); } finally { setBusy(false); }
  };
  const test = (r: Registry) => run(async () => {
    const result = await api<TestResult>(`/registries/${r.id}/test`, { method: 'POST' });
    return result.ok ? `Login to ${r.server_url} succeeded.` : `Login to ${r.server_url} failed: ${result.message}`;
  });
  const remove = (r: Registry) => {
    if (!window.confirm(`Delete registry ${r.name}? Projects using it will pull anonymously.`)) return;
    run(async () => { await api(`/registries/${r.id}`, { method: 'DELETE' }); registries.reload(); return undefined; });
  };

  return <section className="panel">
    <div className="panel-title">
      <h2>Private registries</h2>
      {editing === null && <button className="action" onClick={() => setEditing('new')}>Add registry</button>}
    </div>
    <p className="hint">Credentials for pulling private images. Passwords and tokens are write-only and stored encrypted; they reach the host only while a deployment pulls images.</p>
    {editing !== null && <RegistryForm registry={editing === 'new' ? undefined : editing}
      onDone={() => { setEditing(null); registries.reload(); }} />}
    {registries.loading && <p className="state">Loading…</p>}
    {registries.error && <p className="state error">{registries.error}</p>}
    {message && <p className="state" role="status">{message}</p>}
    {registries.data && (registries.data.length === 0 ? <p className="state">No registries yet.</p>
      : <table><thead><tr><th>Name</th><th>Server</th><th>Username</th><th>Updated</th><th /></tr></thead><tbody>
        {registries.data.map((r) => <tr key={r.id}>
          <td>{r.name}</td>
          <td className="mono small">{r.server_url}</td>
          <td className="mono small">{r.username}</td>
          <td className="small">{formatTime(r.updated_at)}</td>
          <td>
            <button className="link" disabled={busy} onClick={() => test(r)}>test</button>{' '}
            <button className="link" disabled={busy} onClick={() => setEditing(r)}>edit</button>{' '}
            <button className="link danger" disabled={busy} onClick={() => remove(r)}>delete</button>
          </td>
        </tr>)}
      </tbody></table>)}
  </section>;
}

function RegistryForm({ registry, onDone }: { registry?: Registry; onDone: () => void }) {
  const [name, setName] = useState(registry?.name ?? '');
  const [server, setServer] = useState(registry?.server_url ?? '');
  const [username, setUsername] = useState(registry?.username ?? '');
  const [password, setPassword] = useState('');
  const [error, setError] = useState<string>();
  const [result, setResult] = useState<TestResult>();
  const [busy, setBusy] = useState(false);

  const body = () => ({ name: name.trim(), server_url: server.trim(), username: username.trim(), password: password || undefined });

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true); setError(undefined);
    try {
      await api(registry ? `/registries/${registry.id}` : '/registries', { method: registry ? 'PUT' : 'POST', body: body() });
      setPassword('');
      onDone();
    } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  // Tests what is typed, before saving. Needs the password; an edit that keeps
  // the stored one is tested from the list instead.
  const testTyped = async () => {
    setBusy(true); setError(undefined); setResult(undefined);
    try { setResult(await api<TestResult>('/registries/test', { method: 'POST', body: body() })); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };

  return <form className="card" onSubmit={submit}>
    {!registry && <div className="segmented" role="group" aria-label="Presets">
      {PRESETS.map(([label, host, user]) => <button type="button" key={label}
        onClick={() => { setServer(host); if (user) setUsername(user); if (!name) setName(label); }}>{label}</button>)}
    </div>}
    <div className="grid2">
      <label>Name<input required maxLength={100} value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label>Server<input required className="mono" placeholder="ghcr.io" value={server} onChange={(e) => setServer(e.target.value)} /></label>
      <label>Username<input required className="mono" autoComplete="off" value={username} onChange={(e) => setUsername(e.target.value)} /></label>
      <label>Password or token<input type="password" autoComplete="new-password" value={password}
        required={!registry} placeholder={registry ? 'stored; leave empty to keep' : ''}
        onChange={(e) => setPassword(e.target.value)} /></label>
    </div>
    <p className="hint">Docker Hub: an access token. GitHub: a classic token with <span className="mono">read:packages</span>. GitLab: a deploy or access token. ECR: the output of <span className="mono">aws ecr get-login-password</span> (expires after 12 hours).</p>
    {error && <p className="state error" role="alert">{error}</p>}
    {result && <p className={result.ok ? 'state' : 'state error'} role="status">{result.ok ? 'Login succeeded.' : `Login failed: ${result.message}`}</p>}
    <div className="toolbar">
      <button className="action" disabled={busy}>{registry ? 'Save registry' : 'Create registry'}</button>
      <button type="button" className="ghost" disabled={busy || !password || !server || !username} onClick={testTyped}>Test login</button>
      <button type="button" className="ghost" onClick={onDone}>Cancel</button>
    </div>
  </form>;
}

/** Choose which registry a project pulls with (`''` is anonymous). */
export function RegistrySelect({ value, onChange, disabled }: { value: string; onChange: (id: string) => void; disabled?: boolean }) {
  const registries = useApi<Registry[]>('/registries');
  return <label>Private registry
    <select value={value} disabled={disabled || registries.loading} onChange={(e) => onChange(e.target.value)}>
      <option value="">none (anonymous pulls)</option>
      {registries.data?.map((r) => <option key={r.id} value={r.id}>{r.name} · {r.server_url}</option>)}
    </select>
  </label>;
}
