import { FormEvent, useEffect, useState } from 'react';
import { api, ComposeProject, formatTime, useApi } from './api';

export type Destination = {
  id: string; name: string; endpoint: string; region: string; bucket: string; prefix: string;
  retention_count: number; has_credential: boolean; created_at: string; updated_at: string;
};

type Run = {
  id: string; destination_id: string; kind: string; source: string; status: string;
  object_key: string | null; size_bytes: number | null; pruned: number; error: string | null;
  started_at: string; finished_at: string | null; trigger: 'manual' | 'schedule'; schedule_id: string | null;
};

type BackupObject = { key: string; size_bytes: number; last_modified: string };
type TestResult = { ok: boolean; message: string };
type Engine = 'postgres' | 'mysql' | 'mongodb';

const PRESETS: [string, string, string][] = [
  ['AWS S3', 'https://s3.<region>.amazonaws.com', '<region>'],
  ['Cloudflare R2', 'https://<account>.r2.cloudflarestorage.com', 'auto'],
  ['MinIO', 'http://127.0.0.1:9000', 'us-east-1'],
];

const KIND_LABELS: Record<string, string> = {
  database_backup: 'DB backup', database_restore: 'DB restore',
  volume_backup: 'Volume backup', volume_restore: 'Volume restore',
};

const size = (bytes: number | null) => {
  if (bytes == null) return '—';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes / 1024; let i = 0;
  while (value >= 1024 && i < units.length - 1) { value /= 1024; i += 1; }
  return `${value.toFixed(1)} ${units[i]}`;
};

export function Backups({ docker }: { docker: boolean }) {
  const destinations = useApi<Destination[]>('/backups/destinations');
  const [editing, setEditing] = useState<Destination | 'new' | null>(null);
  const [selected, setSelected] = useState<Destination>();
  const [message, setMessage] = useState<string>();
  const [busy, setBusy] = useState(false);

  const run = async (action: () => Promise<string | undefined>) => {
    setBusy(true); setMessage(undefined);
    try { setMessage(await action()); } catch (e) { setMessage((e as Error).message); } finally { setBusy(false); }
  };
  const test = (d: Destination) => run(async () => {
    const result = await api<TestResult>(`/backups/destinations/${d.id}/test`, { method: 'POST' });
    return result.ok ? `${d.name}: bucket reachable.` : `${d.name}: ${result.message}`;
  });
  const remove = (d: Destination) => {
    if (!window.confirm(`Delete destination ${d.name}? Backups already in the bucket are kept.`)) return;
    run(async () => {
      await api(`/backups/destinations/${d.id}`, { method: 'DELETE' });
      if (selected?.id === d.id) setSelected(undefined);
      destinations.reload();
      return undefined;
    });
  };

  return <>
    <section className="panel">
      <div className="panel-title">
        <h2>S3 backup destinations</h2>
        {editing === null && <button className="action" onClick={() => setEditing('new')}>Add destination</button>}
      </div>
      <p className="hint">Backups stream straight to S3-compatible storage (AWS S3, Cloudflare R2, MinIO) without a local dump file. Access keys are write-only and stored encrypted.</p>
      {editing !== null && <DestinationForm destination={editing === 'new' ? undefined : editing}
        onDone={() => { setEditing(null); destinations.reload(); }} />}
      {destinations.loading && <p className="state">Loading…</p>}
      {destinations.error && <p className="state error">{destinations.error}</p>}
      {message && <p className="state" role="status">{message}</p>}
      {destinations.data && (destinations.data.length === 0 ? <p className="state">No destinations yet.</p>
        : <table><thead><tr><th>Name</th><th>Bucket</th><th>Prefix</th><th>Keep</th><th /></tr></thead><tbody>
          {destinations.data.map((d) => <tr key={d.id}>
            <td>{d.name}</td>
            <td className="mono small">{d.bucket} <span className="hint">({d.endpoint})</span></td>
            <td className="mono small">{d.prefix}</td>
            <td className="small">{d.retention_count === 0 ? 'all' : d.retention_count}</td>
            <td>
              <button className="link" disabled={busy} onClick={() => setSelected(d)}>backups</button>{' '}
              <button className="link" disabled={busy} onClick={() => test(d)}>test</button>{' '}
              <button className="link" disabled={busy} onClick={() => setEditing(d)}>edit</button>{' '}
              <button className="link danger" disabled={busy} onClick={() => remove(d)}>delete</button>
            </td>
          </tr>)}
        </tbody></table>)}
    </section>
    {destinations.data && <ScheduledBackups destinations={destinations.data} docker={docker} />}
    {selected && <DestinationDetail key={selected.id} destination={selected} docker={docker} />}
  </>;
}

