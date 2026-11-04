import { FormEvent, useState } from 'react';
import {
  api, ComposeProject, DATABASE_ENGINE_LABELS, DatabaseEngine, DatabaseSpec, Deployment, formatTime, isActive, useApi,
} from './api';
import { placement, ServerSelect } from './Servers';

type EngineInfo = {
  engine: DatabaseEngine; versions: string[]; default_version: string; container_port: number; has_database_and_user: boolean;
};
type Exposure = { public_port: number; allow_from: string[] };
type DatabaseService = { project: ComposeProject; spec: DatabaseSpec; loopback_port: number; exposure: Exposure | null };
type Credentials = {
  username: string; password: string; root_password: string | null; database: string | null;
  host: string; port: number; uri: string; external_uri: string | null;
};

const slugify = (name: string) => name.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 40);

export function Databases() {
  const services = useApi<DatabaseService[]>('/database-services', 5000);
  const engines = useApi<EngineInfo[]>('/database-services/engines');
  const [creating, setCreating] = useState(false);
  const [selected, setSelected] = useState<string>();

  const current = services.data?.find((s) => s.project.id === selected);

  return <section className="panel">
    <div className="panel-title">
      <h2>Databases</h2>
      {!creating && <button className="action" onClick={() => setCreating(true)}>New database</button>}
    </div>
    <p className="hint">PostgreSQL, MySQL, MariaDB, MongoDB, and Redis in their own containers. Each listens on <span className="mono">127.0.0.1</span> only and keeps its data in a named volume; credentials are generated and stored encrypted. Outside access is off until you open it for specific addresses.</p>
    {creating && <CreateForm engines={engines.data} onDone={(id) => { setCreating(false); services.reload(); if (id) setSelected(id); }} />}
    {services.loading && <p className="state">Loading…</p>}
    {(services.error || engines.error) && <p className="state error">{services.error ?? engines.error}</p>}
    {services.data && (services.data.length === 0 ? <p className="state">No databases yet.</p>
      : <table><thead><tr><th>Name</th><th>Engine</th><th>Local address</th><th>Outside access</th><th>Status</th><th>Updated</th></tr></thead><tbody>
        {services.data.map((s) => <tr key={s.project.id} className={`clickable ${s.project.id === selected ? 'selected-row' : ''}`} onClick={() => setSelected(s.project.id)}>
          <td>{s.project.name}<div className="mono small muted">{s.project.slug}</div></td>
          <td>{DATABASE_ENGINE_LABELS[s.spec.engine]} {s.spec.version}</td>
          <td className="mono small">127.0.0.1:{s.loopback_port}</td>
          <td className="small">{s.exposure ? <>port <span className="mono">{s.exposure.public_port}</span> · {s.exposure.allow_from.length ? s.exposure.allow_from.join(', ') : 'any address'}</> : 'closed'}</td>
          <td><span className={`badge ${s.project.status}`}>{s.project.status}</span></td>
          <td className="small">{formatTime(s.project.updated_at)}</td>
        </tr>)}
      </tbody></table>)}
    {current && <Detail key={current.project.id} service={current} onChanged={services.reload} onRemoved={() => { setSelected(undefined); services.reload(); }} />}
  </section>;
}

