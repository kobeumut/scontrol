import { FormEvent, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { Access } from './Access';
import { Account } from './Account';
import { AccessMe, api, ApiError, Capability, holdsGlobally, useApi, User } from './api';
import { ApiKeys } from './ApiKeys';
import { ComposeProjects } from './ComposeProjects';
import { Databases } from './Databases';
import { GitHubApps } from './GitHubApps';
import { Monitoring } from './Monitoring';
import { Notifications } from './Notifications';
import { Projects } from './Projects';
import { Backups } from './Backups';
import { Observability } from './Observability';
import { Registries } from './Registries';
import { Templates } from './Templates';
import { Servers } from './Servers';
import { Sites } from './Sites';
import { Users } from './Users';
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

function App() {
  const health = useApi<{ status: string; capabilities?: Capability }>('/health/agent', 30000);
  const me = useApi<User>('/auth/me');
  // `useApi` keeps the last data when a reload fails, so after signing out
  // (or an expired session) the error is what says nobody is signed in.
  const user = me.error ? undefined : me.data;
  const access = useApi<AccessMe>(user ? '/access/me' : null);
  const [active, setActive] = useState<'sites' | 'groups' | 'projects' | 'templates' | 'databases' | 'registries' | 'backups' | 'github' | 'notifications' | 'observability' | 'monitoring' | 'servers' | 'access' | 'keys' | 'account'>('projects');
  const [usersVersion, setUsersVersion] = useState(0);
  const canSeeServers = holdsGlobally(access.data, 'servers:read');
  const canManageAccess = holdsGlobally(access.data, 'access:manage');
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
        {user && <> · {user.username} <button className="link" onClick={signOut}>sign out</button></>}
      </div>
    </header>

    <section className="capabilities" aria-label="Installed capabilities">
      {(['docker', 'mail', 'antivirus'] as const).map((feature) => <div className="cap" key={feature}>
        <span>{feature}</span><strong>{health.data?.capabilities?.[feature] ? 'installed' : 'not selected'}</strong>
      </div>)}
      <div className="cap"><span>agent</span><strong>{health.data?.capabilities?.version ?? '—'}</strong></div>
    </section>

    {me.loading ? <p className="state">Checking session…</p>
      : !user ? <Login onSignedIn={me.reload} />
      : <>
        <nav className="tabs" aria-label="Primary content">
          <button className={active === 'groups' ? 'selected' : ''} onClick={() => setActive('groups')}>Projects &amp; environments</button>
          <button className={active === 'projects' ? 'selected' : ''} onClick={() => setActive('projects')}>Compose projects</button>
          <button className={active === 'templates' ? 'selected' : ''} onClick={() => setActive('templates')}>Templates</button>
          <button className={active === 'databases' ? 'selected' : ''} onClick={() => setActive('databases')}>Databases</button>
          <button className={active === 'backups' ? 'selected' : ''} onClick={() => setActive('backups')}>Backups</button>
          <button className={active === 'observability' ? 'selected' : ''} onClick={() => setActive('observability')}>Observability</button>
          <button className={active === 'registries' ? 'selected' : ''} onClick={() => setActive('registries')}>Registries</button>
          <button className={active === 'monitoring' ? 'selected' : ''} onClick={() => setActive('monitoring')}>Monitoring</button>
          <button className={active === 'sites' ? 'selected' : ''} onClick={() => setActive('sites')}>Hosted sites</button>
          <button className={active === 'github' ? 'selected' : ''} onClick={() => setActive('github')}>GitHub Apps</button>
          <button className={active === 'notifications' ? 'selected' : ''} onClick={() => setActive('notifications')}>Notifications</button>
          {canSeeServers && <button className={active === 'servers' ? 'selected' : ''} onClick={() => setActive('servers')}>Servers</button>}
          {canManageAccess && <button className={active === 'access' ? 'selected' : ''} onClick={() => setActive('access')}>Access</button>}
          <button className={active === 'keys' ? 'selected' : ''} onClick={() => setActive('keys')}>API keys</button>
          <button className={active === 'account' ? 'selected' : ''} onClick={() => setActive('account')}>Account</button>
        </nav>
        {active === 'sites' ? <Sites isAdmin={user.is_admin} />
          : active === 'account' ? <Account user={user} onChanged={me.reload} />
          : active === 'servers' ? <Servers canManage={holdsGlobally(access.data, 'servers:manage')} />
          : active === 'access' ? <>{user.is_admin && <Users currentUserId={user.id} onChanged={() => setUsersVersion((v) => v + 1)} />}<Access key={usersVersion} /></>
          : active === 'keys' ? <ApiKeys me={access.data} />
          : active === 'observability' ? <Observability isAdmin={user.is_admin} docker={docker !== false} />
          : active === 'monitoring' ? <Monitoring docker={docker !== false} />
          : active === 'notifications' ? <Notifications />
            : active === 'github' ? <GitHubApps />
            : active === 'backups' ? <Backups docker={docker !== false} />
            : active === 'registries' ? (docker === false ? <section className="panel"><p className="state">Docker was not selected at install time.</p></section> : <Registries />)
            : docker === false ? <section className="panel"><p className="state">Docker was not selected at install time. Re-run the installer and opt in to enable projects and Compose services.</p></section>
            : active === 'groups' ? <Projects />
            : active === 'templates' ? <Templates />
            : active === 'databases' ? <Databases />
            : <ComposeProjects buildTools={health.data?.capabilities?.build_tools} />}
      </>}
  </main>;
}

createRoot(document.getElementById('root')!).render(<App />);
