import { FormEvent, useEffect, useRef, useState } from 'react';
import {
  api, ApiState, ApplicationBuild, ApplicationSpec, ComposeProject, ComposeSource, Deployment, Domain, duration,
  formatTime, GitAccess, gitCheckout, gitTransport, isActive, LogChunk, RestartPolicy, ServiceInfo, sourceLabel, useApi,
} from './api';

type EnvRow = { key: string; value: string };

function StateView({ state }: { state: Pick<ApiState<unknown>, 'loading' | 'error'> }) {
  if (state.loading) return <p className="state">Loading…</p>;
  if (state.error) return <p className="state error">{state.error}</p>;
  return null;
}

function ErrorLine({ error }: { error?: string }) {
  return error ? <p className="state error" role="alert">{error}</p> : null;
}

/** Runs an async action, exposing busy/error state for the button that triggers it. */
function useAction() {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const run = async (action: () => Promise<unknown>) => {
    setBusy(true); setError(undefined);
    try { await action(); } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  return { busy, error, run, fail: setError };
}

/**
 * Write-only environment editor: values are typed in but never loaded back,
 * because the panel does not return stored secrets.
 */
function EnvEditor({ rows, onChange }: { rows: EnvRow[]; onChange: (rows: EnvRow[]) => void }) {
  const update = (index: number, patch: Partial<EnvRow>) =>
    onChange(rows.map((row, i) => (i === index ? { ...row, ...patch } : row)));
  return <div className="env-editor">
    {rows.map((row, index) => <div className="env-row" key={index}>
      <input aria-label="Variable name" placeholder="NAME" className="mono" value={row.key}
        onChange={(e) => update(index, { key: e.target.value })} />
      <input aria-label="Variable value" placeholder="value" type="password" autoComplete="off" value={row.value}
        onChange={(e) => update(index, { value: e.target.value })} />
      <button type="button" className="link" onClick={() => onChange(rows.filter((_, i) => i !== index))}>remove</button>
    </div>)}
    <button type="button" className="link" onClick={() => onChange([...rows, { key: '', value: '' }])}>+ add variable</button>
  </div>;
}

const toPairs = (rows: EnvRow[]) => rows.filter((r) => r.key.trim()).map((r) => [r.key.trim(), r.value]);

export function ComposeProjects() {
  const [openId, setOpenId] = useState<string | null>(null);
  if (openId) return <ProjectDetail id={openId} onBack={() => setOpenId(null)} />;
  return <ProjectList onOpen={setOpenId} />;
}

function ProjectList({ onOpen }: { onOpen: (id: string) => void }) {
  const projects = useApi<ComposeProject[]>('/compose-projects', 5000);
  const [creating, setCreating] = useState(false);

  return <section className="panel">
    <div className="panel-title">
      <h2>Compose projects</h2>
      <button className="action" onClick={() => setCreating((c) => !c)}>{creating ? 'Close' : 'New project'}</button>
    </div>
    <p className="hint">Compose projects (Git or raw) and single-service applications (image or Dockerfile) with a durable queue, write-only environment and streamed logs.</p>
    {creating && <CreateProjectForm onCreated={(id) => { setCreating(false); onOpen(id); }} />}
    <StateView state={projects} />
    {projects.data && <table><thead><tr><th>Project</th><th>Slug</th><th>Source</th><th>Status</th></tr></thead><tbody>
      {projects.data.length === 0
        ? <tr><td colSpan={4} className="empty">No Compose projects yet.</td></tr>
        : projects.data.map((project) => <tr key={project.id} className="clickable" onClick={() => onOpen(project.id)}>
          <td><button className="link strong" onClick={() => onOpen(project.id)}>{project.name}</button></td>
          <td className="mono">{project.slug}</td>
          <td>{sourceLabel(project.source)}</td>
          <td><span className={`badge ${project.status}`}>{project.status}</span></td>
        </tr>)}
    </tbody></table>}
  </section>;
}

const SAMPLE_COMPOSE = 'services:\n  web:\n    image: traefik/whoami:v1.10\n';

function CreateProjectForm({ onCreated }: { onCreated: (id: string) => void }) {
  const [name, setName] = useState('');
  const [slug, setSlug] = useState('');
  const [kind, setKind] = useState<SourceChoice>('git');
  const [repo, setRepo] = useState('');
  const [branch, setBranch] = useState('main');
  const [composePath, setComposePath] = useState('docker-compose.yml');
  const [yaml, setYaml] = useState(SAMPLE_COMPOSE);
  const [image, setImage] = useState('');
  const [contextPath, setContextPath] = useState('.');
  const [dockerfilePath, setDockerfilePath] = useState('Dockerfile');
  const [app, setApp] = useState<AppSettings>(DEFAULT_APP);
  const [env, setEnv] = useState<EnvRow[]>([]);
  const [tokenUser, setTokenUser] = useState('x-access-token');
  const [token, setToken] = useState('');
  const [deployNow, setDeployNow] = useState<boolean>();
  const action = useAction();
  const fromGit = kind === 'git' || kind === 'dockerfile';
  const transport = fromGit ? gitTransport(repo.trim()) : null;
  // A private SSH repository needs its deploy key registered first, and the
  // key exists only once the project does.
  const deploy = deployNow ?? transport !== 'ssh';

  const submit = (event: FormEvent) => {
    event.preventDefault();
    let source: ComposeSource;
    if (kind === 'git') source = { kind, repository_url: repo.trim(), branch: branch.trim(), compose_path: composePath.trim() };
    else if (kind === 'raw') source = { kind, compose_yaml: yaml };
    else {
      const spec = toApplicationSpec(app, kind === 'image'
        ? { type: 'image', image: image.trim() }
        : { type: 'dockerfile', repository_url: repo.trim(), branch: branch.trim(), context_path: contextPath.trim() || '.', dockerfile_path: dockerfilePath.trim() || 'Dockerfile' });
      if (typeof spec === 'string') { action.fail(spec); return; }
      source = { kind: 'application', ...spec };
    }
    const git_credentials = transport === 'https' && token ? { username: tokenUser.trim(), token } : undefined;
    action.run(async () => {
      const created = await api<{ project: ComposeProject }>('/compose-projects', {
        method: 'POST',
        body: { name: name.trim(), slug: slug.trim(), source, environment: toPairs(env), git_credentials, deploy },
      });
      setToken('');
      onCreated(created.project.id);
    });
  };

  return <form className="card" onSubmit={submit}>
    <div className="grid2">
      <label>Name<input required maxLength={100} value={name} onChange={(e) => {
        setName(e.target.value);
        setSlug(e.target.value.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 48));
      }} /></label>
      <label>Slug<input required className="mono" pattern="[a-z][a-z0-9-]{2,47}" value={slug}
        onChange={(e) => setSlug(e.target.value)} /></label>
    </div>
    <div className="segmented" role="radiogroup" aria-label="Source">
      {SOURCE_CHOICES.map(([k, label]) => <button type="button" key={k} role="radio" aria-checked={kind === k}
        className={kind === k ? 'selected' : ''} onClick={() => setKind(k)}>{label}</button>)}
    </div>
    {fromGit && <div className="grid3">
      <label>Repository URL<input required placeholder="https://github.com/org/app.git or git@github.com:org/app.git" value={repo} onChange={(e) => setRepo(e.target.value)} /></label>
      <label>Branch<input required value={branch} onChange={(e) => setBranch(e.target.value)} /></label>
      {kind === 'git'
        ? <label>Compose path<input required className="mono" value={composePath} onChange={(e) => setComposePath(e.target.value)} /></label>
        : <div className="grid2">
          <label>Build context<input required className="mono" value={contextPath} onChange={(e) => setContextPath(e.target.value)} /></label>
          <label>Dockerfile<input required className="mono" value={dockerfilePath} onChange={(e) => setDockerfilePath(e.target.value)} /></label>
        </div>}
    </div>}
    {kind === 'raw' && <label>Compose YAML<textarea required rows={8} className="mono" value={yaml} onChange={(e) => setYaml(e.target.value)} /></label>}
    {kind === 'image' && <label>Image<input required className="mono" placeholder="ghcr.io/org/app:1.0" value={image} onChange={(e) => setImage(e.target.value)} /></label>}
    {(kind === 'image' || kind === 'dockerfile') && <ApplicationFields value={app} onChange={setApp} />}
    {transport === 'https' && <fieldset><legend>Private repository token (optional, write-only)</legend>
      <div className="grid2">
        <label>Username<input className="mono" value={tokenUser} onChange={(e) => setTokenUser(e.target.value)} /></label>
        <label>Token<input type="password" autoComplete="off" placeholder="leave empty for a public repository" value={token}
          onChange={(e) => setToken(e.target.value)} /></label>
      </div>
      <p className="hint">GitHub: <span className="mono">x-access-token</span> with a fine-grained token. GitLab: <span className="mono">oauth2</span>. Gitea/Forgejo: your login name.</p>
    </fieldset>}
    {transport === 'ssh' && <p className="hint">SSH remote: after creating, generate the project deploy key under Git access, add it to the repository as a read-only deploy key, then deploy.</p>}
    <fieldset><legend>Environment (write-only)</legend><EnvEditor rows={env} onChange={setEnv} /></fieldset>
    <label className="check"><input type="checkbox" checked={deploy} onChange={(e) => setDeployNow(e.target.checked)} /> deploy right away</label>
    <ErrorLine error={action.error} />
    <button className="action" disabled={action.busy}>{action.busy ? 'Creating…' : deploy ? 'Create and deploy' : 'Create'}</button>
  </form>;
}

