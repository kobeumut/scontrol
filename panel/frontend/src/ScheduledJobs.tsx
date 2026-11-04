import { FormEvent, useEffect, useRef, useState } from 'react';
import {
  api, duration, formatTime, JobInput, JobLogChunk, JobRun, JobSchedule, ScheduledJob, ServiceInfo, useApi,
} from './api';
import { ErrorLine, StateView, useAction } from './ui';

/**
 * Splits a command line into argv the way a shell would quote it, without
 * running one: quotes and backslashes group characters, nothing is
 * expanded, and `;`, `|`, `$()` are ordinary characters. Returns a message
 * for an unterminated quote.
 */
export function splitCommand(line: string): string[] | string {
  const args: string[] = [];
  let current = '';
  let started = false;
  let quote: '"' | "'" | null = null;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (quote === "'") {
      if (c === "'") quote = null; else current += c;
    } else if (quote === '"') {
      if (c === '"') quote = null;
      else if (c === '\\' && (line[i + 1] === '"' || line[i + 1] === '\\')) current += line[++i];
      else current += c;
    } else if (c === "'" || c === '"') {
      quote = c; started = true;
    } else if (c === '\\' && i + 1 < line.length) {
      current += line[++i]; started = true;
    } else if (/\s/.test(c)) {
      if (started) { args.push(current); current = ''; started = false; }
    } else {
      current += c; started = true;
    }
  }
  if (quote) return `Close the ${quote === '"' ? 'double' : 'single'} quote.`;
  if (started) args.push(current);
  return args;
}