function CreateForm({ engines, onDone }: { engines?: EngineInfo[]; onDone: (id?: string) => void }) {
  const [engine, setEngine] = useState<DatabaseEngine>('postgres');
  const info = engines?.find((e) => e.engine === engine);
  const [version, setVersion] = useState('');
  const [name, setName] = useState('');
  const [slug, setSlug] = useState('');
  const [database, setDatabase] = useState('app');
  const [username, setUsername] = useState('app');
  const [memory, setMemory] = useState('');
  const [serverId, setServerId] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true); setError(undefined);
    try {
      const created = await api<{ service: DatabaseService }>('/database-services', {
        method: 'POST',
        body: {
          name: name.trim(), slug: slug || slugify(name), engine,
          version: version || info?.default_version,
          database_name: info?.has_database_and_user === false ? undefined : database,
          username: info?.has_database_and_user === false ? undefined : username,
          memory_limit_mb: memory ? Number(memory) : undefined,
          ...placement(serverId),
        },
      });
      onDone(created.service.project.id);
    } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };

  return <form className="card" onSubmit={submit}>
    <div className="segmented" role="group" aria-label="Engine">
      {(Object.keys(DATABASE_ENGINE_LABELS) as DatabaseEngine[]).map((e) => <button type="button" key={e}
        className={engine === e ? 'selected' : ''} onClick={() => { setEngine(e); setVersion(''); }}>{DATABASE_ENGINE_LABELS[e]}</button>)}
    </div>
    <div className="grid2">
      <label>Name<input required maxLength={100} value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label>Slug<input className="mono" pattern="[a-z][a-z0-9-]{2,47}" placeholder={slugify(name) || 'shop-db'} value={slug} onChange={(e) => setSlug(e.target.value)} /></label>
      <label>Version
        <select value={version || info?.default_version || ''} onChange={(e) => setVersion(e.target.value)} disabled={!info}>
          {info?.versions.map((v) => <option key={v} value={v}>{v}</option>)}
        </select>
      </label>
      <label>Memory limit (MiB) <span className="muted small">(optional)</span>
        <input type="number" min={64} value={memory} onChange={(e) => setMemory(e.target.value)} />
      </label>
      <ServerSelect value={serverId} onChange={setServerId} />
      {info?.has_database_and_user !== false && <>
        <label>Database name<input required className="mono" pattern="[A-Za-z][A-Za-z0-9_]{0,62}" value={database} onChange={(e) => setDatabase(e.target.value)} /></label>
        <label>Username<input required className="mono" pattern="[A-Za-z][A-Za-z0-9_]{0,62}" value={username} onChange={(e) => setUsername(e.target.value)} /></label>
      </>}
    </div>
    <p className="hint">The password{engine === 'mysql' || engine === 'mariadb' ? ' and the root password are' : ' is'} generated. The container port {info ? <span className="mono">{info.container_port}</span> : null} is published on <span className="mono">127.0.0.1</span> only.</p>
    {error && <p className="state error" role="alert">{error}</p>}
    <div className="toolbar">
      <button className="action" disabled={busy || !info}>{busy ? 'Creating…' : 'Create and start'}</button>
      <button type="button" className="ghost" onClick={() => onDone()}>Cancel</button>
    </div>
  </form>;
}

function Detail({ service, onChanged, onRemoved }: { service: DatabaseService; onChanged: () => void; onRemoved: () => void }) {
  const { project } = service;
  const deployments = useApi<Deployment[]>(`/compose-projects/${project.id}/deployments`, 5000);
  const [credentials, setCredentials] = useState<Credentials>();
  const [message, setMessage] = useState<string>();
  const [busy, setBusy] = useState(false);
  const running = isActive(project.status);

  const run = async (action: () => Promise<string | undefined>) => {
    setBusy(true); setMessage(undefined);
    try { setMessage(await action()); onChanged(); deployments.reload(); }
    catch (e) { setMessage((e as Error).message); } finally { setBusy(false); }
  };
  const lifecycle = (action: 'start' | 'stop' | 'restart') => run(async () => {
    await api(`/database-services/${project.id}/${action}`, { method: 'POST' });
    return action === 'start' ? 'Start queued.' : action === 'stop' ? 'Stopped.' : 'Restarted.';
  });
  const remove = () => {
    if (!window.confirm(`Delete ${project.name}? The containers and settings are removed. The data volume is kept on the host until you remove it there.`)) return;
    run(async () => { await api(`/database-services/${project.id}`, { method: 'DELETE' }); onRemoved(); return undefined; });
  };
  const reveal = () => run(async () => {
    setCredentials(await api<Credentials>(`/database-services/${project.id}/credentials`, { method: 'POST' }));
    return undefined;
  });
  const copy = (value: string) => { navigator.clipboard?.writeText(value).then(() => setMessage('Copied.'), () => undefined); };

  return <div className="card">
    <div className="panel-title">
      <h2>{project.name} <span className={`badge ${project.status}`}>{project.status}</span></h2>
      <div className="toolbar">
        <button className="ghost" disabled={busy || running} onClick={() => lifecycle('start')}>Start</button>
        <button className="ghost" disabled={busy || running} onClick={() => lifecycle('restart')}>Restart</button>
        <button className="ghost" disabled={busy || running} onClick={() => lifecycle('stop')}>Stop</button>
        <button className="ghost danger" disabled={busy || running} onClick={remove}>Delete</button>
      </div>
    </div>
    <p className="hint">{DATABASE_ENGINE_LABELS[service.spec.engine]} {service.spec.version} · listens on <span className="mono">127.0.0.1:{service.loopback_port}</span>
      {service.spec.engine !== 'redis' && <> · database <span className="mono">{service.spec.database_name}</span> · user <span className="mono">{service.spec.username}</span></>}.
      Deployment history, logs, and extra variables are under Compose projects ({project.slug}).</p>
    {message && <p className="state" role="status">{message}</p>}

    <h3>Connection</h3>
    {!credentials
      ? <div className="toolbar"><button className="ghost" disabled={busy} onClick={reveal}>Show credentials</button><span className="hint">Showing them is recorded in the audit log.</span></div>
      : <div className="secret">
        <table><tbody>
          <tr><th>Host</th><td className="mono">{credentials.host}:{credentials.port}</td></tr>
          <tr><th>User</th><td className="mono">{credentials.username}</td></tr>
          {credentials.database && <tr><th>Database</th><td className="mono">{credentials.database}</td></tr>}
          <tr><th>Password</th><td className="mono">{credentials.password} <button className="link" onClick={() => copy(credentials.password)}>copy</button></td></tr>
          {credentials.root_password && <tr><th>Root password</th><td className="mono">{credentials.root_password} <button className="link" onClick={() => copy(credentials.root_password!)}>copy</button></td></tr>}
          <tr><th>URI</th><td className="mono">{credentials.uri} <button className="link" onClick={() => copy(credentials.uri)}>copy</button></td></tr>
          {credentials.external_uri && <tr><th>Outside URI</th><td className="mono">{credentials.external_uri} <button className="link" onClick={() => copy(credentials.external_uri!)}>copy</button></td></tr>}
        </tbody></table>
        <div className="toolbar"><button className="ghost" onClick={() => setCredentials(undefined)}>Hide</button></div>
      </div>}

    <ExposureEditor service={service} onChanged={() => { setCredentials(undefined); onChanged(); }} />

    <h3>Recent deployments</h3>
    {deployments.error && <p className="state error">{deployments.error}</p>}
    {deployments.data && (deployments.data.length === 0 ? <p className="state">Not deployed yet.</p>
      : <table><thead><tr><th>Queued</th><th>Trigger</th><th>Status</th><th>Error</th></tr></thead><tbody>
        {deployments.data.slice(0, 5).map((d) => <tr key={d.id}>
          <td className="small">{formatTime(d.queued_at)}</td><td className="small">{d.trigger}</td>
          <td><span className={`badge ${d.status}`}>{d.status}</span></td><td className="small">{d.error_message ?? ''}</td>
        </tr>)}
      </tbody></table>)}
  </div>;
}

