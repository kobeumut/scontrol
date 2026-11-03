import { FormEvent, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { api, ApiError, Capability, Site, useApi, User } from './api';
import { ComposeProjects } from './ComposeProjects';
import { GitHubApps } from './GitHubApps';
import { Notifications } from './Notifications';
import { Projects } from './Projects';
import { Registries } from './Registries';
import './styles.css';

function Login({ onSignedIn }: { onSignedIn: () => void }) {
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [totp, setTotp] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true); setError(undefined);
    try {
      await api('/auth/login', { method: 'POST', body: { username, password, totp_code: totp || undefined } });
      onSignedIn();
    } catch (e) {
      setError(e instanceof ApiError && e.status === 401 ? 'Wrong username, password or 2FA code.' : (e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return <form className="card login" onSubmit={submit}>
    <h2>Sign in</h2>
    <label>Username<input required autoComplete="username" value={username} onChange={(e) => setUsername(e.target.value)} /></label>
    <label>Password<input required type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} /></label>
    <label>2FA code <span className="muted small">(if enabled)</span><input inputMode="numeric" autoComplete="one-time-code" pattern="[0-9]{6}" value={totp} onChange={(e) => setTotp(e.target.value)} /></label>
    {error && <p className="state error" role="alert">{error}</p>}
    <button className="action" disabled={busy}>{busy ? 'Signing in…' : 'Sign in'}</button>
  </form>;
}

function Sites() {
  const sites = useApi<Site[]>('/sites');
  return <section className="panel">
    <div className="panel-title"><h2>Hosted sites</h2><button className="action" disabled>New site</button></div>
    {sites.loading && <p className="state">Loading…</p>}
    {sites.error && <p className="state error">{sites.error}</p>}
    {sites.data && <table><thead><tr><th>Domain</th><th>Runtime</th><th>Status</th><th>TLS</th></tr></thead><tbody>
      {sites.data.length === 0 ? <tr><td colSpan={4} className="empty">No sites provisioned.</td></tr> : sites.data.map((site) => <tr key={site.id}><td>{site.domain}</td><td>{site.runtime.kind}</td><td><span className={`badge ${site.status}`}>{site.status}</span></td><td>{site.ssl_enabled ? 'enabled' : '—'}</td></tr>)}
    </tbody></table>}
  </section>;
}

function App() {
  const health = useApi<{ status: string; capabilities?: Capability }>('/health/agent', 30000);
  const me = useApi<User>('/auth/me');
  const [active, setActive] = useState<'sites' | 'groups' | 'projects' | 'registries' | 'github' | 'notifications'>('projects');
  const docker = health.data?.capabilities?.docker;

  const signOut = async () => {
    await api('/auth/logout', { method: 'POST' }).catch(() => undefined);
    me.reload();
  };

  return <main className="shell">
    <header className="masthead">
      <div><span className="mark">S/</span><span className="product">scontrol</span></div>
      <div className="system-state">
        <i className={health.data?.status === 'ok' ? 'lamp up' : 'lamp'} /> agent {health.data?.status ?? 'checking'}
        {me.data && <> · {me.data.username} <button className="link" onClick={signOut}>sign out</button></>}
      </div>
    </header>

    <section className="capabilities" aria-label="Installed capabilities">
      {(['docker', 'mail', 'antivirus'] as const).map((feature) => <div className="cap" key={feature}>
        <span>{feature}</span><strong>{health.data?.capabilities?.[feature] ? 'installed' : 'not selected'}</strong>
      </div>)}
      <div className="cap"><span>agent</span><strong>{health.data?.capabilities?.version ?? '—'}</strong></div>
    </section>

    {me.loading ? <p className="state">Checking session…</p>
      : !me.data ? <Login onSignedIn={me.reload} />
      : <>
        <nav className="tabs" aria-label="Primary content">
          <button className={active === 'groups' ? 'selected' : ''} onClick={() => setActive('groups')}>Projects &amp; environments</button>
          <button className={active === 'projects' ? 'selected' : ''} onClick={() => setActive('projects')}>Compose projects</button>
          <button className={active === 'registries' ? 'selected' : ''} onClick={() => setActive('registries')}>Registries</button>
          <button className={active === 'sites' ? 'selected' : ''} onClick={() => setActive('sites')}>Hosted sites</button>
          <button className={active === 'github' ? 'selected' : ''} onClick={() => setActive('github')}>GitHub Apps</button>
          <button className={active === 'notifications' ? 'selected' : ''} onClick={() => setActive('notifications')}>Notifications</button>
        </nav>
        {active === 'sites' ? <Sites />
          : active === 'notifications' ? <Notifications />
            : active === 'github' ? <GitHubApps />
            : active === 'registries' ? (docker === false ? <section className="panel"><p className="state">Docker was not selected at install time.</p></section> : <Registries />)
            : docker === false ? <section className="panel"><p className="state">Docker was not selected at install time. Re-run the installer and opt in to enable projects and Compose services.</p></section>
            : active === 'groups' ? <Projects />
            : <ComposeProjects buildTools={health.data?.capabilities?.build_tools} />}
      </>}
  </main>;
}

createRoot(document.getElementById('root')!).render(<App />);