type SourceChoice = 'git' | 'raw' | 'image' | 'dockerfile';

const SOURCE_CHOICES: [SourceChoice, string][] = [
  ['git', 'Compose from Git'], ['raw', 'Raw Compose'], ['image', 'App: image'], ['dockerfile', 'App: Dockerfile'],
];

type AppSettings = {
  serviceName: string; port: string; restart: RestartPolicy; memoryMb: string; cpus: string;
  healthCommand: string; volumes: string;
};

const DEFAULT_APP: AppSettings = {
  serviceName: 'app', port: '', restart: 'unless-stopped', memoryMb: '', cpus: '', healthCommand: '', volumes: '',
};

/** Builds the API spec from form fields, or returns a message to show. */
function toApplicationSpec(app: AppSettings, build: ApplicationBuild): ApplicationSpec | string {
  const optionalInt = (value: string) => (value.trim() === '' ? null : Number(value));
  const port = optionalInt(app.port);
  if (port !== null && !(Number.isInteger(port) && port >= 1 && port <= 65535)) return 'Container port must be 1–65535.';
  const memory = optionalInt(app.memoryMb);
  if (memory !== null && !(Number.isInteger(memory) && memory >= 6)) return 'Memory limit is a whole number of MiB, at least 6.';
  const cpus = app.cpus.trim() === '' ? null : Math.round(Number(app.cpus) * 1000);
  if (cpus !== null && !(Number.isFinite(cpus) && cpus >= 10)) return 'CPU limit must be at least 0.01.';
  // One argument per line: the command runs in exec form, never in a shell.
  const command = app.healthCommand.split('\n').map((arg) => arg.trim()).filter(Boolean);
  const volumes = [];
  for (const line of app.volumes.split('\n').map((l) => l.trim()).filter(Boolean)) {
    const at = line.indexOf(':');
    if (at < 1) return `Volume "${line}" must be name:/container/path.`;
    volumes.push({ name: line.slice(0, at), target: line.slice(at + 1) });
  }
  return {
    build,
    service_name: app.serviceName.trim() || 'app',
    container_port: port,
    restart: app.restart,
    memory_limit_mb: memory,
    cpu_limit_millis: cpus,
    health_check: command.length
      ? { command, interval_secs: 30, timeout_secs: 10, retries: 3, start_period_secs: 0 }
      : null,
    volumes,
  };
}