/** Shows argv as a command line that `splitCommand` reads back unchanged. */
export function joinCommand(argv: string[]): string {
  return argv.map((arg) => (/^[A-Za-z0-9_@%+=:,./-]+$/.test(arg) ? arg : `'${arg.replace(/'/g, `'"'"'`)}'`)).join(' ');
}

const CRON_PRESETS: [string, string][] = [
  ['*/5 * * * *', 'Every 5 minutes'],
  ['0 * * * *', 'Every hour'],
  ['0 3 * * *', 'Every day at 03:00'],
  ['30 2 * * 1-5', 'Weekdays at 02:30'],
  ['0 4 * * 0', 'Sundays at 04:00'],
  ['0 0 1 * *', 'First day of the month'],
];

const CALENDAR_PRESETS: [string, string][] = [
  ['hourly', 'hourly'], ['daily', 'daily'], ['weekly', 'weekly'], ['Mon..Fri *-*-* 09:00', 'Weekdays at 09:00'],
];

const browserZone = () => Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';
const zoneList = (): string[] => {
  const intl = Intl as unknown as { supportedValuesOf?: (key: string) => string[] };
  return ['UTC', ...(intl.supportedValuesOf?.('timeZone') ?? [])];
};

const describeSchedule = (job: ScheduledJob) => `${job.schedule.kind === 'cron' ? 'cron' : 'calendar'} ${job.schedule.expression}`;

export function ScheduledJobs({ projectId }: { projectId: string }) {
  const base = `/compose-projects/${projectId}/jobs`;
  const jobs = useApi<ScheduledJob[]>(base, 5000);
  const services = useApi<ServiceInfo[]>(`/compose-projects/${projectId}/services`);
  const [editing, setEditing] = useState<ScheduledJob | 'new' | null>(null);
  const [history, setHistory] = useState<ScheduledJob | null>(null);
  const action = useAction();
  const serviceNames = [...new Set((services.data ?? []).map((s) => s.name))];

  const runNow = (job: ScheduledJob) => action.run(async () => {
    await api(`${base}/${job.id}/run`, { method: 'POST' });
    jobs.reload();
    setHistory(job);
  });
  const toggle = (job: ScheduledJob) => action.run(async () => {
    const body: JobInput = { ...job, enabled: !job.enabled };
    await api(`${base}/${job.id}`, { method: 'PUT', body });
    jobs.reload();
  });
  const remove = (job: ScheduledJob) => {
    if (!window.confirm(`Delete the job "${job.name}"? Its timer and run logs are removed.`)) return;
    action.run(async () => { await api(`${base}/${job.id}`, { method: 'DELETE' }); jobs.reload(); });
  };

  return <>
    <div className="panel-title subsection">
      <h3>Scheduled jobs</h3>
      {editing === null && <button className="ghost" onClick={() => setEditing('new')}>New job</button>}
    </div>
    <p className="hint">A systemd timer on the server runs each command in a running service container with <span className="mono">docker compose exec</span>. Commands are arguments, never a shell string; output is masked like deployment logs and kept for the last 50 runs.</p>
    <StateView state={jobs} />
    {jobs.data && jobs.data.length > 0 && <table><thead><tr><th>Job</th><th>Schedule</th><th>Command</th><th>Last run</th><th /></tr></thead><tbody>
      {jobs.data.map((job) => <tr key={job.id}>
        <td><strong>{job.name}</strong><div className="muted small mono">{job.service_name}</div></td>
        <td><span className="mono">{describeSchedule(job)}</span><div className="muted small">{job.timezone}{job.enabled ? '' : ' · paused'}</div></td>
        <td className="mono">{joinCommand(job.command)}</td>
        <td>{job.last_run_status
          ? <><span className={`badge ${job.last_run_status}`}>{job.last_run_status}</span> <span className="muted small">{formatTime(job.last_run_at)}</span></>
          : <span className="muted small">never</span>}</td>
        <td className="actions">
          <button className="link" disabled={action.busy} onClick={() => runNow(job)}>run now</button>
          <button className="link" onClick={() => setHistory(job)}>runs</button>
          <button className="link" onClick={() => setEditing(job)}>edit</button>
          <button className="link" disabled={action.busy} onClick={() => toggle(job)}>{job.enabled ? 'pause' : 'resume'}</button>
          <button className="link danger" disabled={action.busy} onClick={() => remove(job)}>delete</button>
        </td>
      </tr>)}
    </tbody></table>}
    {jobs.data && jobs.data.length === 0 && editing === null && <p className="state">No scheduled jobs.</p>}
    <ErrorLine error={action.error} />
    {editing !== null && <JobEditor base={base} job={editing === 'new' ? undefined : editing} services={serviceNames}
      onCancel={() => setEditing(null)} onSaved={() => { setEditing(null); jobs.reload(); }} />}
    {history && <RunHistory base={`${base}/${history.id}`} job={history} onClose={() => setHistory(null)} />}
  </>;
}

function JobEditor({ base, job, services, onSaved, onCancel }: {
  base: string; job?: ScheduledJob; services: string[]; onSaved: () => void; onCancel: () => void;
}) {
  const [name, setName] = useState(job?.name ?? '');
  const [service, setService] = useState(job?.service_name ?? services[0] ?? '');
  const [commandLine, setCommandLine] = useState(job ? joinCommand(job.command) : '');
  const [schedule, setSchedule] = useState<JobSchedule>(job?.schedule ?? { kind: 'cron', expression: '0 3 * * *' });
  const [timezone, setTimezone] = useState(job?.timezone ?? browserZone());
  const [timeoutMinutes, setTimeoutMinutes] = useState(String(Math.max(1, Math.round((job?.timeout_secs ?? 3600) / 60))));
  const [enabled, setEnabled] = useState(job?.enabled ?? true);
  const [preview, setPreview] = useState<{ lines?: string[]; error?: string }>({});
  const action = useAction();
  const [zones] = useState(zoneList);

  const argv = splitCommand(commandLine);
  const presets = schedule.kind === 'cron' ? CRON_PRESETS : CALENDAR_PRESETS;

  // Ask the API what the timer will run on, a moment after typing stops.
  useEffect(() => {
    if (!schedule.expression.trim()) { setPreview({}); return; }
    let active = true;
    const timer = window.setTimeout(() => {
      api<{ on_calendar: string[] }>('/scheduled-jobs/preview', { method: 'POST', body: { schedule, timezone: timezone.trim() } })
        .then((r) => active && setPreview({ lines: r.on_calendar }))
        .catch((e: Error) => active && setPreview({ error: e.message }));
    }, 300);
    return () => { active = false; window.clearTimeout(timer); };
  }, [schedule, timezone]);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (typeof argv === 'string') { action.fail(argv); return; }
    if (argv.length === 0) { action.fail('Enter the command to run, e.g. php artisan schedule:run'); return; }
    const body: JobInput = {
      name: name.trim(), service_name: service.trim(), command: argv,
      schedule: { kind: schedule.kind, expression: schedule.expression.trim() },
      timezone: timezone.trim(), timeout_secs: Math.round(Number(timeoutMinutes) * 60), enabled,
    };
    action.run(async () => {
      await api(job ? `${base}/${job.id}` : base, { method: job ? 'PUT' : 'POST', body });
      onSaved();
    });
  };

  return <form className="card" onSubmit={submit}>
    <div className="grid2">
      <label>Name<input required maxLength={64} placeholder="nightly cleanup" value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label>Service<input required className="mono" list="job-services" placeholder="web" value={service} onChange={(e) => setService(e.target.value)} /></label>
      <datalist id="job-services">{services.map((s) => <option key={s} value={s} />)}</datalist>
    </div>
    <label>Command (arguments, no shell)
      <input required className="mono" placeholder="php artisan schedule:run" value={commandLine} onChange={(e) => setCommandLine(e.target.value)} />
    </label>
    {typeof argv === 'string'
      ? <p className="state error">{argv}</p>
      : argv.length > 0 && <p className="hint">Runs as <span className="mono">[{argv.map((a) => JSON.stringify(a)).join(', ')}]</span>. Quotes group words; <span className="mono">;</span>, <span className="mono">|</span> and <span className="mono">$()</span> are passed literally. To run a pipeline, put it in a script file in the image.</p>}

    <fieldset><legend>Schedule</legend>
      <div className="toolbar">
        <div className="segmented" role="group" aria-label="Schedule type">
          <button type="button" className={schedule.kind === 'cron' ? 'selected' : ''} onClick={() => setSchedule({ kind: 'cron', expression: '0 3 * * *' })}>Cron</button>
          <button type="button" className={schedule.kind === 'calendar' ? 'selected' : ''} onClick={() => setSchedule({ kind: 'calendar', expression: 'daily' })}>systemd calendar</button>
        </div>
        <select aria-label="Preset" value="" onChange={(e) => e.target.value && setSchedule({ ...schedule, expression: e.target.value })}>
          <option value="">Presets…</option>
          {presets.map(([value, label]) => <option key={value} value={value}>{label}</option>)}
        </select>
      </div>
      <div className="grid2">
        <label>{schedule.kind === 'cron' ? 'Cron expression (minute hour day month weekday)' : 'OnCalendar expression'}
          <input required className="mono" value={schedule.expression} onChange={(e) => setSchedule({ ...schedule, expression: e.target.value })} />
        </label>
        <label>Time zone<input required className="mono" list="job-zones" value={timezone} onChange={(e) => setTimezone(e.target.value)} /></label>
        <datalist id="job-zones">{zones.map((z) => <option key={z} value={z} />)}</datalist>
      </div>
      {preview.error ? <p className="state error">{preview.error}</p>
        : preview.lines && <p className="hint">Timer: {preview.lines.map((line) => <span key={line} className="mono calendar-line">OnCalendar={line}</span>)}</p>}
    </fieldset>

    <div className="grid2">
      <label>Timeout (minutes)<input required type="number" min={1} max={1440} value={timeoutMinutes} onChange={(e) => setTimeoutMinutes(e.target.value)} /></label>
      <label className="check"><input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} /> timer enabled (a paused job can still be run by hand)</label>
    </div>
    <ErrorLine error={action.error} />
    <div className="toolbar">
      <button className="action" disabled={action.busy}>{job ? 'Save job' : 'Create job'}</button>
      <button type="button" className="ghost" onClick={onCancel}>Cancel</button>
    </div>
  </form>;
}

