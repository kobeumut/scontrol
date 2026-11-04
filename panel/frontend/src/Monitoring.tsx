import { FormEvent, useMemo, useRef, useState } from 'react';
import { api, formatTime, useApi } from './api';
import { ErrorLine, StateView, useAction } from './ui';

type Range = '1h' | '6h' | '24h' | '7d' | '30d';
const RANGES: Range[] = ['1h', '6h', '24h', '7d', '30d'];

type Disk = { mount_point: string; fs_type: string; total_bytes: number; used_bytes: number };
type Host = {
  cpu_pct: number; cpu_count: number; mem_total_bytes: number; mem_used_bytes: number;
  swap_total_bytes: number; swap_used_bytes: number; load: { one: number; five: number; fifteen: number };
  disk_read_bps: number; disk_write_bps: number; disks: Disk[];
};
type Container = {
  compose_project_id: string; project_name: string; container_id: string; name: string; project: string; service: string;
  cpu_pct: number; mem_bytes: number; mem_limit_bytes: number | null; io_read_bps: number; io_write_bps: number;
};
type Current = { sampled_at: string | null; interval_secs: number | null; sample_micros: number | null; host: Host | null; containers: Container[] };
type ServerPoint = {
  bucket: string; cpu_pct: number; mem_used_bytes: number; mem_total_bytes: number; load1: number;
  disk_used_bytes: number; disk_total_bytes: number; disk_read_bps: number; disk_write_bps: number;
};
type ServerSeries = { range: Range; resolution_secs: number; points: ServerPoint[] };
type ContainerPoint = { bucket: string; service: string; cpu_pct: number; mem_bytes: number; mem_limit_bytes: number | null; io_read_bps: number; io_write_bps: number };
type ProjectSeries = { range: Range; resolution_secs: number; services: { service: string; points: ContainerPoint[] }[] };
type AlertSettings = {
  enabled: boolean; cpu_pct: number; memory_pct: number; disk_pct: number; container_memory_pct: number;
  hysteresis_pct: number; trigger_samples: number; clear_samples: number;
};
type AlertState = { key: string; firing: boolean; last_value: number; changed_at: string };
type Alerts = { settings: AlertSettings; firing: AlertState[] };

/** Categorical slots 1 and 2 (validated: CVD ΔE 24.7, normal ΔE 33.6 on the card surface). */
const SERIES = ['#2a78d6', '#eb6834'];