function ApplicationFields({ value, onChange }: { value: AppSettings; onChange: (v: AppSettings) => void }) {
  const set = (key: keyof AppSettings) => (e: { target: { value: string } }) => onChange({ ...value, [key]: e.target.value });
  return <fieldset><legend>Application</legend>
    <div className="grid3">
      <label>Service name<input className="mono" pattern="[a-zA-Z0-9][a-zA-Z0-9_.\-]{0,62}" value={value.serviceName} onChange={set('serviceName')} /></label>
      <label>Container port<input inputMode="numeric" placeholder="e.g. 3000" value={value.port} onChange={set('port')} /></label>
      <label>Restart<select value={value.restart} onChange={set('restart')}>
        {(['unless-stopped', 'always', 'on-failure', 'no'] as const).map((r) => <option key={r} value={r}>{r}</option>)}
      </select></label>
      <label>Memory limit (MiB)<input inputMode="numeric" placeholder="none" value={value.memoryMb} onChange={set('memoryMb')} /></label>
      <label>CPU limit (cores)<input inputMode="decimal" placeholder="none" value={value.cpus} onChange={set('cpus')} /></label>
    </div>
    <div className="grid2">
      <label>Health check command (one argument per line, no shell)<textarea rows={3} className="mono"
        placeholder={'wget\n-qO-\nhttp://127.0.0.1:3000/health'} value={value.healthCommand} onChange={set('healthCommand')} /></label>
      <label>Named volumes (name:/container/path per line)<textarea rows={3} className="mono"
        placeholder="uploads:/app/uploads" value={value.volumes} onChange={set('volumes')} /></label>
    </div>
    <p className="hint">The environment below reaches the container through the project's write-only env file. Bind a domain to the service name and container port after creating.</p>
  </fieldset>;
}

