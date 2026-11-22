import { FormEvent, useState } from 'react';
import { api, Site, useApi } from './api';
import { ErrorLine, StateView, useAction } from './ui';

type RuntimeKind = 'static' | 'php' | 'node_js' | 'python' | 'reverse_proxy';
const RUNTIME_LABELS: Record<RuntimeKind, string> = {
  static: 'Static files', php: 'PHP (PHP-FPM)', node_js: 'Node.js', python: 'Python', reverse_proxy: 'Reverse proxy',
};
const DEFAULT_VERSION: Partial<Record<RuntimeKind, string>> = { php: '8.3', node_js: '20', python: '3.12' };
const DEFAULT_ENTRYPOINT: Partial<Record<RuntimeKind, string>> = { node_js: 'index.js', python: 'app.py' };

type HostDatabase = { id: string; engine: 'postgres' | 'my_sql'; name: string; username: string; created_at: string };
type CronJob = { id: string; schedule: string; command: string; enabled: boolean };
type FirewallRule = { id: string; port: number; protocol: 'tcp' | 'udp'; description: string; allow: boolean };

function runtimeLabel(runtime: Site['runtime'] & { version?: string; upstream?: string }): string {
  const label = RUNTIME_LABELS[runtime.kind as RuntimeKind] ?? runtime.kind;
  return runtime.version ? `${label} ${runtime.version}` : runtime.upstream ? `${label} → ${runtime.upstream}` : label;
}

/** Sites served by the host Nginx, with their databases and cron jobs, and the host firewall. */
export function Sites({ isAdmin }: { isAdmin: boolean }) {
  const sites = useApi<Site[]>('/sites');
  const [creating, setCreating] = useState(false);
  const [openId, setOpenId] = useState<string>();
  const action = useAction();
  const open = sites.data?.find((s) => s.id === openId);

  const remove = (site: Site) => {
    if (!window.confirm(`Delete ${site.domain}? Its files, vhost and system user's site directory are removed.`)) return;
    action.run(async () => {
      await api(`/sites/${site.id}`, { method: 'DELETE' });
      if (openId === site.id) setOpenId(undefined);
      sites.reload();
    });
  };
  const issue = (site: Site) => {
    const email = window.prompt(`Email address for the Let's Encrypt account of ${site.domain}:`);
    if (!email?.trim()) return;
    action.run(async () => {
      await api(`/tls/site/${site.id}`, { method: 'POST', body: { admin_email: email.trim() } });
      sites.reload();
    });
  };

  return <>
    <section className="panel">
      <div className="panel-title">
        <h2>Hosted sites</h2>
        <button className="action" onClick={() => setCreating((c) => !c)}>{creating ? 'Close' : 'New site'}</button>
      </div>
      <p className="hint">Sites run directly on the host behind Nginx, each under its own system user. For containers use Compose projects.</p>
      {creating && <CreateSite onCreated={() => { setCreating(false); sites.reload(); }} />}
      <StateView state={sites} />
      <ErrorLine error={action.error} />
      {sites.data && <table><thead><tr><th>Domain</th><th>Runtime</th><th>Status</th><th>TLS</th><th /></tr></thead><tbody>
        {sites.data.length === 0 ? <tr><td colSpan={5} className="empty">No sites provisioned.</td></tr> : sites.data.map((site) => <tr key={site.id}>
          <td><button className="link strong" onClick={() => setOpenId(openId === site.id ? undefined : site.id)}>{site.domain}</button></td>
          <td>{runtimeLabel(site.runtime)}</td>
          <td><span className={`badge ${site.status}`}>{site.status}</span></td>
          <td>{site.ssl_enabled ? 'enabled' : '—'}</td>
          <td>
            <button className="link" onClick={() => setOpenId(openId === site.id ? undefined : site.id)}>{openId === site.id ? 'close' : 'manage'}</button>{' '}
            {!site.ssl_enabled && <><button className="link" disabled={action.busy} onClick={() => issue(site)}>issue certificate</button>{' '}</>}
            <button className="link danger" disabled={action.busy} onClick={() => remove(site)}>delete</button>
          </td>
        </tr>)}
      </tbody></table>}
    </section>
    {open && <SiteDetail key={open.id} site={open} isAdmin={isAdmin} />}
    {isAdmin && <Firewall />}
  </>;
}