function DestinationForm({ destination, onDone }: { destination?: Destination; onDone: () => void }) {
  const [name, setName] = useState(destination?.name ?? '');
  const [endpoint, setEndpoint] = useState(destination?.endpoint ?? '');
  const [region, setRegion] = useState(destination?.region ?? '');
  const [bucket, setBucket] = useState(destination?.bucket ?? '');
  const [prefix, setPrefix] = useState(destination?.prefix ?? 'scontrol');
  const [retention, setRetention] = useState(String(destination?.retention_count ?? 7));
  const [accessKey, setAccessKey] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [error, setError] = useState<string>();
  const [result, setResult] = useState<TestResult>();
  const [busy, setBusy] = useState(false);

  // Stored keys are never sent to another endpoint or bucket.
  const keysHint = !destination ? '' : endpoint.trim() !== destination.endpoint || bucket.trim() !== destination.bucket
    ? 'enter it again: the endpoint or bucket changed' : 'stored; leave empty to keep';
  const body = () => ({
    name: name.trim(), endpoint: endpoint.trim(), region: region.trim(), bucket: bucket.trim(),
    prefix: prefix.trim(), retention_count: Number(retention),
    access_key: accessKey || undefined, secret_key: secretKey || undefined,
  });
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true); setError(undefined);
    try {
      await api(destination ? `/backups/destinations/${destination.id}` : '/backups/destinations',
        { method: destination ? 'PUT' : 'POST', body: body() });
      setSecretKey('');
      onDone();
    } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  const testTyped = async () => {
    setBusy(true); setError(undefined); setResult(undefined);
    try { setResult(await api<TestResult>('/backups/destinations/test', { method: 'POST', body: body() })); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };

  return <form className="card" onSubmit={submit}>
    {!destination && <div className="segmented" role="group" aria-label="Presets">
      {PRESETS.map(([label, url, reg]) => <button type="button" key={label}
        onClick={() => { setEndpoint(url); setRegion(reg); if (!name) setName(label); }}>{label}</button>)}
    </div>}
    <div className="grid2">
      <label>Name<input required maxLength={100} value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label>Endpoint<input required className="mono" placeholder="https://s3.eu-west-1.amazonaws.com" value={endpoint} onChange={(e) => setEndpoint(e.target.value)} /></label>
      <label>Region<input required className="mono" placeholder="auto" value={region} onChange={(e) => setRegion(e.target.value)} /></label>
      <label>Bucket<input required className="mono" value={bucket} onChange={(e) => setBucket(e.target.value)} /></label>
      <label>Key prefix<input required className="mono" value={prefix} onChange={(e) => setPrefix(e.target.value)} /></label>
      <label>Backups to keep (0 = all)<input type="number" min={0} max={1000} required value={retention} onChange={(e) => setRetention(e.target.value)} /></label>
      <label>Access key ID<input className="mono" autoComplete="off" required={!destination} value={accessKey}
        placeholder={keysHint} onChange={(e) => setAccessKey(e.target.value)} /></label>
      <label>Secret access key<input type="password" autoComplete="new-password" required={!destination} value={secretKey}
        placeholder={keysHint} onChange={(e) => setSecretKey(e.target.value)} /></label>
    </div>
    {error && <p className="state error" role="alert">{error}</p>}
    {result && <p className={result.ok ? 'state' : 'state error'} role="status">{result.ok ? 'Bucket reachable.' : `Check failed: ${result.message}`}</p>}
    <div className="toolbar">
      <button className="action" disabled={busy}>{destination ? 'Save destination' : 'Create destination'}</button>
      <button type="button" className="ghost" disabled={busy || !accessKey || !secretKey || !endpoint || !bucket} onClick={testTyped}>Test connection</button>
      <button type="button" className="ghost" onClick={onDone}>Cancel</button>
    </div>
  </form>;
}