export function formatBytes(bytes: number): string {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes; let unit = 0;
  while (Math.abs(value) >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
  return `${value.toFixed(value >= 100 || unit === 0 ? 0 : 1)} ${units[unit]}`;
}
const pct = (v: number) => `${v.toFixed(1)}%`;
const rate = (v: number) => `${formatBytes(v)}/s`;
const share = (used: number, total: number) => (total > 0 ? (used * 100) / total : 0);

/** Readable name of an alert key (`host:cpu`, `disk:/`, `container:<id>:<service>:memory`). */
function alertLabel(key: string, projects: Map<string, string>): string {
  if (key === 'host:cpu') return 'CPU';
  if (key === 'host:memory') return 'Memory';
  if (key.startsWith('disk:')) return `Disk ${key.slice(5)}`;
  const [, project, service] = key.split(':');
  return `Memory of ${projects.get(project) ?? 'service'}/${service} (of its limit)`;
}

export function Monitoring({ docker }: { docker: boolean }) {
  const [range, setRange] = useState<Range>('1h');
  const current = useApi<Current>('/metrics/current', 15000);
  const server = useApi<ServerSeries>(`/metrics/server?range=${range}`, range === '1h' ? 30000 : 120000);
  const alerts = useApi<Alerts>('/metrics/alerts', 30000);
  const host = current.data?.host;
  const projects = useMemo(() => new Map((current.data?.containers ?? []).map((c) => [c.compose_project_id, c.project_name])), [current.data]);
  const fullestDisk = host?.disks.reduce<Disk | undefined>((max, d) => (!max || share(d.used_bytes, d.total_bytes) > share(max.used_bytes, max.total_bytes) ? d : max), undefined);

  return <section className="panel">
    <div className="panel-title">
      <h2>Monitoring</h2>
      <div className="segmented" role="group" aria-label="Time range">
        {RANGES.map((r) => <button key={r} className={range === r ? 'selected' : ''} onClick={() => setRange(r)}>{r}</button>)}
      </div>
    </div>
    <p className="hint">The agent reads <span className="mono">/proc</span> and cgroup v2 directly every 10–30 s, without spawning a process. History is kept at 30 s for 24 h, 5 min for 7 days, and 1 h for 30 days.</p>

    <StateView state={current} />
    {current.data && !host && <p className="state">Waiting for the first metrics sample from the agent…</p>}
    {host && <div className="stat-row">
      <Stat label="CPU" value={pct(host.cpu_pct)} detail={`${host.cpu_count} cores · load ${host.load.one.toFixed(2)} / ${host.load.five.toFixed(2)} / ${host.load.fifteen.toFixed(2)}`} />
      <Stat label="Memory" value={pct(share(host.mem_used_bytes, host.mem_total_bytes))} detail={`${formatBytes(host.mem_used_bytes)} of ${formatBytes(host.mem_total_bytes)}${host.swap_total_bytes ? ` · swap ${formatBytes(host.swap_used_bytes)}` : ''}`} />
      <Stat label={`Disk ${fullestDisk?.mount_point ?? ''}`} value={fullestDisk ? pct(share(fullestDisk.used_bytes, fullestDisk.total_bytes)) : '—'} detail={fullestDisk ? `${formatBytes(fullestDisk.used_bytes)} of ${formatBytes(fullestDisk.total_bytes)}` : 'no disk mounts found'} />
      <Stat label="Disk I/O" value={rate(host.disk_read_bps + host.disk_write_bps)} detail={`read ${rate(host.disk_read_bps)} · write ${rate(host.disk_write_bps)}`} />
    </div>}
    {current.data?.sampled_at && <p className="hint small">Sampled {formatTime(current.data.sampled_at)}{current.data.sample_micros != null && <> · sample took {current.data.sample_micros} µs</>}</p>}

    {alerts.data && alerts.data.firing.length > 0 && <div className="state warn" role="alert">
      <strong>⚠ {alerts.data.firing.length} alert{alerts.data.firing.length > 1 ? 's' : ''} firing:</strong>{' '}
      {alerts.data.firing.map((a) => `${alertLabel(a.key, projects)} at ${pct(a.last_value)} since ${formatTime(a.changed_at)}`).join(' · ')}
    </div>}

    <h3>Server</h3>
    <StateView state={server} />
    {server.data && <ServerCharts series={server.data} />}
    {host && host.disks.length > 1 && <table className="compact">
      <thead><tr><th>Mount</th><th>File system</th><th>Used</th><th>Size</th><th>Use</th></tr></thead>
      <tbody>{host.disks.map((d) => <tr key={d.mount_point}><td className="mono">{d.mount_point}</td><td>{d.fs_type}</td><td>{formatBytes(d.used_bytes)}</td><td>{formatBytes(d.total_bytes)}</td><td>{pct(share(d.used_bytes, d.total_bytes))}</td></tr>)}</tbody>
    </table>}

    <h3>Services</h3>
    {!docker ? <p className="state">Docker was not selected at install time, so there are no containers to measure.</p>
      : <Services containers={current.data?.containers ?? []} range={range} />}

    <h3>Threshold alerts</h3>
    <StateView state={alerts} />
    {alerts.data && <AlertForm settings={alerts.data.settings} onSaved={alerts.reload} />}
  </section>;
}

function Stat({ label, value, detail }: { label: string; value: string; detail: string }) {
  return <div className="stat"><span>{label}</span><strong>{value}</strong><small>{detail}</small></div>;
}

function ServerCharts({ series }: { series: ServerSeries }) {
  const { points, resolution_secs } = series;
  if (points.length === 0) return <p className="state">No samples stored for this range yet.</p>;
  const times = points.map((p) => Date.parse(p.bucket));
  return <div className="chart-grid">
    <LineChart title="CPU" times={times} resolution={resolution_secs} max={100} format={pct}
      series={[{ label: 'CPU', values: points.map((p) => p.cpu_pct) }]} />
    <LineChart title="Memory" times={times} resolution={resolution_secs} max={100} format={pct}
      series={[{ label: 'Memory', values: points.map((p) => share(p.mem_used_bytes, p.mem_total_bytes)) }]}
      detail={(i) => `${formatBytes(points[i].mem_used_bytes)} of ${formatBytes(points[i].mem_total_bytes)}`} />
    <LineChart title="Disk I/O" times={times} resolution={resolution_secs} format={rate}
      series={[{ label: 'Read', values: points.map((p) => p.disk_read_bps) }, { label: 'Write', values: points.map((p) => p.disk_write_bps), dashed: true }]} />
    <LineChart title="Disk used" times={times} resolution={resolution_secs} max={100} format={pct}
      series={[{ label: 'Disk used', values: points.map((p) => share(p.disk_used_bytes, p.disk_total_bytes)) }]}
      detail={(i) => `${formatBytes(points[i].disk_used_bytes)} of ${formatBytes(points[i].disk_total_bytes)}`} />
  </div>;
}

function Services({ containers, range }: { containers: Container[]; range: Range }) {
  const [selected, setSelected] = useState<string>();
  const projectIds = [...new Set(containers.map((c) => c.compose_project_id))];
  const projectId = selected ?? projectIds[0];
  return <>
    {containers.length === 0 ? <p className="state">No running containers of panel projects.</p>
      : <table className="compact">
        <thead><tr><th>Project</th><th>Service</th><th>Container</th><th>CPU</th><th>Memory</th><th>Disk I/O</th></tr></thead>
        <tbody>{containers.map((c) => <tr key={c.container_id} className={`clickable${c.compose_project_id === projectId ? ' selected-row' : ''}`} onClick={() => setSelected(c.compose_project_id)}>
          <td>{c.project_name}</td><td>{c.service}</td><td className="mono">{c.name}</td>
          <td>{pct(c.cpu_pct)}</td>
          <td>{formatBytes(c.mem_bytes)}{c.mem_limit_bytes ? <span className="muted"> / {formatBytes(c.mem_limit_bytes)} ({pct(share(c.mem_bytes, c.mem_limit_bytes))})</span> : <span className="muted"> · no limit</span>}</td>
          <td>{rate(c.io_read_bps + c.io_write_bps)}</td>
        </tr>)}</tbody>
      </table>}
    <p className="hint small">Container CPU follows the <span className="mono">docker stats</span> convention: 100% is one full core.</p>
    {projectId && <ProjectCharts id={projectId} name={containers.find((c) => c.compose_project_id === projectId)?.project_name ?? ''} range={range} />}
  </>;
}

function ProjectCharts({ id, name, range }: { id: string; name: string; range: Range }) {
  const data = useApi<ProjectSeries>(`/metrics/projects/${id}?range=${range}`, range === '1h' ? 30000 : 120000);
  const [service, setService] = useState<string>();
  if (!data.data) return <StateView state={data} />;
  const services = data.data.services;
  if (services.length === 0) return <p className="state">No stored samples for {name} in this range yet.</p>;
  const chosen = services.find((s) => s.service === service) ?? services[0];
  const times = chosen.points.map((p) => Date.parse(p.bucket));
  const limit = chosen.points.reduce<number | null>((l, p) => p.mem_limit_bytes ?? l, null);
  return <div className="card">
    <div className="toolbar">
      <strong>{name}</strong>
      <label className="inline">Service
        <select value={chosen.service} onChange={(e) => setService(e.target.value)}>
          {services.map((s) => <option key={s.service}>{s.service}</option>)}
        </select>
      </label>
    </div>
    <div className="chart-grid">
      <LineChart title="CPU (100% = one core)" times={times} resolution={data.data.resolution_secs} format={pct}
        series={[{ label: 'CPU', values: chosen.points.map((p) => p.cpu_pct) }]} />
      <LineChart title={limit ? `Memory (limit ${formatBytes(limit)})` : 'Memory'} times={times} resolution={data.data.resolution_secs}
        max={limit ?? undefined} format={formatBytes} series={[{ label: 'Memory', values: chosen.points.map((p) => p.mem_bytes) }]} />
      <LineChart title="Disk I/O" times={times} resolution={data.data.resolution_secs} format={rate}
        series={[{ label: 'Read', values: chosen.points.map((p) => p.io_read_bps) }, { label: 'Write', values: chosen.points.map((p) => p.io_write_bps), dashed: true }]} />
    </div>
  </div>;
}

type Series = { label: string; values: number[]; dashed?: boolean };

const W = 520, H = 170, PAD = { left: 56, right: 12, top: 10, bottom: 22 };

/**
 * A small line chart: one y-axis, recessive grid, 2px lines, a crosshair
 * tooltip on hover, and a legend (with direct end labels) for two series.
 * A gap longer than two buckets breaks the line instead of bridging it.
 */
export function LineChart({ title, times, series, resolution, max, format, detail }: {
  title: string; times: number[]; series: Series[]; resolution: number; max?: number;
  format: (v: number) => string; detail?: (index: number) => string;
}) {
  const [hover, setHover] = useState<number>();
  const svg = useRef<SVGSVGElement>(null);
  const t0 = times[0], t1 = times[times.length - 1];
  const span = Math.max(t1 - t0, resolution * 1000);
  const peak = Math.max(...series.flatMap((s) => s.values), 0);
  const yMax = max ?? niceCeil(peak);
  const x = (t: number) => PAD.left + ((t - t0) / span) * (W - PAD.left - PAD.right);
  const y = (v: number) => PAD.top + (1 - Math.min(v, yMax) / (yMax || 1)) * (H - PAD.top - PAD.bottom);
  const path = (values: number[]) => values.map((v, i) => {
    const gap = i === 0 || times[i] - times[i - 1] > resolution * 2000;
    return `${gap ? 'M' : 'L'}${x(times[i]).toFixed(1)},${y(v).toFixed(1)}`;
  }).join('');
  const ticks = [0, 0.25, 0.5, 0.75, 1].map((f) => f * yMax);

  const onMove = (event: React.MouseEvent<SVGSVGElement>) => {
    const box = svg.current?.getBoundingClientRect();
    if (!box) return;
    const t = t0 + (((event.clientX - box.left) / box.width) * W - PAD.left) / (W - PAD.left - PAD.right) * span;
    let best = 0;
    times.forEach((time, i) => { if (Math.abs(time - t) < Math.abs(times[best] - t)) best = i; });
    setHover(best);
  };
  const last = times.length - 1;

  return <figure className="chart">
    <figcaption>
      <span>{title}</span>
      {series.length > 1 && <span className="legend">{series.map((s, i) => <span key={s.label}><i className={s.dashed ? 'dashed' : ''} style={{ borderColor: SERIES[i] }} />{s.label}</span>)}</span>}
    </figcaption>
    <svg ref={svg} viewBox={`0 0 ${W} ${H}`} role="img" aria-label={`${title}, latest ${series.map((s) => `${s.label} ${format(s.values[last])}`).join(', ')}`}
      onMouseMove={onMove} onMouseLeave={() => setHover(undefined)}>
      {ticks.map((v) => <g key={v}>
        <line className="grid" x1={PAD.left} x2={W - PAD.right} y1={y(v)} y2={y(v)} />
        <text className="axis" x={PAD.left - 6} y={y(v) + 3} textAnchor="end">{format(v)}</text>
      </g>)}
      <text className="axis" x={PAD.left} y={H - 6}>{timeLabel(t0, span)}</text>
      <text className="axis" x={W - PAD.right} y={H - 6} textAnchor="end">{timeLabel(t1, span)}</text>
      {series.map((s, i) => <path key={s.label} d={path(s.values)} fill="none" stroke={SERIES[i]} strokeWidth={2}
        strokeDasharray={s.dashed ? '5 3' : undefined} strokeLinejoin="round" strokeLinecap="round" />)}
      {series.length > 1 && endLabelYs(series.map((s) => y(s.values[last]) - 5)).map((labelY, i) => <text key={series[i].label} className="end-label" x={W - PAD.right - 2} y={labelY} textAnchor="end">{series[i].label} {format(series[i].values[last])}</text>)}
      {hover !== undefined && <g>
        <line className="crosshair" x1={x(times[hover])} x2={x(times[hover])} y1={PAD.top} y2={H - PAD.bottom} />
        {series.map((s, i) => <circle key={s.label} cx={x(times[hover])} cy={y(s.values[hover])} r={4} fill={SERIES[i]} stroke="#f7f6f0" strokeWidth={2} />)}
      </g>}
    </svg>
    {hover !== undefined && <div className="chart-tip" style={{ left: `${(x(times[hover]) / W) * 100}%` }}>
      <div className="muted">{new Date(times[hover]).toLocaleString()}</div>
      {series.map((s, i) => <div key={s.label}><i style={{ background: SERIES[i] }} />{s.label} <strong>{format(s.values[hover])}</strong></div>)}
      {detail && <div className="muted">{detail(hover)}</div>}
    </div>}
  </figure>;
}

/** Spreads end labels at least one line apart, keeping them inside the plot. */
function endLabelYs(wanted: number[]): number[] {
  const order = wanted.map((v, i) => i).sort((a, b) => wanted[b] - wanted[a]);
  const placed = [...wanted];
  let floor = H - PAD.bottom - 3;
  for (const i of order) { placed[i] = Math.min(wanted[i], floor); floor = placed[i] - 11; }
  return placed.map((v) => Math.max(v, PAD.top + 9));
}

function niceCeil(value: number): number {
  if (value <= 0) return 1;
  const magnitude = 10 ** Math.floor(Math.log10(value));
  const step = [1, 2, 2.5, 5, 10].find((s) => s * magnitude >= value) ?? 10;
  return step * magnitude;
}

function timeLabel(t: number, span: number): string {
  const date = new Date(t);
  return span > 36 * 3600 * 1000
    ? date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
    : date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
}

function AlertForm({ settings, onSaved }: { settings: AlertSettings; onSaved: () => void }) {
  const [form, setForm] = useState(settings);
  const [saved, setSaved] = useState(false);
  const action = useAction();
  const number = (key: keyof AlertSettings, label: string, min: number, max: number, step = 1) =>
    <label key={key}>{label}<input type="number" min={min} max={max} step={step} value={Number(form[key])}
      onChange={(e) => { setSaved(false); setForm({ ...form, [key]: Number(e.target.value) }); }} /></label>;
  const submit = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      setForm(await api<AlertSettings>('/metrics/alerts/settings', { method: 'PUT', body: form }));
      setSaved(true);
      onSaved();
    });
  };
  return <form className="card" onSubmit={submit}>
    <label className="check"><input type="checkbox" checked={form.enabled} onChange={(e) => { setSaved(false); setForm({ ...form, enabled: e.target.checked }); }} /> Send alerts when a threshold is crossed</label>
    <div className="grid4">
      {number('cpu_pct', 'CPU %', 1, 100)}
      {number('memory_pct', 'Memory %', 1, 100)}
      {number('disk_pct', 'Disk % (any mount)', 1, 100)}
      {number('container_memory_pct', 'Service memory % of limit', 1, 100)}
    </div>
    <div className="grid4">
      {number('hysteresis_pct', 'Hysteresis (points)', 0, 50, 0.5)}
      {number('trigger_samples', 'Fire after N samples', 1, 60)}
      {number('clear_samples', 'Resolve after N samples', 1, 60)}
    </div>
    <p className="hint">Samples are 30 s apart. An alert fires once when a value stays at or above its threshold for N samples, and resolves once it stays below threshold − hysteresis. Values in between change nothing, so a metric hovering near the line does not repeat the alert. Subscribe channels to “Resource alert” under Notifications.</p>
    <ErrorLine error={action.error} />
    <div className="toolbar"><button className="action" disabled={action.busy}>{action.busy ? 'Saving…' : 'Save thresholds'}</button>{saved && <span className="state">Saved.</span>}</div>
  </form>;
}