function CreateSite({ onCreated }: { onCreated: () => void }) {
  const [domain, setDomain] = useState('');
  const [owner, setOwner] = useState('');
  const [kind, setKind] = useState<RuntimeKind>('static');
  const [version, setVersion] = useState('');
  const [entrypoint, setEntrypoint] = useState('');
  const [upstream, setUpstream] = useState('http://127.0.0.1:3000');
  const action = useAction();

  const choose = (next: RuntimeKind) => {
    setKind(next); setVersion(DEFAULT_VERSION[next] ?? ''); setEntrypoint(DEFAULT_ENTRYPOINT[next] ?? '');
  };
  const submit = (event: FormEvent) => {
    event.preventDefault();
    const runtime = kind === 'static' ? { kind }
      : kind === 'php' ? { kind, version: version.trim() }
      : kind === 'reverse_proxy' ? { kind, upstream: upstream.trim() }
      : { kind, version: version.trim(), entrypoint: entrypoint.trim() };
    action.run(async () => {
      await api('/sites', { method: 'POST', body: { domain: domain.trim().toLowerCase(), owner_username: owner.trim(), runtime } });
      onCreated();
    });
  };

  return <form className="card" onSubmit={submit}>
    <div className="grid2">
      <label>Domain<input required className="mono" placeholder="example.com" value={domain} onChange={(e) => setDomain(e.target.value)} /></label>
      <label>System user <span className="muted small">(created if missing; owns the site's files)</span>
        <input required className="mono" placeholder="example" pattern="[a-z][a-z0-9_\-]{2,31}"
          title="3 to 32 lowercase letters, digits, - or _, starting with a letter" value={owner} onChange={(e) => setOwner(e.target.value)} /></label>
      <label>Runtime<select value={kind} onChange={(e) => choose(e.target.value as RuntimeKind)}>
        {(Object.keys(RUNTIME_LABELS) as RuntimeKind[]).map((k) => <option key={k} value={k}>{RUNTIME_LABELS[k]}</option>)}
      </select></label>
      {(kind === 'php' || kind === 'node_js' || kind === 'python') &&
        <label>Version<input required className="mono" pattern="[0-9]+(\.[0-9]+)?" value={version} onChange={(e) => setVersion(e.target.value)} /></label>}
      {(kind === 'node_js' || kind === 'python') &&
        <label>Entrypoint<input required className="mono" value={entrypoint} onChange={(e) => setEntrypoint(e.target.value)} /></label>}
      {kind === 'reverse_proxy' &&
        <label>Upstream<input required className="mono" type="url" value={upstream} onChange={(e) => setUpstream(e.target.value)} /></label>}
    </div>
    <ErrorLine error={action.error} />
    <button className="action" disabled={action.busy}>{action.busy ? 'Creating…' : 'Create site'}</button>
  </form>;
}

function SiteDetail({ site, isAdmin }: { site: Site; isAdmin: boolean }) {
  return <section className="panel">
    <div className="panel-title"><h2>{site.domain}</h2></div>
    <SiteDatabases site={site} isAdmin={isAdmin} />
    <SiteCron site={site} />
  </section>;
}

function SiteDatabases({ site, isAdmin }: { site: Site; isAdmin: boolean }) {
  const databases = useApi<HostDatabase[]>(`/databases/site/${site.id}`);
  const [engine, setEngine] = useState<HostDatabase['engine']>('postgres');
  const [name, setName] = useState('');
  const [username, setUsername] = useState('');
  const [created, setCreated] = useState<{ name: string; username: string; password: string }>();
  const action = useAction();

  const create = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      const result = await api<HostDatabase & { password: string }>('/databases', {
        method: 'POST', body: { site_id: site.id, engine, name: name.trim(), username: username.trim() },
      });
      setCreated(result); setName(''); setUsername('');
      databases.reload();
    });
  };
  const remove = (database: HostDatabase) => {
    if (!window.confirm(`Drop database ${database.name} and its user ${database.username}? The data is lost.`)) return;
    action.run(async () => { await api(`/databases/${database.id}`, { method: 'DELETE' }); databases.reload(); });
  };

  return <>
    <h3>Databases</h3>
    <p className="hint">Databases on the host's own PostgreSQL or MySQL server. The password is generated and shown once.</p>
    <StateView state={databases} />
    {created && <div className="card">
      <strong>Copy the password for {created.username} now. It is not shown again.</strong>
      <div className="secret mono">{created.password}</div>
      <div className="toolbar"><button className="ghost" onClick={() => setCreated(undefined)}>Done</button></div>
    </div>}
    {databases.data && <table><thead><tr><th>Database</th><th>User</th><th>Engine</th><th /></tr></thead><tbody>
      {databases.data.length === 0 ? <tr><td colSpan={4} className="empty">No databases for this site.</td></tr>
        : databases.data.map((d) => <tr key={d.id}>
          <td className="mono">{d.name}</td><td className="mono">{d.username}</td>
          <td>{d.engine === 'postgres' ? 'PostgreSQL' : 'MySQL'}</td>
          <td>{isAdmin && <button className="link danger" disabled={action.busy} onClick={() => remove(d)}>drop</button>}</td>
        </tr>)}
    </tbody></table>}
    <form className="toolbar" onSubmit={create}>
      <select aria-label="Database engine" value={engine} onChange={(e) => setEngine(e.target.value as HostDatabase['engine'])}>
        <option value="postgres">PostgreSQL</option><option value="my_sql">MySQL</option>
      </select>
      <input required aria-label="Database name" className="mono" placeholder="database" pattern="[A-Za-z][A-Za-z0-9_]{0,62}" value={name} onChange={(e) => setName(e.target.value)} />
      <input required aria-label="Database user" className="mono" placeholder="user" pattern="[A-Za-z][A-Za-z0-9_]{0,62}" value={username} onChange={(e) => setUsername(e.target.value)} />
      <button className="ghost" disabled={action.busy}>Create database</button>
    </form>
    <ErrorLine error={action.error} />
  </>;
}