function DestinationDetail({ destination, docker }: { destination: Destination; docker: boolean }) {
  const objects = useApi<BackupObject[]>(`/backups/destinations/${destination.id}/objects`);
  const runs = useApi<Run[]>(`/backups/runs?destination_id=${destination.id}`);
  const [restoring, setRestoring] = useState<BackupObject>();
  const running = runs.data?.some((r) => r.status === 'running');

  // Poll while a run is in flight.
  useEffect(() => {
    if (!running) return undefined;
    const timer = window.setInterval(() => runs.reload(), 3000);
    return () => window.clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [running]);
  // Refresh the object list whenever a run has finished. Counting finished
  // runs (not watching `running`) also catches a run that ended before the
  // first poll ever saw it running.
  const finished = runs.data?.filter((r) => r.status !== 'running').map((r) => r.id).join(',');
  const reloadObjects = objects.reload;
  useEffect(() => { if (finished) reloadObjects(); }, [finished, reloadObjects]);

  return <section className="panel">
    <div className="panel-title"><h2>{destination.name}: backups</h2></div>
    <BackupForms destination={destination} docker={docker} onStarted={() => runs.reload()} />
    <h3>Recent runs</h3>
    {runs.error && <p className="state error">{runs.error}</p>}
    {runs.data && (runs.data.length === 0 ? <p className="state">No runs yet.</p>
      : <table><thead><tr><th>Started</th><th>Kind</th><th>Source</th><th>Status</th><th>Size</th><th>Object</th></tr></thead><tbody>
        {runs.data.map((r) => <tr key={r.id}>
          <td className="small">{formatTime(r.started_at)}</td>
          <td>{KIND_LABELS[r.kind] ?? r.kind}{r.trigger === 'schedule' ? <span className="muted small"> · scheduled</span> : null}</td>
          <td className="mono small">{r.source}</td>
          <td className={r.status === 'failed' ? 'error' : undefined} title={r.error ?? undefined}>
            {r.status}{r.error ? `: ${r.error}` : ''}{r.pruned > 0 ? ` (pruned ${r.pruned})` : ''}
          </td>
          <td className="small">{size(r.size_bytes)}</td>
          <td className="mono small">{r.object_key ?? ''}</td>
        </tr>)}
      </tbody></table>)}
    <h3>Backups in the bucket</h3>
    {objects.loading && <p className="state">Loading…</p>}
    {objects.error && <p className="state error">{objects.error}</p>}
    {objects.data && (objects.data.length === 0 ? <p className="state">Nothing stored under <span className="mono">{destination.prefix}/</span> yet.</p>
      : <table><thead><tr><th>Object</th><th>Size</th><th>Modified</th><th /></tr></thead><tbody>
        {objects.data.map((o) => <tr key={o.key}>
          <td className="mono small">{o.key}</td><td className="small">{size(o.size_bytes)}</td>
          <td className="small">{formatTime(o.last_modified)}</td>
          <td><button className="link" onClick={() => setRestoring(o)}>restore</button></td>
        </tr>)}
      </tbody></table>)}
    {restoring && <RestoreForm destination={destination} object={restoring} docker={docker}
      onDone={() => { setRestoring(undefined); runs.reload(); }} />}
  </section>;
}

function BackupForms({ destination, docker, onStarted }: { destination: Destination; docker: boolean; onStarted: () => void }) {
  const [mode, setMode] = useState<'database' | 'volume'>('database');
  const [message, setMessage] = useState<string>();
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const submit = async (event: FormEvent, path: string, body: unknown) => {
    event.preventDefault();
    setBusy(true); setError(undefined); setMessage(undefined);
    try { await api(`/backups/destinations/${destination.id}/${path}`, { method: 'POST', body }); setMessage('Backup started.'); onStarted(); }
    catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  return <div className="card">
    <div className="segmented" role="group" aria-label="Backup type">
      <button type="button" className={mode === 'database' ? 'selected' : ''} onClick={() => setMode('database')}>Database</button>
      <button type="button" className={mode === 'volume' ? 'selected' : ''} onClick={() => setMode('volume')} disabled={!docker}>Volume</button>
    </div>
    {mode === 'database'
      ? <DatabaseFields busy={busy} label="Back up now" withService onSubmit={(body) => (e) => submit(e, 'database/backup', body)} />
      : <VolumeFields busy={busy} label="Back up now" onSubmit={(body) => (e) => submit(e, 'volume/backup', body)} />}
    {message && <p className="state" role="status">{message}</p>}
    {error && <p className="state error" role="alert">{error}</p>}
  </div>;
}

function RestoreForm({ destination, object, docker, onDone }: { destination: Destination; object: BackupObject; docker: boolean; onDone: () => void }) {
  const isVolume = object.key.endsWith('.tar.gz');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const submit = (body: Record<string, unknown>) => async (event: FormEvent) => {
    event.preventDefault();
    if (!window.confirm(`Restore ${object.key}? This overwrites the current ${isVolume ? 'volume contents' : 'database contents'}.`)) return;
    setBusy(true); setError(undefined);
    try {
      await api(`/backups/destinations/${destination.id}/${isVolume ? 'volume' : 'database'}/restore`,
        { method: 'POST', body: { ...body, key: object.key } });
      onDone();
    } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  return <div className="card">
    <h3>Restore <span className="mono">{object.key}</span></h3>
    {isVolume && !docker && <p className="state error">Docker was not selected at install time.</p>}
    {isVolume
      ? <VolumeFields busy={busy || !docker} label="Restore" onSubmit={(body) => submit(body)} />
      : <DatabaseFields busy={busy} label="Restore" onSubmit={(body) => submit(body)} />}
    {error && <p className="state error" role="alert">{error}</p>}
    <div className="toolbar"><button type="button" className="ghost" onClick={onDone}>Cancel</button></div>
  </div>;
}

function DatabaseFields({ busy, label, withService, onSubmit }: { busy: boolean; label: string; withService?: boolean; onSubmit: (body: Record<string, unknown>) => (e: FormEvent) => void }) {
  const [engine, setEngine] = useState<Engine>('postgres');
  const [host, setHost] = useState('127.0.0.1');
  const [port, setPort] = useState('5432');
  const [database, setDatabase] = useState('');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [service, setService] = useState('');
  const pick = (value: Engine) => { setEngine(value); setPort({ postgres: '5432', mysql: '3306', mongodb: '27017' }[value]); };
  return <form onSubmit={onSubmit({ engine, host: host.trim(), port: Number(port), database: database.trim(), username: username.trim(), password, service: (withService && service.trim()) || undefined })}>
    <div className="grid2">
      <label>Engine<select value={engine} onChange={(e) => pick(e.target.value as Engine)}>
        <option value="postgres">PostgreSQL</option><option value="mysql">MySQL / MariaDB</option><option value="mongodb">MongoDB</option>
      </select></label>
      <label>Host<input required className="mono" value={host} onChange={(e) => setHost(e.target.value)} /></label>
      <label>Port<input required type="number" min={1} max={65535} value={port} onChange={(e) => setPort(e.target.value)} /></label>
      <label>Database<input required className="mono" value={database} onChange={(e) => setDatabase(e.target.value)} /></label>
      <label>Username<input required className="mono" autoComplete="off" value={username} onChange={(e) => setUsername(e.target.value)} /></label>
      <label>Password<input required type="password" autoComplete="new-password" value={password} onChange={(e) => setPassword(e.target.value)} /></label>
      {withService && <label>Service slug (optional)<input className="mono" value={service} onChange={(e) => setService(e.target.value)} /></label>}
    </div>
    <p className="hint">The password is used for this run only and is never stored.</p>
    {withService && <p className="hint">Backups are kept per service: a managed database service is recognised by its port, otherwise name the service or the host and port are used, so retention never removes another service's backups.</p>}
    <div className="toolbar"><button className="action" disabled={busy}>{label}</button></div>
  </form>;
}

function VolumeFields({ busy, label, onSubmit }: { busy: boolean; label: string; onSubmit: (body: Record<string, unknown>) => (e: FormEvent) => void }) {
  const [project, setProject] = useState('');
  const [volume, setVolume] = useState('');
  const [service, setService] = useState('');
  const [stop, setStop] = useState(true);
  return <form onSubmit={onSubmit({ project_slug: project.trim(), volume: volume.trim(), service: service.trim() || undefined, stop_service: stop && service.trim() !== '' })}>
    <div className="grid2">
      <label>Project slug<input required className="mono" value={project} onChange={(e) => setProject(e.target.value)} /></label>
      <label>Volume name<input required className="mono" value={volume} onChange={(e) => setVolume(e.target.value)} /></label>
      <label>Service using it<input className="mono" value={service} onChange={(e) => setService(e.target.value)} /></label>
      <label className="check"><input type="checkbox" checked={stop} onChange={(e) => setStop(e.target.checked)} /> Stop the service during the copy (restarted afterwards)</label>
    </div>
    <div className="toolbar"><button className="action" disabled={busy}>{label}</button></div>
  </form>;
}

export type BackupSchedule = {
  id: string; name: string; target_kind: 'database' | 'volume'; compose_project_id: string;
  project_slug: string | null; project_name: string | null; server_id: string | null;
  database_engine: string | null; database_name: string | null;
  volume_name: string | null; service_name: string | null; stop_service: boolean;
  s3_destination_id: string; destination_name: string | null;
  cron_expression: string; timezone: string; retention_count: number; enabled: boolean;
  next_run_at: string | null; last_run_at: string | null; last_run_id: string | null;
  last_run_status: string | null; last_run_error: string | null;
};

type DatabaseServiceInfo = { project: ComposeProject; spec: { engine: string; database_name: string } };

const BACKUP_CRON_PRESETS: [string, string][] = [
  ['0 3 * * *', 'Daily at 03:00'], ['0 */6 * * *', 'Every 6 hours'], ['@hourly', 'Hourly'],
  ['0 4 * * 0', 'Weekly (Sunday 04:00)'], ['0 5 1 * *', 'Monthly (1st, 05:00)'],
];
const browserZone = () => Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';
const zoneList = (): string[] => {
  const intl = Intl as unknown as { supportedValuesOf?: (key: string) => string[] };
  return ['UTC', ...(intl.supportedValuesOf?.('timeZone') ?? [])];
};

const describeTarget = (s: BackupSchedule) => s.target_kind === 'database'
  ? `${s.project_slug ?? '(removed service)'} · ${s.database_engine ?? 'db'}/${s.database_name ?? '?'}`
  : `${s.project_slug ?? '(removed service)'} · volume ${s.volume_name}${s.service_name ? ` (${s.service_name}${s.stop_service ? ', stopped' : ''})` : ''}`;

function ScheduledBackups({ destinations, docker }: { destinations: Destination[]; docker: boolean }) {
  const schedules = useApi<BackupSchedule[]>('/backups/schedules', 10000);
  const [editing, setEditing] = useState<BackupSchedule | 'new' | null>(null);
  const [history, setHistory] = useState<BackupSchedule>();
  const [message, setMessage] = useState<string>();
  const [busy, setBusy] = useState(false);

  const act = async (action: () => Promise<string | undefined>) => {
    setBusy(true); setMessage(undefined);
    try { setMessage(await action()); schedules.reload(); } catch (e) { setMessage((e as Error).message); } finally { setBusy(false); }
  };
  const runNow = (s: BackupSchedule) => act(async () => {
    await api(`/backups/schedules/${s.id}/run`, { method: 'POST' });
    setHistory(s);
    return `${s.name}: backup started.`;
  });
  const toggle = (s: BackupSchedule) => act(async () => {
    await api(`/backups/schedules/${s.id}`, { method: 'PUT', body: { enabled: !s.enabled } });
    return `${s.name}: ${s.enabled ? 'paused' : 'resumed'}.`;
  });
  const remove = (s: BackupSchedule) => {
    if (!window.confirm(`Delete schedule ${s.name}? Past runs and backups in the bucket are kept.`)) return;
    act(async () => {
      await api(`/backups/schedules/${s.id}`, { method: 'DELETE' });
      if (history?.id === s.id) setHistory(undefined);
      return undefined;
    });
  };

  return <section className="panel">
    <div className="panel-title">
      <h2>Scheduled backups</h2>
      {editing === null && <button className="action" disabled={destinations.length === 0} onClick={() => setEditing('new')}>Add schedule</button>}
    </div>
    <p className="hint">The panel runs these on a cron schedule with the service's own stored credentials, then keeps only the newest backups of that service. A run whose previous one is still going is skipped.</p>
    {destinations.length === 0 && <p className="state">Add an S3 destination first.</p>}
    {editing !== null && <ScheduleForm schedule={editing === 'new' ? undefined : editing} destinations={destinations} docker={docker}
      onDone={() => { setEditing(null); schedules.reload(); }} />}
    {schedules.loading && !schedules.data && <p className="state">Loading…</p>}
    {schedules.error && <p className="state error">{schedules.error}</p>}
    {message && <p className="state" role="status">{message}</p>}
    {schedules.data && (schedules.data.length === 0 ? <p className="state">No scheduled backups yet.</p>
      : <table><thead><tr><th>Name</th><th>Target</th><th>Schedule</th><th>Destination</th><th>Keep</th><th>Next run</th><th>Last run</th><th /></tr></thead><tbody>
        {schedules.data.map((s) => <tr key={s.id}>
          <td>{s.name}</td>
          <td className="mono small">{describeTarget(s)}</td>
          <td><span className="mono">{s.cron_expression}</span><div className="muted small">{s.timezone}{s.enabled ? '' : ' · paused'}</div></td>
          <td className="small">{s.destination_name ?? '—'}</td>
          <td className="small">{s.retention_count === 0 ? 'all' : s.retention_count}</td>
          <td className="small">{s.enabled ? formatTime(s.next_run_at) : 'paused'}</td>
          <td className={`small${s.last_run_status === 'failed' ? ' error' : ''}`} title={s.last_run_error ?? undefined}>
            {s.last_run_status ? `${s.last_run_status} · ${formatTime(s.last_run_at)}` : 'never'}
          </td>
          <td>
            <button className="link" disabled={busy || s.last_run_status === 'running'} onClick={() => runNow(s)}>run now</button>{' '}
            <button className="link" disabled={busy} onClick={() => setHistory(s)}>runs</button>{' '}
            <button className="link" disabled={busy} onClick={() => toggle(s)}>{s.enabled ? 'pause' : 'resume'}</button>{' '}
            <button className="link" disabled={busy} onClick={() => setEditing(s)}>edit</button>{' '}
            <button className="link danger" disabled={busy} onClick={() => remove(s)}>delete</button>
          </td>
        </tr>)}
      </tbody></table>)}
    {history && <ScheduleRuns key={history.id} schedule={history} onClose={() => setHistory(undefined)} />}
  </section>;
}

function ScheduleRuns({ schedule, onClose }: { schedule: BackupSchedule; onClose: () => void }) {
  const runs = useApi<Run[]>(`/backups/runs?schedule_id=${schedule.id}`, 3000);
  return <div className="card">
    <div className="panel-title"><h3>Runs of {schedule.name}</h3><button className="ghost" onClick={onClose}>Close</button></div>
    {runs.error && <p className="state error">{runs.error}</p>}
    {runs.data && (runs.data.length === 0 ? <p className="state">No runs yet.</p>
      : <table><thead><tr><th>Started</th><th>Status</th><th>Size</th><th>Object</th></tr></thead><tbody>
        {runs.data.map((r) => <tr key={r.id}>
          <td className="small">{formatTime(r.started_at)}</td>
          <td className={r.status === 'failed' ? 'error' : undefined}>
            {r.status}{r.error ? `: ${r.error}` : ''}{r.pruned > 0 ? ` (pruned ${r.pruned})` : ''}
          </td>
          <td className="small">{size(r.size_bytes)}</td>
          <td className="mono small">{r.object_key ?? ''}</td>
        </tr>)}
      </tbody></table>)}
  </div>;
}

function ScheduleForm({ schedule, destinations, docker, onDone }: { schedule?: BackupSchedule; destinations: Destination[]; docker: boolean; onDone: () => void }) {
  const databases = useApi<DatabaseServiceInfo[]>(schedule ? null : '/database-services');
  const projects = useApi<ComposeProject[]>(schedule || !docker ? null : '/compose-projects');
  const [kind, setKind] = useState<'database' | 'volume'>(schedule?.target_kind ?? 'database');
  const [name, setName] = useState(schedule?.name ?? 'nightly');
  const [projectId, setProjectId] = useState(schedule?.compose_project_id ?? '');
  const [volume, setVolume] = useState(schedule?.volume_name ?? '');
  const [service, setService] = useState(schedule?.service_name ?? '');
  const [stop, setStop] = useState(schedule?.stop_service ?? false);
  const [destination, setDestination] = useState(schedule?.s3_destination_id ?? destinations[0]?.id ?? '');
  const [cron, setCron] = useState(schedule?.cron_expression ?? '0 3 * * *');
  const [timezone, setTimezone] = useState(schedule?.timezone ?? browserZone());
  const [retention, setRetention] = useState(String(schedule?.retention_count ?? 7));
  const [enabled, setEnabled] = useState(schedule?.enabled ?? true);
  const [preview, setPreview] = useState<{ runs?: string[]; error?: string }>({});
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [zones] = useState(zoneList);

  const dumpable = (databases.data ?? []).filter((d) => d.spec.engine !== 'redis');
  const volumeProjects = (projects.data ?? []).filter((p) => p.source.kind !== 'static');

  // Show the next run times a moment after typing stops.
  useEffect(() => {
    if (!cron.trim()) { setPreview({}); return undefined; }
    let active = true;
    const timer = window.setTimeout(() => {
      api<{ next_runs: string[] }>('/backups/schedules/preview', { method: 'POST', body: { cron_expression: cron.trim(), timezone: timezone.trim() } })
        .then((r) => active && setPreview({ runs: r.next_runs }))
        .catch((e: Error) => active && setPreview({ error: e.message }));
    }, 300);
    return () => { active = false; window.clearTimeout(timer); };
  }, [cron, timezone]);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true); setError(undefined);
    const common = {
      name: name.trim(), s3_destination_id: destination, cron_expression: cron.trim(),
      timezone: timezone.trim(), retention_count: Number(retention), enabled,
    };
    const volumeFields = kind === 'volume'
      ? { service_name: service.trim() || null, stop_service: stop && service.trim() !== '' } : {};
    try {
      if (schedule) {
        await api(`/backups/schedules/${schedule.id}`, { method: 'PUT', body: { ...common, ...volumeFields } });
      } else {
        await api('/backups/schedules', {
          method: 'POST',
          body: { ...common, target_kind: kind, compose_project_id: projectId,
            ...(kind === 'volume' ? { volume_name: volume.trim(), service_name: service.trim() || undefined, stop_service: stop && service.trim() !== '' } : {}) },
        });
      }
      onDone();
    } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };

  return <form className="card" onSubmit={submit}>
    {schedule
      ? <p className="hint">Target: <span className="mono">{describeTarget(schedule)}</span> (fixed for the life of a schedule).</p>
      : <>
        <div className="segmented" role="group" aria-label="Backup target">
          <button type="button" className={kind === 'database' ? 'selected' : ''} onClick={() => { setKind('database'); setProjectId(''); }}>Database service</button>
          <button type="button" className={kind === 'volume' ? 'selected' : ''} disabled={!docker} onClick={() => { setKind('volume'); setProjectId(''); }}>Volume</button>
        </div>
        {kind === 'database' && databases.error && <p className="state error">{databases.error}</p>}
        {kind === 'database' && databases.data && dumpable.length === 0 && <p className="state">No managed PostgreSQL, MySQL, MariaDB or MongoDB service yet. Other databases can be backed up by their volume.</p>}
        {kind === 'volume' && projects.error && <p className="state error">{projects.error}</p>}
      </>}
    <div className="grid2">
      <label>Name<input required maxLength={100} value={name} onChange={(e) => setName(e.target.value)} /></label>
      {!schedule && kind === 'database' && <label>Database service<select required value={projectId} onChange={(e) => setProjectId(e.target.value)}>
        <option value="">Choose…</option>
        {dumpable.map((d) => <option key={d.project.id} value={d.project.id}>{d.project.name} ({d.spec.engine}/{d.spec.database_name})</option>)}
      </select></label>}
      {!schedule && kind === 'volume' && <>
        <label>Service<select required value={projectId} onChange={(e) => setProjectId(e.target.value)}>
          <option value="">Choose…</option>
          {volumeProjects.map((p) => <option key={p.id} value={p.id}>{p.name} ({p.slug})</option>)}
        </select></label>
        <label>Volume name<input required className="mono" pattern="[A-Za-z0-9][A-Za-z0-9_.\-]*" value={volume} onChange={(e) => setVolume(e.target.value)} /></label>
      </>}
      {kind === 'volume' && <>
        <label>Container using it (optional)<input className="mono" value={service} onChange={(e) => setService(e.target.value)} /></label>
        <label className="check"><input type="checkbox" checked={stop} onChange={(e) => setStop(e.target.checked)} /> Stop that container during the copy</label>
      </>}
      <label>S3 destination<select required value={destination} onChange={(e) => setDestination(e.target.value)}>
        {destinations.map((d) => <option key={d.id} value={d.id}>{d.name}</option>)}
      </select></label>
      <label>Backups to keep (0 = all)<input type="number" min={0} max={1000} required value={retention} onChange={(e) => setRetention(e.target.value)} /></label>
    </div>
    <fieldset><legend>Schedule</legend>
      <div className="toolbar">
        <select aria-label="Preset" value="" onChange={(e) => e.target.value && setCron(e.target.value)}>
          <option value="">Presets…</option>
          {BACKUP_CRON_PRESETS.map(([value, label]) => <option key={value} value={value}>{label}</option>)}
        </select>
      </div>
      <div className="grid2">
        <label>Cron expression (minute hour day month weekday)<input required className="mono" value={cron} onChange={(e) => setCron(e.target.value)} /></label>
        <label>Time zone<input required className="mono" list="backup-zones" value={timezone} onChange={(e) => setTimezone(e.target.value)} /></label>
        <datalist id="backup-zones">{zones.map((z) => <option key={z} value={z} />)}</datalist>
      </div>
      {preview.error ? <p className="state error">{preview.error}</p>
        : preview.runs && <p className="hint">Next runs: {preview.runs.map(formatTime).join(' · ')}</p>}
      <label className="check"><input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} /> Enabled</label>
    </fieldset>
    {error && <p className="state error" role="alert">{error}</p>}
    <div className="toolbar">
      <button className="action" disabled={busy}>{schedule ? 'Save schedule' : 'Create schedule'}</button>
      <button type="button" className="ghost" onClick={onDone}>Cancel</button>
    </div>
  </form>;
}