/** Run history of one job in a dialog, with the selected run's log. */
function RunHistory({ base, job, onClose }: { base: string; job: ScheduledJob; onClose: () => void }) {
  const runs = useApi<JobRun[]>(`${base}/runs`, 3000);
  const [selected, setSelected] = useState<string | null>(null);
  const shown = runs.data?.find((r) => r.id === (selected ?? runs.data?.[0]?.id));

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return <div className="modal-backdrop" onClick={onClose}>
    <div className="modal" role="dialog" aria-modal="true" aria-label={`Runs of ${job.name}`} onClick={(e) => e.stopPropagation()}>
      <div className="panel-title">
        <h2>{job.name} <span className="mono muted">runs</span></h2>
        <button className="link" onClick={onClose}>close</button>
      </div>
      <p className="hint mono">{joinCommand(job.command)} · {describeSchedule(job)} ({job.timezone})</p>
      <StateView state={runs} />
      {runs.data && runs.data.length === 0 && <p className="state">No runs yet. A run started by hand shows up within a few seconds.</p>}
      {runs.data && runs.data.length > 0 && <table><thead><tr><th>Started</th><th>Trigger</th><th>Duration</th><th>Exit</th><th>Status</th></tr></thead><tbody>
        {runs.data.map((r) => <tr key={r.id} className={r.id === shown?.id ? 'selected-row clickable' : 'clickable'} onClick={() => setSelected(r.id)}>
          <td>{formatTime(r.started_at)}</td>
          <td>{r.trigger}</td>
          <td>{duration(r.started_at, r.finished_at)}</td>
          <td className="mono">{r.exit_code ?? '—'}</td>
          <td><span className={`badge ${r.status}`}>{r.status}</span></td>
        </tr>)}
      </tbody></table>}
      {shown && <RunLog key={shown.id} base={base} run={shown} />}
    </div>
  </div>;
}