function SiteCron({ site }: { site: Site }) {
  const jobs = useApi<CronJob[]>(`/cron/site/${site.id}`);
  const [schedule, setSchedule] = useState('');
  const [command, setCommand] = useState('');
  const action = useAction();

  const create = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      await api(`/cron/site/${site.id}`, { method: 'POST', body: { schedule: schedule.trim().replace(/\s+/g, ' '), command: command.trim() } });
      setSchedule(''); setCommand('');
      jobs.reload();
    });
  };
  const toggle = (job: CronJob) => action.run(async () => {
    await api(`/cron/${job.id}`, { method: 'PATCH', body: { enabled: !job.enabled } });
    jobs.reload();
  });
  const remove = (job: CronJob) => {
    if (!window.confirm(`Delete the cron job "${job.command}"?`)) return;
    action.run(async () => { await api(`/cron/${job.id}`, { method: 'DELETE' }); jobs.reload(); });
  };

  return <>
    <h3>Cron jobs</h3>
    <p className="hint">Run as the site's system user from its crontab. The schedule is five fields: minute, hour, day of month, month, day of week.</p>
    <StateView state={jobs} />
    {jobs.data && <table><thead><tr><th>Schedule</th><th>Command</th><th>State</th><th /></tr></thead><tbody>
      {jobs.data.length === 0 ? <tr><td colSpan={4} className="empty">No cron jobs for this site.</td></tr>
        : jobs.data.map((job) => <tr key={job.id}>
          <td className="mono">{job.schedule}</td><td className="mono">{job.command}</td>
          <td>{job.enabled ? 'enabled' : 'paused'}</td>
          <td>
            <button className="link" disabled={action.busy} onClick={() => toggle(job)}>{job.enabled ? 'pause' : 'resume'}</button>{' '}
            <button className="link danger" disabled={action.busy} onClick={() => remove(job)}>delete</button>
          </td>
        </tr>)}
    </tbody></table>}
    <form className="toolbar" onSubmit={create}>
      <input required aria-label="Cron schedule" className="mono" placeholder="*/5 * * * *" value={schedule} onChange={(e) => setSchedule(e.target.value)} />
      <input required aria-label="Cron command" className="mono" placeholder="php artisan schedule:run" value={command} onChange={(e) => setCommand(e.target.value)} />
      <button className="ghost" disabled={action.busy}>Add cron job</button>
    </form>
    <ErrorLine error={action.error} />
  </>;
}

