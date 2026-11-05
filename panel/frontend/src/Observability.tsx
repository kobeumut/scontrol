import { useEffect, useState } from 'react';
import { api, formatTime, useApi } from './api';
import { ErrorLine, StateView, useAction } from './ui';

type HostTotals = {
  host: string; requests: number; status_2xx: number; status_3xx: number; status_4xx: number;
  status_5xx: number; status_other: number; bytes_sent: number; avg_latency_ms: number;
};
type Point = {
  minute: string; requests: number; status_2xx: number; status_3xx: number; status_4xx: number;
  status_5xx: number; bytes_sent: number; avg_latency_ms: number;
};
type RequestStats = {
  hours: number; step_minutes: number; hosts: HostTotals[]; series: Point[];
  /** How reading each server's access log went. */
  sources: { server_id: string; server_name: string; last_read_at: string | null; last_error: string | null }[];
};
type CleanupSettings = { enabled: boolean; interval_hours: number; older_than_hours: number };
type CleanupRun = {
  id: string; trigger: 'schedule' | 'manual'; status: 'running' | 'succeeded' | 'failed';
  older_than_hours: number; started_at: string; finished_at: string | null;
  images_freed_bytes: number; builder_freed_bytes: number; error_message: string | null;
};
type CleanupOverview = { settings: CleanupSettings; runs: CleanupRun[]; intervals: number[] };

const RANGES: [string, number][] = [['1 h', 1], ['6 h', 6], ['24 h', 24], ['7 d', 168], ['30 d', 720]];
const CLASSES = [
  ['status_2xx', '2xx', '#4c9e5c'], ['status_3xx', '3xx', '#6c7fa8'],
  ['status_4xx', '4xx', '#ac8613'], ['status_5xx', '5xx', '#b3402f'],
] as const;

export const size = (bytes: number) => {
  if (bytes < 1000) return `${bytes} B`;
  const units = ['kB', 'MB', 'GB', 'TB'];
  let value = bytes / 1000; let i = 0;
  while (value >= 1000 && i < units.length - 1) { value /= 1000; i += 1; }
  return `${value.toFixed(1)} ${units[i]}`;
};
const count = (n: number) => n.toLocaleString();

export function Observability({ isAdmin, docker }: { isAdmin: boolean; docker: boolean }) {
  return <>
    <RequestStatsPanel />
    {isAdmin && <DockerCleanup docker={docker} />}
  </>;
}

function RequestStatsPanel() {
  const [hours, setHours] = useState(24);
  const [host, setHost] = useState('');
  const query = `/observability/requests?hours=${hours}${host ? `&host=${encodeURIComponent(host)}` : ''}`;
  const stats = useApi<RequestStats>(query, 60000);
  const data = stats.data;
  const totals = data?.hosts.reduce((a, h) => ({ r: a.r + h.requests, b: a.b + h.bytes_sent }), { r: 0, b: 0 });

  return <section className="panel">
    <div className="panel-title">
      <h2>Request statistics</h2>
      <div className="segmented" role="group" aria-label="Time range">
        {RANGES.map(([label, h]) => <button key={h} className={hours === h ? 'selected' : ''} onClick={() => setHours(h)}>{label}</button>)}
      </div>
    </div>
    <p className="hint">Per-minute counts from the Nginx JSON access log: status classes, average latency and bandwidth by domain. Headers and query strings are never logged.</p>
    <StateView state={stats} />
    {data?.sources.filter((s) => s.last_error).map((s) => <p key={s.server_id} className="state warn">Reading the access log of {s.server_name} failed: {s.last_error}</p>)}
    {data && !data.sources.some((s) => s.last_read_at) && !data.sources.some((s) => s.last_error) && <p className="state">The access log has not been read yet. Statistics appear once Nginx writes <span className="mono">scontrol-access.json.log</span>.</p>}
    {data && <>
      <div className="toolbar">
        <label>Domain
          <select value={host} onChange={(e) => setHost(e.target.value)}>
            <option value="">All domains</option>
            {data.hosts.map((h) => <option key={h.host} value={h.host}>{h.host}</option>)}
          </select>
        </label>
        <span className="spacer" />
        <span className="small muted">{count(totals?.r ?? 0)} requests · {size(totals?.b ?? 0)} sent · {data.step_minutes} min buckets</span>
      </div>
      <RequestChart points={data.series} />
      {data.hosts.length === 0 ? <p className="state">No requests in this period.</p>
        : <table><thead><tr><th>Domain</th><th>Requests</th><th>2xx</th><th>3xx</th><th>4xx</th><th>5xx</th><th>Avg latency</th><th>Sent</th></tr></thead><tbody>
          {data.hosts.map((h) => <tr key={h.host} className="clickable" onClick={() => setHost(h.host)}>
            <td className="mono">{h.host}</td><td>{count(h.requests)}</td><td>{count(h.status_2xx)}</td>
            <td>{count(h.status_3xx)}</td><td>{count(h.status_4xx)}</td>
            <td className={h.status_5xx > 0 ? 'danger' : ''}>{count(h.status_5xx)}</td>
            <td>{h.avg_latency_ms.toFixed(0)} ms</td><td>{size(h.bytes_sent)}</td>
          </tr>)}
        </tbody></table>}
    </>}
  </section>;
}