function SourceSummary({ source }: { source: ComposeSource }) {
  if (source.kind === 'git') {
    return <>Git <span className="mono">{source.repository_url}</span> · branch <span className="mono">{source.branch}</span> · <span className="mono">{source.compose_path}</span></>;
  }
  if (source.kind === 'raw') return <>Raw Compose YAML</>;
  const limits = [
    source.container_port ? `port ${source.container_port}` : null,
    source.memory_limit_mb ? `${source.memory_limit_mb} MiB` : null,
    source.cpu_limit_millis ? `${source.cpu_limit_millis / 1000} CPU` : null,
    source.restart,
  ].filter(Boolean).join(' · ');
  const build = source.build;
  return <>Application <span className="mono">{source.service_name}</span> · {build.type === 'image'
    ? <>image <span className="mono">{build.image}</span></>
    : <>Dockerfile <span className="mono">{build.dockerfile_path}</span> in <span className="mono">{build.context_path}</span> from <span className="mono">{build.repository_url}</span> · branch <span className="mono">{build.branch}</span></>}
    {' · '}{limits}</>;
}

function ProjectDetail({ id, onBack }: { id: string; onBack: () => void }) {
  const project = useApi<ComposeProject>(`/compose-projects/${id}`, 3000);
  const deployments = useApi<Deployment[]>(`/compose-projects/${id}/deployments`, 3000);
  const [selected, setSelected] = useState<string | null>(null);
  const [pull, setPull] = useState(false);
  const action = useAction();

  const latest = deployments.data?.[0];
  const shown = deployments.data?.find((d) => d.id === (selected ?? latest?.id));

  const deploy = () => action.run(async () => {
    const queued = await api<Deployment>(`/compose-projects/${id}/deploy`, { method: 'POST', body: { pull_images: pull } });
    setSelected(queued.id);
    deployments.reload(); project.reload();
  });
  const stop = () => action.run(async () => {
    await api(`/compose-projects/${id}/stop`, { method: 'POST' });
    project.reload();
  });
  const remove = () => {
    if (!project.data || window.prompt(`Type ${project.data.slug} to delete this project. Named volumes are kept.`) !== project.data.slug) return;
    action.run(async () => { await api(`/compose-projects/${id}`, { method: 'DELETE' }); onBack(); });
  };

  return <section className="panel">
    <button className="link" onClick={onBack}>← All projects</button>
    <StateView state={project} />
    {project.data && <>
      <div className="panel-title">
        <h2>{project.data.name} <span className="mono muted">{project.data.slug}</span></h2>
        <span className={`badge ${project.data.status}`}>{project.data.status}</span>
      </div>
      <p className="hint"><SourceSummary source={project.data.source} /></p>
      <div className="toolbar">
        <button className="action" disabled={action.busy} onClick={deploy}>Deploy</button>
        <label className="check"><input type="checkbox" checked={pull} onChange={(e) => setPull(e.target.checked)} /> pull images</label>
        <span className="spacer" />
        <button className="ghost" disabled={action.busy || project.data.status === 'running'} onClick={stop}>Stop</button>
        <button className="ghost danger" disabled={action.busy || project.data.status === 'running'} onClick={remove}>Delete</button>
      </div>
      <ErrorLine error={action.error} />

      <h3>Deployments</h3>
      <StateView state={deployments} />
      {deployments.data && <DeploymentTable projectId={id} deployments={deployments.data} selectedId={shown?.id}
        onSelect={setSelected} onChanged={deployments.reload} />}
      {shown && <DeploymentLog key={shown.id} projectId={id} deployment={shown} />}

      <Services projectId={id} refreshKey={latest?.status} />
      <ContainerLogs projectId={id} />
      <Domains projectId={id} onDeploy={(d) => { setSelected(d.id); deployments.reload(); }} />
      <Environment projectId={id} />
      {gitCheckout(project.data.source) && <GitAccessPanel projectId={id} />}
      {gitCheckout(project.data.source) && <Webhook projectId={id} />}
    </>}
  </section>;
}

