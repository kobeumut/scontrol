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

type Tab = 'projects' | 'services' | 'templates' | 'databases' | 'backups' | 'observability' | 'monitoring'
  | 'sites' | 'registries' | 'github' | 'notifications' | 'servers' | 'access' | 'keys' | 'account';

/** Sidebar sections; `needs` hides an entry the signed-in user cannot use. */
const NAV: { heading: string; items: { id: Tab; label: string; needs?: 'servers' | 'access' }[] }[] = [
  { heading: 'Deploy', items: [
    { id: 'projects', label: 'Projects' }, { id: 'services', label: 'Services' },
    { id: 'templates', label: 'Templates' }, { id: 'databases', label: 'Databases' },
  ] },
  { heading: 'Operate', items: [
    { id: 'monitoring', label: 'Monitoring' }, { id: 'observability', label: 'Observability' }, { id: 'backups', label: 'Backups' },
  ] },
  { heading: 'Hosting', items: [{ id: 'sites', label: 'Hosted sites' }] },
  { heading: 'Settings', items: [
    { id: 'registries', label: 'Registries' }, { id: 'github', label: 'GitHub Apps' }, { id: 'notifications', label: 'Notifications' },
    { id: 'servers', label: 'Servers', needs: 'servers' }, { id: 'access', label: 'Access', needs: 'access' },
  ] },
  { heading: 'You', items: [{ id: 'keys', label: 'API keys' }, { id: 'account', label: 'Account' }] },
];
const ALL_TABS = NAV.flatMap((group) => group.items);

/** The screen named by the address (`#/services`), so a reload stays on it. */
function tabFromHash(): Tab {
  const id = window.location.hash.replace(/^#\/?/, '');
  return ALL_TABS.find((t) => t.id === id)?.id ?? 'projects';
}

/** Where a screen should start: an open item, or a "new" form aimed at an environment. */
type Target = { projectId?: string; serviceId?: string; databaseId?: string; environmentId?: string };

function App() {
  const health = useApi<{ status: string; capabilities?: Capability }>('/health/agent', 30000);
  const me = useApi<User>('/auth/me');
  // `useApi` keeps the last data when a reload fails, so after signing out
  // (or an expired session) the error is what says nobody is signed in.
  const user = me.error ? undefined : me.data;
  const access = useApi<AccessMe>(user ? '/access/me' : null);
  const [requested, setRequested] = useState<Tab>(tabFromHash);
  const [target, setTarget] = useState<Target>({});
  // Remounts the screen, so choosing the entry of the open screen returns to its list.
  const [visit, setVisit] = useState(0);
  const [menuOpen, setMenuOpen] = useState(false);
  const [usersVersion, setUsersVersion] = useState(0);
  const canSeeServers = holdsGlobally(access.data, 'servers:read');
  const canManageAccess = holdsGlobally(access.data, 'access:manage');
  const docker = health.data?.capabilities?.docker;
  const allowed = (item: { needs?: 'servers' | 'access' }) =>
    item.needs === 'servers' ? canSeeServers : item.needs === 'access' ? canManageAccess : true;
  const active: Tab = ALL_TABS.some((t) => t.id === requested && allowed(t)) ? requested : 'projects';
  const activeLabel = ALL_TABS.find((t) => t.id === active)?.label;

  const go = (tab: Tab, next: Target = {}) => {
    setRequested(tab); setTarget(next); setVisit((v) => v + 1); setMenuOpen(false);
    window.history.replaceState(null, '', `#/${tab}`);
    window.scrollTo(0, 0);
  };
  const signOut = async () => {
    await api('/auth/logout', { method: 'POST' }).catch(() => undefined);
    me.reload();
  };

  const capabilities = <section className="capabilities" aria-label="Installed capabilities">
    {(['docker', 'mail', 'antivirus'] as const).map((feature) => {
      const on = health.data?.capabilities?.[feature];
      return <span className={`cap${on ? ' on' : ''}`} key={feature} title={on ? `${feature}: installed` : `${feature}: not selected at install time`}>
        <i aria-hidden="true" />{feature} {on ? <span className="sr-only">installed</span> : <strong>not selected</strong>}
      </span>;
    })}
    <span className="cap on"><i aria-hidden="true" />agent <strong>{health.data?.capabilities?.version ?? '—'}</strong></span>
  </section>;

  const dockerOff = (what: string) => <section className="panel"><p className="state">Docker was not selected at install time. {what}</p></section>;

  return <main className="shell">
    <header className="masthead">
      <div className="brand"><span className="mark">S/</span><span className="product">scontrol</span></div>
      {user && <button type="button" className="menu-toggle" aria-label="Menu" aria-expanded={menuOpen} aria-controls="primary-nav"
        onClick={() => setMenuOpen((open) => !open)}><span aria-hidden="true">{menuOpen ? '✕' : '☰'}</span> {activeLabel}</button>}
      <div className="system-state">
        <span className="agent-state"><i className={health.data?.status === 'ok' ? 'lamp up' : 'lamp'} />agent {health.data?.status ?? 'checking'}</span>
        {user && <> · {user.username} <button className="link" onClick={signOut}>sign out</button></>}
      </div>
    </header>

    {me.loading ? <p className="state">Checking session…</p>
      : !user ? <><Login onSignedIn={me.reload} />{capabilities}</>
      : <div className="layout">
        <aside id="primary-nav" className={menuOpen ? 'sidebar open' : 'sidebar'}>
          <nav aria-label="Primary content">
            {NAV.map((group) => {
              const items = group.items.filter(allowed);
              return items.length === 0 ? null : <div className="nav-group" key={group.heading} role="group" aria-label={group.heading}>
                <p className="nav-heading" aria-hidden="true">{group.heading}</p>
                {items.map((item) => <button type="button" key={item.id} className={active === item.id ? 'selected' : ''}
                  aria-current={active === item.id ? 'page' : undefined} onClick={() => go(item.id)}>{item.label}</button>)}
              </div>;
            })}
          </nav>
          {capabilities}
        </aside>
        <div className="content" key={`${active}-${visit}`}>
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
            : active === 'registries' ? (docker === false ? dockerOff('') : <Registries />)
            : docker === false ? dockerOff('Re-run the installer and opt in to enable projects and services.')
            : active === 'projects' ? <Projects initialOpenId={target.projectId}
                onOpenService={(serviceId) => go('services', { serviceId })}
                onOpenDatabase={(databaseId) => go('databases', { databaseId })}
                onAdd={(kind, environmentId) => go(kind, { environmentId })} />
            : active === 'templates' ? <Templates initialEnvironmentId={target.environmentId}
                onOpenService={(serviceId) => go('services', { serviceId })} />
            : active === 'databases' ? <Databases initialEnvironmentId={target.environmentId} initialSelectedId={target.databaseId} />
            : <ComposeProjects buildTools={health.data?.capabilities?.build_tools} initialOpenId={target.serviceId}
                initialEnvironmentId={target.environmentId} onOpenProject={(projectId) => go('projects', { projectId })} />}
        </div>
      </div>}
  </main>;
}

createRoot(document.getElementById('root')!).render(<App />);