function ExposureEditor({ service, onChanged }: { service: DatabaseService; onChanged: () => void }) {
  const [port, setPort] = useState(String(service.exposure?.public_port ?? ''));
  const [sources, setSources] = useState((service.exposure?.allow_from ?? []).join('\n'));
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const open = !!service.exposure;

  const save = async (event: FormEvent) => {
    event.preventDefault();
    const allow_from = sources.split(/[\s,]+/).map((s) => s.trim()).filter(Boolean);
    if (allow_from.length === 0 && !window.confirm('No source addresses: the database will accept connections from any address (password still required). Continue?')) return;
    setBusy(true); setError(undefined);
    try {
      await api(`/database-services/${service.project.id}/exposure`, { method: 'PUT', body: { public_port: Number(port), allow_from } });
      onChanged();
    } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  const close = async () => {
    setBusy(true); setError(undefined);
    try { await api(`/database-services/${service.project.id}/exposure`, { method: 'DELETE' }); setPort(''); setSources(''); onChanged(); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };

  return <form onSubmit={save}>
    <h3>Outside access {open ? <span className="badge succeeded">open</span> : <span className="badge stopped">closed</span>}</h3>
    <p className="hint">The host Nginx listens on the public port and forwards to the loopback port, and a UFW rule allows only the listed addresses (both check them). The container itself is never published on <span className="mono">0.0.0.0</span>.</p>
    <div className="grid2">
      <label>Public port<input required type="number" min={1024} max={65535} value={port} onChange={(e) => setPort(e.target.value)} /></label>
      <label>Allowed addresses or CIDR ranges <span className="muted small">(one per line; empty = any)</span>
        <textarea className="mono" rows={3} placeholder={'203.0.113.4\n10.0.0.0/8'} value={sources} onChange={(e) => setSources(e.target.value)} />
      </label>
    </div>
    {error && <p className="state error" role="alert">{error}</p>}
    <div className="toolbar">
      <button className="action" disabled={busy || !port}>{open ? 'Update access' : 'Open access'}</button>
      {open && <button type="button" className="ghost danger" disabled={busy} onClick={close}>Close access</button>}
    </div>
  </form>;
}