function DeploymentTable({ projectId, deployments, selectedId, onSelect, onChanged }: {
  projectId: string; deployments: Deployment[]; selectedId?: string;
  onSelect: (id: string) => void; onChanged: () => void;
}) {
  const action = useAction();
  const cancel = (deployment: Deployment) => action.run(async () => {
    await api(`/compose-projects/${projectId}/deployments/${deployment.id}/cancel`, { method: 'POST' });
    onChanged();
  });
  if (deployments.length === 0) return <p className="state">No deployments yet.</p>;
  return <>
    <ErrorLine error={action.error} />
    <table><thead><tr><th>Queued</th><th>Trigger</th><th>Commit</th><th>Duration</th><th>Status</th><th /></tr></thead><tbody>
      {deployments.slice(0, 10).map((d) => <tr key={d.id} className={d.id === selectedId ? 'selected-row clickable' : 'clickable'} onClick={() => onSelect(d.id)}>
        <td>{formatTime(d.queued_at)}</td>
        <td>{d.trigger}</td>
        <td className="mono">{d.commit_sha?.slice(0, 8) ?? '—'}</td>
        <td>{duration(d.started_at, d.finished_at)}</td>
        <td><span className={`badge ${d.status}`}>{d.status}</span></td>
        <td>{isActive(d.status) && <button className="link danger" disabled={action.busy}
          onClick={(e) => { e.stopPropagation(); cancel(d); }}>cancel</button>}</td>
      </tr>)}
    </tbody></table>
  </>;
}

/** Tails a deployment log by byte offset until it is complete. */
function DeploymentLog({ projectId, deployment }: { projectId: string; deployment: Deployment }) {
  const [text, setText] = useState('');
  const [error, setError] = useState<string>();
  const pre = useRef<HTMLPreElement>(null);

  useEffect(() => {
    let offset = 0;
    let stopped = false;
    let timer: number | undefined;
    const poll = async () => {
      try {
        const chunk = await api<LogChunk>(`/compose-projects/${projectId}/deployments/${deployment.id}/log?offset=${offset}`);
        if (stopped) return;
        offset = chunk.next_offset;
        if (chunk.content) setText((t) => t + chunk.content);
        // Keep reading at once while there is more; otherwise wait, and stop
        // for good once a finished deployment has been read to the end.
        if (!chunk.eof) timer = window.setTimeout(poll, 0);
        else if (isActive(chunk.status)) timer = window.setTimeout(poll, 1000);
      } catch (e) {
        if (!stopped) setError((e as Error).message);
      }
    };
    poll();
    return () => { stopped = true; window.clearTimeout(timer); };
  }, [projectId, deployment.id]);

  useEffect(() => {
    const el = pre.current;
    if (el && el.scrollHeight - el.scrollTop - el.clientHeight < 80) el.scrollTop = el.scrollHeight;
  }, [text]);

  return <div className="log">
    <div className="log-head mono">deployment {deployment.id.slice(0, 8)} · {deployment.status}</div>
    <pre ref={pre}>{text || (isActive(deployment.status) ? 'Waiting for output…' : 'No output.')}</pre>
    {deployment.error_message && <p className="state error">{deployment.error_message}</p>}
    <ErrorLine error={error} />
  </div>;
}

function Services({ projectId, refreshKey }: { projectId: string; refreshKey?: string }) {
  const services = useApi<ServiceInfo[]>(`/compose-projects/${projectId}/services`, 10000);
  const { reload } = services;
  useEffect(reload, [refreshKey, reload]);
  return <>
    <h3>Services</h3>
    {services.error ? <p className="state">Not deployed yet, or Docker is unavailable.</p> : <StateView state={services} />}
    {services.data && <table><thead><tr><th>Service</th><th>Image</th><th>State</th><th>Ports</th></tr></thead><tbody>
      {services.data.length === 0 ? <tr><td colSpan={4} className="empty">No containers.</td></tr> : services.data.map((s) => <tr key={s.container || s.name}>
        <td>{s.name}</td>
        <td className="mono">{s.image}</td>
        <td><span className={`badge ${s.state === 'running' ? (s.health === 'unhealthy' ? 'failed' : 'succeeded') : 'failed'}`}>{s.health || s.state}</span> <span className="muted small">{s.status}</span></td>
        <td className="mono">{s.ports.join(', ') || '—'}</td>
      </tr>)}
    </tbody></table>}
  </>;
}