/** Tails a run log by byte offset until the run has finished and been read. */
function RunLog({ base, run }: { base: string; run: JobRun }) {
  const [text, setText] = useState('');
  const [error, setError] = useState<string>();
  const pre = useRef<HTMLPreElement>(null);

  useEffect(() => {
    let offset = 0;
    let stopped = false;
    let timer: number | undefined;
    const poll = async () => {
      try {
        const chunk = await api<JobLogChunk>(`${base}/runs/${run.id}/log?offset=${offset}`);
        if (stopped) return;
        offset = chunk.next_offset;
        if (chunk.content) setText((t) => t + chunk.content);
        if (!chunk.eof) timer = window.setTimeout(poll, 0);
        else if (chunk.status === 'running') timer = window.setTimeout(poll, 1000);
      } catch (e) {
        if (!stopped) setError((e as Error).message);
      }
    };
    poll();
    return () => { stopped = true; window.clearTimeout(timer); };
  }, [base, run.id]);

  useEffect(() => {
    const el = pre.current;
    if (el && el.scrollHeight - el.scrollTop - el.clientHeight < 80) el.scrollTop = el.scrollHeight;
  }, [text]);

  return <div className="log">
    <div className="log-head mono">run {run.id.slice(0, 8)} · {run.status}{run.log_truncated ? ' · output truncated at 5 MiB' : ''}</div>
    <pre ref={pre}>{text || (run.status === 'running' ? 'Waiting for output…' : 'No output (logs of older runs are pruned).')}</pre>
    {run.error_message && <p className="state error">{run.error_message}</p>}
    <ErrorLine error={error} />
  </div>;
}