function Firewall() {
  const rules = useApi<FirewallRule[]>('/firewall');
  const [port, setPort] = useState('');
  const [protocol, setProtocol] = useState<FirewallRule['protocol']>('tcp');
  const [allow, setAllow] = useState(true);
  const [description, setDescription] = useState('');
  const action = useAction();

  const apply = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      await api('/firewall', { method: 'POST', body: { port: Number(port), protocol, allow, description: description.trim() } });
      setPort(''); setDescription('');
      rules.reload();
    });
  };
  const remove = (rule: FirewallRule) => {
    if (!window.confirm(`Remove the ${rule.allow ? 'allow' : 'deny'} rule for ${rule.port}/${rule.protocol} from UFW?`)) return;
    action.run(async () => { await api(`/firewall/${rule.id}`, { method: 'DELETE' }); rules.reload(); });
  };

  return <section className="panel">
    <div className="panel-title"><h2>Firewall</h2></div>
    <p className="hint">UFW rules on this host. Applying a rule for a port and protocol that already has one replaces it (allow ↔ deny).</p>
    <StateView state={rules} />
    {rules.data && <table><thead><tr><th>Port</th><th>Protocol</th><th>Rule</th><th>Description</th><th /></tr></thead><tbody>
      {rules.data.length === 0 ? <tr><td colSpan={5} className="empty">No rules applied through the panel.</td></tr>
        : rules.data.map((rule) => <tr key={rule.id}>
          <td className="mono">{rule.port}</td><td>{rule.protocol}</td>
          <td><span className={`badge ${rule.allow ? 'succeeded' : 'failed'}`}>{rule.allow ? 'allow' : 'deny'}</span></td>
          <td>{rule.description || '—'}</td>
          <td><button className="link danger" disabled={action.busy} onClick={() => remove(rule)}>remove</button></td>
        </tr>)}
    </tbody></table>}
    <form className="toolbar" onSubmit={apply}>
      <input required aria-label="Port" type="number" min={1} max={65535} placeholder="port" value={port} onChange={(e) => setPort(e.target.value)} />
      <select aria-label="Protocol" value={protocol} onChange={(e) => setProtocol(e.target.value as FirewallRule['protocol'])}>
        <option value="tcp">tcp</option><option value="udp">udp</option>
      </select>
      <select aria-label="Rule" value={allow ? 'allow' : 'deny'} onChange={(e) => setAllow(e.target.value === 'allow')}>
        <option value="allow">allow</option><option value="deny">deny</option>
      </select>
      <input aria-label="Description" placeholder="description" maxLength={200} value={description} onChange={(e) => setDescription(e.target.value)} />
      <button className="ghost" disabled={action.busy}>Apply rule</button>
    </form>
    <ErrorLine error={action.error} />
  </section>;
}