/** Longest live log kept in the browser; older lines are dropped. */
const MAX_LIVE_LINES = 2000;

/**
 * Live container output over Server-Sent Events. Searching happens on the
 * agent, after secrets are masked. A dropped connection is not retried
 * automatically, because a reconnect would replay the tail as duplicates.
 */
function ContainerLogs({ projectId }: { projectId: string }) {
  const services = useApi<ServiceInfo[]>(`/compose-projects/${projectId}/services`);
  const [service, setService] = useState('');
  const [search, setSearch] = useState('');
  const [timestamps, setTimestamps] = useState(false);
  const [query, setQuery] = useState<string | null>(null);
  const [lines, setLines] = useState<string[]>([]);
  const [status, setStatus] = useState<string>();
  const [live, setLive] = useState(false);
  const pre = useRef<HTMLPreElement>(null);

  useEffect(() => {
    if (query === null) return;
    setLines([]); setStatus('connecting…'); setLive(true);
    const source = new EventSource(`/api/compose-projects/${projectId}/logs/stream?${query}`);
    const finish = (message: string) => { source.close(); setStatus(message); setLive(false); };
    source.onopen = () => setStatus('following');
    source.onmessage = (event) => setLines((current) => {
      const next = [...current, event.data];
      return next.length > MAX_LIVE_LINES ? next.slice(next.length - MAX_LIVE_LINES) : next;
    });
    source.addEventListener('end', () => finish('stream ended (containers stopped or the follow limit elapsed)'));
    source.addEventListener('stream-error', (event) => finish(`error: ${(event as MessageEvent).data}`));
    source.onerror = () => finish('disconnected');
    return () => source.close();
  }, [projectId, query]);

  useEffect(() => {
    const el = pre.current;
    if (el && el.scrollHeight - el.scrollTop - el.clientHeight < 80) el.scrollTop = el.scrollHeight;
  }, [lines]);

  const start = (event: FormEvent) => {
    event.preventDefault();
    const params = new URLSearchParams({ tail: '200' });
    if (service) params.set('service', service);
    if (search.trim()) params.set('search', search.trim());
    if (timestamps) params.set('timestamps', 'true');
    // A changed nonce restarts the stream even when the filters are unchanged.
    params.set('n', String(Date.now()));
    setQuery(params.toString());
  };
  return <>
    <h3>Container logs</h3>
    <form className="inline-form" onSubmit={start}>
      <select aria-label="Service" value={service} onChange={(e) => setService(e.target.value)}>
        <option value="">all services</option>
        {[...new Set(services.data?.map((s) => s.name) ?? [])].map((name) => <option key={name} value={name}>{name}</option>)}
      </select>
      <input aria-label="Search" placeholder="search (case-insensitive)" value={search} onChange={(e) => setSearch(e.target.value)} />
      <label className="check"><input type="checkbox" checked={timestamps} onChange={(e) => setTimestamps(e.target.checked)} /> timestamps</label>
      <button className="action">{live ? 'Restart' : 'Follow'}</button>
      {live && <button type="button" className="ghost" onClick={() => { setQuery(null); setLive(false); setStatus('stopped'); }}>Stop</button>}
    </form>
    {status ? <div className="log">
      <div className="log-head mono">live · {status}</div>
      <pre ref={pre}>{lines.length ? lines.join('\n') : 'Waiting for output…'}</pre>
    </div> : <p className="hint">Shows the last 200 lines, then follows new output. Environment values are masked.</p>}
  </>;
}