/** Stacked bars of requests by status class, one bar per bucket. */
function RequestChart({ points }: { points: Point[] }) {
  if (points.length === 0) return null;
  const W = 960, H = 160, pad = 28;
  const max = Math.max(1, ...points.map((p) => p.requests));
  const bar = (W - pad) / points.length;
  return <figure style={{ margin: '8px 0 20px' }}>
    <svg viewBox={`0 0 ${W} ${H + 18}`} width="100%" role="img" aria-label="Requests over time by status class">
      <text x={0} y={10} fontSize="10" fill="#777">{count(max)}</text>
      <line x1={pad} x2={W} y1={H} y2={H} stroke="#ccc" />
      {points.map((p, i) => {
        let y = H;
        return <g key={p.minute}>
          <title>{`${new Date(p.minute).toLocaleString()}: ${count(p.requests)} requests, ${p.avg_latency_ms.toFixed(0)} ms avg, ${size(p.bytes_sent)}`}</title>
          {CLASSES.map(([key, , color]) => {
            const h = (p[key] / max) * (H - 14);
            y -= h;
            return h > 0 ? <rect key={key} x={pad + i * bar} y={y} width={Math.max(1, bar - 1)} height={h} fill={color} /> : null;
          })}
        </g>;
      })}
      <text x={pad} y={H + 13} fontSize="10" fill="#777">{new Date(points[0].minute).toLocaleString()}</text>
      <text x={W} y={H + 13} fontSize="10" fill="#777" textAnchor="end">{new Date(points[points.length - 1].minute).toLocaleString()}</text>
    </svg>
    <figcaption className="small muted">{CLASSES.map(([, label, color]) => <span key={label} style={{ marginRight: 14 }}><span style={{ color }}>■</span> {label}</span>)}</figcaption>
  </figure>;
}

function DockerCleanup({ docker }: { docker: boolean }) {
  const overview = useApi<CleanupOverview>(docker ? '/observability/docker-cleanup' : null, 10000);
  const action = useAction();
  const [form, setForm] = useState<CleanupSettings>();
  const data = overview.data;
  useEffect(() => { if (data && !form) setForm(data.settings); }, [data, form]);

  if (!docker) return <section className="panel"><div className="panel-title"><h2>Docker cleanup</h2></div>
    <p className="state">Docker was not selected at install time.</p></section>;

  const runNow = () => {
    if (!window.confirm('Remove all unused images and build cache older than the configured age?')) return;
    action.run(async () => {
      await api('/observability/docker-cleanup/run', { method: 'POST', body: { older_than_hours: form?.older_than_hours } });
      overview.reload();
    });
  };
  const save = () => action.run(async () => {
    if (!form) return;
    await api('/observability/docker-cleanup/settings', { method: 'PUT', body: form });
    overview.reload();
  });
  const running = data?.runs.some((r) => r.status === 'running');

  return <section className="panel">
    <div className="panel-title">
      <h2>Docker cleanup</h2>
      <button className="action" disabled={action.busy || running || !form} onClick={runNow}>{running ? 'Cleaning…' : 'Clean up now'}</button>
    </div>
    <p className="hint">Runs <span className="mono">docker image prune -a</span> and <span className="mono">docker builder prune -a</span> with an age filter. Images used by a container are never removed.</p>
    <StateView state={overview} />
    <ErrorLine error={action.error} />
    {form && <div className="toolbar">
      <label className="check"><input type="checkbox" checked={form.enabled} onChange={(e) => setForm({ ...form, enabled: e.target.checked })} />Automatic cleanup</label>
      <label>Every
        <select value={form.interval_hours} onChange={(e) => setForm({ ...form, interval_hours: Number(e.target.value) })}>
          {(data?.intervals ?? [form.interval_hours]).map((h) => <option key={h} value={h}>{h < 24 ? `${h} hours` : `${h / 24} day${h === 24 ? '' : 's'}`}</option>)}
        </select>
      </label>
      <label>Older than (hours)
        <input type="number" min={1} max={8760} value={form.older_than_hours}
          onChange={(e) => setForm({ ...form, older_than_hours: Number(e.target.value) })} />
      </label>
      <button className="ghost" disabled={action.busy} onClick={save}>Save</button>
    </div>}
    {data && (data.runs.length === 0 ? <p className="state">No cleanups yet.</p>
      : <table><thead><tr><th>Started</th><th>Trigger</th><th>Status</th><th>Older than</th><th>Images freed</th><th>Build cache freed</th><th>Total freed</th></tr></thead><tbody>
        {data.runs.map((r) => <tr key={r.id}>
          <td className="small">{formatTime(r.started_at)}</td>
          <td className="small">{r.trigger}</td>
          <td><span className={`badge ${r.status}`}>{r.status}</span>{r.error_message && <div className="small danger">{r.error_message}</div>}</td>
          <td className="small">{r.older_than_hours} h</td>
          <td>{size(r.images_freed_bytes)}</td><td>{size(r.builder_freed_bytes)}</td>
          <td><strong>{size(r.images_freed_bytes + r.builder_freed_bytes)}</strong></td>
        </tr>)}
      </tbody></table>)}
  </section>;
}
