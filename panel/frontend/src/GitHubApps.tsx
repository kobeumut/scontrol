import { FormEvent, useState } from 'react';
import { api, formatTime, GitHubApp, useApi } from './api';

/**
 * Registered GitHub Apps. The private key is write-only: it is sealed on the
 * server and used only to mint short-lived installation tokens per deployment.
 */
export function GitHubApps() {
  const apps = useApi<GitHubApp[]>('/github-apps');
  const [adding, setAdding] = useState(false);
  const [message, setMessage] = useState<string>();
  const [busy, setBusy] = useState(false);

  const remove = async (app: GitHubApp) => {
    if (!window.confirm(`Delete GitHub App ${app.name}? Projects must be unlinked first.`)) return;
    setBusy(true); setMessage(undefined);
    try { await api(`/github-apps/${app.id}`, { method: 'DELETE' }); apps.reload(); }
    catch (e) { setMessage((e as Error).message); }
    finally { setBusy(false); }
  };

  return <section className="panel">
    <div className="panel-title">
      <h2>GitHub Apps</h2>
      {!adding && <button className="action" onClick={() => setAdding(true)}>Add App</button>}
    </div>
    <p className="hint">Deploy private GitHub repositories without long-lived tokens. Create a GitHub App with read-only <span className="mono">Contents</span> permission, install it on the repositories, then link the installation under a project's Git access. Each deployment mints a one-hour token limited to that repository.</p>
    {adding && <AppForm onDone={() => { setAdding(false); apps.reload(); }} />}
    {apps.loading && <p className="state">Loading…</p>}
    {apps.error && <p className="state error">{apps.error}</p>}
    {message && <p className="state error" role="alert">{message}</p>}
    {apps.data && (apps.data.length === 0 ? <p className="state">No GitHub Apps yet.</p>
      : <table><thead><tr><th>Name</th><th>App ID</th><th>Added</th><th /></tr></thead><tbody>
        {apps.data.map((app) => <tr key={app.id}>
          <td>{app.name}</td>
          <td className="mono">{app.app_id}</td>
          <td>{formatTime(app.created_at)}</td>
          <td><button className="link danger" disabled={busy} onClick={() => remove(app)}>delete</button></td>
        </tr>)}
      </tbody></table>)}
  </section>;
}

function AppForm({ onDone }: { onDone: () => void }) {
  const [name, setName] = useState('');
  const [appId, setAppId] = useState('');
  const [privateKey, setPrivateKey] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  const readFile = async (file?: File) => { if (file) setPrivateKey(await file.text()); };
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true); setError(undefined);
    try {
      await api('/github-apps', { method: 'POST', body: { name: name.trim(), app_id: Number(appId), private_key: privateKey } });
      setPrivateKey('');
      onDone();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return <form className="card" onSubmit={submit}>
    <div className="grid2">
      <label>Name<input required maxLength={100} value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label>App ID<input required inputMode="numeric" pattern="[0-9]+" className="mono" value={appId} onChange={(e) => setAppId(e.target.value)} /></label>
    </div>
    <label>Private key (.pem, write-only)
      <input type="file" accept=".pem" onChange={(e) => readFile(e.target.files?.[0])} />
      <textarea required rows={4} className="mono" autoComplete="off" spellCheck={false}
        placeholder="-----BEGIN RSA PRIVATE KEY-----" value={privateKey} onChange={(e) => setPrivateKey(e.target.value)} />
    </label>
    <p className="hint">The key is encrypted at rest and never shown again.</p>
    {error && <p className="state error" role="alert">{error}</p>}
    <div className="toolbar">
      <button className="action" disabled={busy}>{busy ? 'Saving…' : 'Save App'}</button>
      <button type="button" className="ghost" onClick={onDone}>Cancel</button>
    </div>
  </form>;
}