function Domains({ projectId, onDeploy }: { projectId: string; onDeploy: (deployment: Deployment) => void }) {
  const domains = useApi<Domain[]>(`/compose-projects/${projectId}/domains`);
  const [domain, setDomain] = useState('');
  const [service, setService] = useState('');
  const [port, setPort] = useState('80');
  const [https, setHttps] = useState(true);
  const action = useAction();

  const add = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      const created = await api<Domain & { deployment?: Deployment }>(`/compose-projects/${projectId}/domains`, {
        method: 'POST', body: { domain: domain.trim(), service_name: service.trim(), container_port: Number(port), https },
      });
      setDomain('');
      domains.reload();
      if (created.deployment) onDeploy(created.deployment);
    });
  };
  const remove = (d: Domain) => action.run(async () => {
    const result = await api<{ deployment?: Deployment }>(`/compose-projects/${projectId}/domains/${d.id}`, { method: 'DELETE' });
    domains.reload();
    if (result.deployment) onDeploy(result.deployment);
  });

  return <>
    <h3>Domains</h3>
    <p className="hint">Each domain proxies to one service port published on loopback only. Changes queue a redeploy.</p>
    <StateView state={domains} />
    {domains.data && domains.data.length > 0 && <table><thead><tr><th>Domain</th><th>Target</th><th>Loopback</th><th>HTTPS</th><th /></tr></thead><tbody>
      {domains.data.map((d) => <tr key={d.id}>
        <td><a href={`${d.https ? 'https' : 'http'}://${d.domain}`} target="_blank" rel="noreferrer">{d.domain}</a></td>
        <td className="mono">{d.service_name}:{d.container_port}</td>
        <td className="mono">127.0.0.1:{d.host_port}</td>
        <td>{d.https ? 'yes' : '—'}</td>
        <td><button className="link danger" disabled={action.busy} onClick={() => remove(d)}>remove</button></td>
      </tr>)}
    </tbody></table>}
    <form className="inline-form" onSubmit={add}>
      <input required aria-label="Domain" placeholder="app.example.com" value={domain} onChange={(e) => setDomain(e.target.value)} />
      <input required aria-label="Service" placeholder="service" className="mono" value={service} onChange={(e) => setService(e.target.value)} />
      <input required aria-label="Container port" type="number" min={1} max={65535} className="port" value={port} onChange={(e) => setPort(e.target.value)} />
      <label className="check"><input type="checkbox" checked={https} onChange={(e) => setHttps(e.target.checked)} /> HTTPS</label>
      <button className="action" disabled={action.busy}>Add domain</button>
    </form>
    <ErrorLine error={action.error} />
  </>;
}

function Environment({ projectId }: { projectId: string }) {
  const [rows, setRows] = useState<EnvRow[]>([]);
  const [saved, setSaved] = useState(false);
  const action = useAction();
  const save = () => action.run(async () => {
    await api(`/compose-projects/${projectId}/environment`, { method: 'PUT', body: { environment: toPairs(rows) } });
    setRows([]); setSaved(true);
  });
  return <>
    <h3>Environment</h3>
    <p className="hint">Write-only. Saving replaces the whole set; stored values are never shown again. The next deployment uses it.</p>
    <EnvEditor rows={rows} onChange={(r) => { setRows(r); setSaved(false); }} />
    <div className="toolbar">
      <button className="ghost" disabled={action.busy || rows.length === 0} onClick={save}>Replace environment</button>
      {saved && <span className="state">Saved. Deploy to apply.</span>}
    </div>
    <ErrorLine error={action.error} />
  </>;
}

/**
 * Deploy key (SSH remotes) or write-only token (HTTPS remotes). The deploy
 * key's private half never leaves the server; the token is never shown again.
 */
function GitAccessPanel({ projectId }: { projectId: string }) {
  const base = `/compose-projects/${projectId}/git-access`;
  const access = useApi<GitAccess>(base);
  const [username, setUsername] = useState('x-access-token');
  const [token, setToken] = useState('');
  const [copied, setCopied] = useState(false);
  const action = useAction();
  const data = access.data;

  const generate = () => {
    if (data?.deploy_public_key && !window.confirm('Replace the deploy key? Deployments fail until the new key is added on the Git host.')) return;
    action.run(async () => { await api(`${base}/deploy-key`, { method: 'POST' }); setCopied(false); access.reload(); });
  };
  const removeKey = () => action.run(async () => { await api(`${base}/deploy-key`, { method: 'DELETE' }); access.reload(); });
  const saveToken = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      await api(`${base}/token`, { method: 'PUT', body: { username: username.trim(), token } });
      setToken(''); access.reload();
    });
  };
  const removeToken = () => action.run(async () => { await api(`${base}/token`, { method: 'DELETE' }); access.reload(); });
  const copy = (key: string) => navigator.clipboard?.writeText(key).then(() => setCopied(true)).catch(() => undefined);

  return <>
    <h3>Git access</h3>
    <StateView state={access} />
    {data?.transport === 'ssh' && <div className="card">
      <p className="hint">Add this public key to the repository on <span className="mono">{data.host}</span> as a read-only deploy key. The private key stays on this server. The host key is trusted on first connection.</p>
      {data.deploy_public_key
        ? <p className="secret mono">{data.deploy_public_key}</p>
        : <p className="state">No deploy key yet; deployments connect without an SSH identity.</p>}
      <div className="toolbar">
        <button className="ghost" disabled={action.busy} onClick={generate}>{data.deploy_public_key ? 'Replace key' : 'Generate deploy key'}</button>
        {data.deploy_public_key && <button className="ghost" onClick={() => copy(data.deploy_public_key!)}>{copied ? 'Copied' : 'Copy'}</button>}
        {data.deploy_public_key && <button className="ghost danger" disabled={action.busy} onClick={removeKey}>Remove</button>}
      </div>
    </div>}
    {data?.transport === 'https' && <form className="card" onSubmit={saveToken}>
      <p className="hint">{data.token_host
        ? <>A token is saved for <span className="mono">{data.token_host}</span>. It is never shown again; saving replaces it.</>
        : <>Public repository, or no token yet. A token is offered only to <span className="mono">{data.host}</span>.</>}</p>
      <div className="grid2">
        <label>Username<input required className="mono" value={username} onChange={(e) => setUsername(e.target.value)} /></label>
        <label>Token<input required type="password" autoComplete="off" value={token} onChange={(e) => setToken(e.target.value)} /></label>
      </div>
      <div className="toolbar">
        <button className="ghost" disabled={action.busy || !token}>{data.token_host ? 'Replace token' : 'Save token'}</button>
        {data.token_host && <button type="button" className="ghost danger" disabled={action.busy} onClick={removeToken}>Remove token</button>}
      </div>
    </form>}
    {data && data.transport === null && <p className="state">This remote URL is not a supported Git transport.</p>}
    <ErrorLine error={action.error} />
  </>;
}

type WebhookStatus = { enabled: boolean; path: string; watch_paths: string[] };

function Webhook({ projectId }: { projectId: string }) {
  const status = useApi<WebhookStatus>(`/compose-projects/${projectId}/webhook`);
  const [secret, setSecret] = useState<string>();
  const [watchPaths, setWatchPaths] = useState<string>();
  const action = useAction();
  const rotate = () => action.run(async () => {
    const credentials = await api<{ path: string; secret: string }>(`/compose-projects/${projectId}/webhook`, { method: 'POST' });
    setSecret(credentials.secret); status.reload();
  });
  const disable = () => action.run(async () => {
    await api(`/compose-projects/${projectId}/webhook`, { method: 'DELETE' });
    setSecret(undefined); status.reload();
  });
  const saveWatchPaths = () => action.run(async () => {
    const lines = (watchPaths ?? '').split('\n').map((line) => line.trim()).filter(Boolean);
    await api(`/compose-projects/${projectId}/webhook`, { method: 'PUT', body: { watch_paths: lines } });
    setWatchPaths(undefined); status.reload();
  });
  const url = status.data ? `${window.location.origin}${status.data.path}` : '';
  const editedPaths = watchPaths ?? status.data?.watch_paths.join('\n') ?? '';

  return <>
    <h3>Push webhook</h3>
    <p className="hint">Deploys on pushes to the project branch. Requests must be signed (GitHub/Gitea secret) or carry the GitLab secret token.</p>
    <StateView state={status} />
    {status.data && <div className="card">
      <p>Payload URL <span className="mono">{url}</span> · content type <span className="mono">application/json</span></p>
      {secret && <p className="secret">Secret (shown once): <span className="mono">{secret}</span></p>}
      <div className="toolbar">
        <button className="ghost" disabled={action.busy} onClick={rotate}>{status.data.enabled ? 'Rotate secret' : 'Enable webhook'}</button>
        {status.data.enabled && <button className="ghost danger" disabled={action.busy} onClick={disable}>Disable</button>}
        <span className="state">{status.data.enabled ? 'enabled' : 'disabled'}</span>
      </div>
      <label>Watch paths
        <textarea className="mono" rows={3} placeholder={'src/**\ncompose.yml'} value={editedPaths}
          onChange={(e) => setWatchPaths(e.target.value)} />
      </label>
      <p className="hint">One glob per line, relative to the repository root (<span className="mono">*</span>, <span className="mono">?</span>, <span className="mono">**</span>). A directory covers everything below it. Leave empty to deploy every push. Pushes whose payload does not list all changed files always deploy.</p>
      <div className="toolbar">
        <button className="ghost" disabled={action.busy || watchPaths === undefined} onClick={saveWatchPaths}>Save watch paths</button>
      </div>
    </div>}
    <ErrorLine error={action.error} />
  </>;
}
