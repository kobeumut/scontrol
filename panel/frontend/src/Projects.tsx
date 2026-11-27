import { FormEvent, useEffect, useState } from 'react';
import {
  api, ComposeProject, DATABASE_ENGINE_LABELS, DatabaseService, Deployment, Domain, Environment, formatTime, kindLabel,
  LOCAL_SERVER_ID, Project, Server, ServiceInfo, sourceDetail, timeAgo, useApi,
} from './api';
import { LayeredVariables } from './Variables';

const toSlug = (name: string) => name.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 48);
const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`;

/** What "add to this environment" opens: the new-service form, the template catalogue, or the new-database form. */
export type AddKind = 'services' | 'templates' | 'databases';

type Navigation = {
  onOpenService: (serviceId: string) => void;
  onOpenDatabase: (serviceId: string) => void;
  onAdd: (kind: AddKind, environmentId: string) => void;
};

/**
 * Projects → environments → services. Variables set on a project apply to
 * every service in it; an environment's override the project's; a service's
 * own override both (merged at each deployment).
 */
export function Projects({ initialOpenId, ...navigation }: { initialOpenId?: string } & Navigation) {
  const projects = useApi<Project[]>('/projects');
  // Only for the status summary; the list works without it.
  const services = useApi<ComposeProject[]>('/compose-projects', 10000);
  const [openId, setOpenId] = useState<string | null>(initialOpenId ?? null);
  const [creating, setCreating] = useState(false);
  const open = projects.data?.find((p) => p.id === openId);

  if (open) return <ProjectDetail project={open} onBack={() => setOpenId(null)} onChanged={projects.reload} {...navigation} />;
  return <section className="panel">
    <div className="panel-title">
      <h2>Projects</h2>
      <button className="action" onClick={() => setCreating((c) => !c)}>{creating ? 'Close' : 'New project'}</button>
    </div>
    <p className="hint">A project groups environments (production, staging, …), and each environment holds the services and databases that run in it. Open a project to see what runs where. Variables are inherited project → environment → service, and the most specific layer wins.</p>
    {creating && <CreateProject onCreated={(id) => { setCreating(false); projects.reload(); setOpenId(id); }} />}
    {projects.loading && <p className="state">Loading…</p>}
    {projects.error && <p className="state error">{projects.error}</p>}
    {projects.data && <table className="stack-narrow"><thead><tr><th>Project</th><th>Slug</th><th>Environments</th><th>Services</th></tr></thead><tbody>
      {projects.data.length === 0
        ? <tr><td colSpan={4} className="empty">No projects yet. Create one with “New project”, then add services to its production environment.</td></tr>
        : projects.data.map((p) => {
          const total = p.environments.reduce((n, e) => n + e.service_count, 0);
          const environmentIds = new Set(p.environments.map((e) => e.id));
          const mine = (services.data ?? []).filter((s) => environmentIds.has(s.environment_id));
          const failed = mine.filter((s) => s.status === 'failed').length;
          const deploying = mine.filter((s) => s.status === 'running').length;
          return <tr key={p.id} className="clickable" onClick={() => setOpenId(p.id)}>
            <td data-label="Project"><button className="link strong" onClick={() => setOpenId(p.id)}>{p.name}</button></td>
            <td data-label="Slug" className="mono">{p.slug}</td>
            <td data-label="Environments"><div className="chips">{p.environments.map((e) => <span className="chip" key={e.id}>{e.name} <span className="muted">{e.service_count}</span></span>)}</div></td>
            <td data-label="Services">{total === 0 ? <span className="muted">none yet</span> : <>{total}
              {failed > 0 && <> <span className="badge failed">{failed} failed</span></>}
              {deploying > 0 && <> <span className="badge running">{deploying} deploying</span></>}</>}</td>
          </tr>;
        })}
    </tbody></table>}
  </section>;
}

function CreateProject({ onCreated }: { onCreated: (id: string) => void }) {
  const [name, setName] = useState('');
  const [slug, setSlug] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true); setError(undefined);
    try {
      const created = await api<Project>('/projects', { method: 'POST', body: { name: name.trim(), slug: slug.trim() } });
      onCreated(created.id);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  return <form className="card" onSubmit={submit}>
    <div className="grid2">
      <label>Name<input required maxLength={100} value={name} onChange={(e) => { setName(e.target.value); setSlug(toSlug(e.target.value)); }} /></label>
      <label>Slug<input required className="mono" pattern="[a-z][a-z0-9\-]{2,47}" value={slug} onChange={(e) => setSlug(e.target.value)} /></label>
    </div>
    <p className="hint">The project starts with a <span className="mono">production</span> environment.</p>
    {error && <p className="state error" role="alert">{error}</p>}
    <button className="action" disabled={busy}>{busy ? 'Creating…' : 'Create project'}</button>
  </form>;
}

function ProjectDetail({ project, onBack, onChanged, ...navigation }: { project: Project; onBack: () => void; onChanged: () => void } & Navigation) {
  const [envId, setEnvId] = useState<string>(project.environments[0]?.id ?? '');
  const [newEnv, setNewEnv] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [projectSaves, setProjectSaves] = useState(0);
  const [renaming, setRenaming] = useState<string>();
  const environment = project.environments.find((e) => e.id === envId) ?? project.environments[0];
  const base = `/projects/${project.id}`;
  const services = useApi<ComposeProject[]>('/compose-projects', 5000);
  const databases = useApi<DatabaseService[]>('/database-services', 15000);
  // Needs `servers:read`; without it the server column falls back to a generic name.
  const servers = useApi<Server[]>('/servers');
  const total = project.environments.reduce((n, e) => n + e.service_count, 0);

  // The counts on the environment tabs come from the project list.
  const serviceCount = services.data?.length;
  useEffect(() => { if (serviceCount !== undefined) onChanged(); }, [serviceCount]); // eslint-disable-line react-hooks/exhaustive-deps

  const run = async (action: () => Promise<unknown>) => {
    setBusy(true); setError(undefined);
    try { await action(); onChanged(); } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  const addEnvironment = (event: FormEvent) => {
    event.preventDefault();
    run(async () => {
      const created = await api<{ id: string }>(`${base}/environments`, { method: 'POST', body: { name: newEnv.trim() } });
      setNewEnv(''); setEnvId(created.id);
    });
  };
  const removeEnvironment = () => {
    if (!environment || !window.confirm(`Delete environment ${environment.name} and its variables?`)) return;
    run(async () => { await api(`${base}/environments/${environment.id}`, { method: 'DELETE' }); setEnvId(''); });
  };
  const rename = (event: FormEvent) => {
    event.preventDefault();
    run(async () => {
      await api(base, { method: 'PATCH', body: { name: (renaming ?? '').trim() } });
      setRenaming(undefined);
    });
  };
  const removeProject = () => {
    if (window.prompt(`Type ${project.slug} to delete this project, its environments and variables.`) !== project.slug) return;
    run(async () => { await api(base, { method: 'DELETE' }); onBack(); });
  };

  return <section className="panel">
    <button className="link" onClick={onBack}>← All projects</button>
    <div className="panel-title">
      <h2>{project.name} <span className="mono muted">{project.slug}</span></h2>
      <div className="toolbar">
        <button className="ghost" disabled={busy} onClick={() => setRenaming(renaming === undefined ? project.name : undefined)}>{renaming === undefined ? 'Rename' : 'Cancel rename'}</button>
        <button className="ghost danger" disabled={busy} onClick={removeProject}>Delete project</button>
      </div>
    </div>
    <p className="hint">{plural(project.environments.length, 'environment')} · {plural(total, 'service')} in total (databases included). Pick an environment to see what runs in it.</p>
    {renaming !== undefined && <form className="toolbar" onSubmit={rename}>
      <input aria-label="Project name" required maxLength={100} value={renaming} onChange={(e) => setRenaming(e.target.value)} />
      <button className="action" disabled={busy || !renaming.trim() || renaming.trim() === project.name}>Save name</button>
      <span className="hint">The slug stays <span className="mono">{project.slug}</span>.</span>
    </form>}
    {error && <p className="state error" role="alert">{error}</p>}

    <div className="env-bar">
      <div className="segmented" role="tablist" aria-label="Environment">
        {project.environments.map((e) => <button type="button" key={e.id} role="tab" aria-selected={environment?.id === e.id}
          className={environment?.id === e.id ? 'selected' : ''} onClick={() => setEnvId(e.id)}>
          {e.name} <span className="muted small">{e.service_count} service{e.service_count === 1 ? '' : 's'}</span>
        </button>)}
      </div>
      <form className="toolbar" onSubmit={addEnvironment}>
        <input aria-label="New environment name" placeholder="new environment, e.g. staging" maxLength={100} value={newEnv} onChange={(e) => setNewEnv(e.target.value)} />
        <button className="ghost" disabled={busy || !newEnv.trim()}>Add environment</button>
      </form>
    </div>

    {environment && <EnvironmentContents key={environment.id} environment={environment}
      services={services.data?.filter((s) => s.environment_id === environment.id)} loading={services.loading} error={services.error}
      databases={databases.data} servers={servers.data} {...navigation} />}

    <LayeredVariables key={`p-${project.id}`} path={`${base}/variables`} layer="project" title="Project variables"
      onSaved={() => setProjectSaves((n) => n + 1)}
      hint="Write-only. Shared by every environment and service of this project unless they override a key." />

    {environment && <LayeredVariables key={`e-${environment.id}`} path={`${base}/environments/${environment.id}/variables`}
      layer="environment" title={`${environment.name} variables`} refresh={projectSaves}
      hint="Write-only. Override project variables for the services of this environment; a service can override them again." />}

    {environment && <div className="section-head danger-zone">
      <button type="button" className="ghost danger" disabled={busy || environment.service_count > 0}
        title={environment.service_count > 0 ? 'Delete its services first' : undefined} onClick={removeEnvironment}>Delete {environment.name}</button>
      <span className="hint">{environment.service_count > 0
        ? `The ${environment.name} environment can be deleted once its services are deleted.`
        : `Removes the ${environment.name} environment and its variables.`}</span>
    </div>}
  </section>;
}

/** Everything that runs in one environment: services, then databases, each one click from its detail. */
function EnvironmentContents({ environment, services, loading, error, databases, servers, onOpenService, onOpenDatabase, onAdd }: {
  environment: Environment; services?: ComposeProject[]; loading: boolean; error?: string;
  databases?: DatabaseService[]; servers?: Server[];
} & Navigation) {
  const apps = services?.filter((s) => s.source.kind !== 'database') ?? [];
  const dbs = services?.filter((s) => s.source.kind === 'database') ?? [];
  const serverName = (id?: string) => !id || id === LOCAL_SERVER_ID ? 'this host' : servers?.find((s) => s.id === id)?.name ?? 'remote server';
  const add = <div className="toolbar add-actions">
    <button className="action" onClick={() => onAdd('services', environment.id)}>Add service</button>
    <button className="ghost" onClick={() => onAdd('templates', environment.id)}>Install template</button>
    <button className="ghost" onClick={() => onAdd('databases', environment.id)}>Add database</button>
  </div>;

  return <div className="env-contents" role="tabpanel" aria-label={`${environment.name} environment`}>
    <div className="section-head">
      <h3 className="section-title">Running in {environment.name}</h3>
      {add}
    </div>
    {!services && loading && <p className="state">Loading…</p>}
    {error && <p className="state error">{error}</p>}
    {services && apps.length === 0 && dbs.length === 0 && <div className="empty-state">
      <strong>Nothing runs in {environment.name} yet.</strong>
      <p>Add a service (a Compose file, a Git repository, an image, or a static site), install a ready-made application from the template catalogue, or add a database. Whatever you add here inherits this environment’s variables.</p>
    </div>}
    {apps.length > 0 && <table className="stack-narrow services-overview" aria-label={`Services in ${environment.name}`}>
      <thead><tr><th>Service</th><th>Type</th><th>Last deployment</th><th>Containers</th><th>Domains</th><th>Server</th></tr></thead>
      <tbody>{apps.map((s) => <ServiceRow key={s.id} service={s} server={serverName(s.server_id)} onOpen={() => onOpenService(s.id)} />)}</tbody>
    </table>}
    {dbs.length > 0 && <>
      <h4 className="subhead">Databases</h4>
      <table className="stack-narrow services-overview" aria-label={`Databases in ${environment.name}`}>
        <thead><tr><th>Database</th><th>Engine</th><th>Last deployment</th><th>Address for services here</th><th>Outside access</th><th>Server</th></tr></thead>
        <tbody>{dbs.map((s) => {
          const db = databases?.find((d) => d.project.id === s.id);
          return <tr key={s.id} className="clickable" onClick={() => onOpenDatabase(s.id)}>
            <td data-label="Database"><button className="link strong" onClick={() => onOpenDatabase(s.id)}>{s.name}</button><div className="mono small muted">{s.slug}</div></td>
            <td data-label="Engine">{s.source.kind === 'database' ? `${DATABASE_ENGINE_LABELS[s.source.engine]} ${s.source.version}` : '—'}</td>
            <td data-label="Last deployment"><span className={`badge ${s.status}`}>{s.status}</span></td>
            <td data-label="Address" className="mono small">{s.slug}{db ? <div className="muted">host: 127.0.0.1:{db.loopback_port}</div> : null}</td>
            <td data-label="Outside access" className="small">{!db ? '—' : db.exposure ? <>port <span className="mono">{db.exposure.public_port}</span></> : 'closed'}</td>
            <td data-label="Server" className="small">{serverName(s.server_id)}</td>
          </tr>;
        })}</tbody>
      </table>
    </>}
  </div>;
}

/** One service of an environment, with what its detail page would otherwise have to be opened for. */
function ServiceRow({ service, server, onOpen }: { service: ComposeProject; server: string; onOpen: () => void }) {
  const isStatic = service.source.kind === 'static';
  const containers = useApi<ServiceInfo[]>(isStatic ? null : `/compose-projects/${service.id}/services`, 15000);
  const domains = useApi<Domain[]>(`/compose-projects/${service.id}/domains`);
  const deployments = useApi<Deployment[]>(`/compose-projects/${service.id}/deployments`);
  const reloadContainers = containers.reload, reloadDeployments = deployments.reload;
  useEffect(() => { reloadContainers(); reloadDeployments(); }, [service.status, reloadContainers, reloadDeployments]);
  const latest = deployments.data?.[0];
  const neverDeployed = deployments.data !== undefined && !latest;
  const up = containers.data?.filter((c) => c.state === 'running' && c.health !== 'unhealthy').length ?? 0;
  const all = containers.data?.length ?? 0;

  return <tr className="clickable" onClick={onOpen}>
    <td data-label="Service"><button className="link strong" onClick={onOpen}>{service.name}</button><div className="mono small muted">{service.slug}</div></td>
    <td data-label="Type">{deployments.data?.some((d) => d.trigger === 'template') ? 'Template · ' : ''}{kindLabel(service.source)}<div className="small muted wrap-anywhere">{sourceDetail(service.source)}</div></td>
    <td data-label="Last deployment">{neverDeployed
      ? <span className="badge stopped">not deployed</span>
      : <><span className={`badge ${service.status}`}>{service.status}</span>
        {latest && <div className="small muted" title={formatTime(latest.finished_at ?? latest.queued_at)}>{timeAgo(latest.finished_at ?? latest.queued_at)} · {latest.trigger}</div>}</>}</td>
    <td data-label="Containers">{isStatic ? <span className="small muted">none (served by Nginx)</span>
      : containers.error ? <span className="small muted" title={containers.error}>unavailable</span>
      : !containers.data ? <span className="small muted">…</span>
      : all === 0 ? <span className="small muted">none running</span>
      : <><span className={`badge ${up === all ? 'succeeded' : 'failed'}`}>{up}/{all} running</span>
        <div className="small muted wrap-anywhere">{[...new Set(containers.data.map((c) => c.name))].join(', ')}</div></>}</td>
    <td data-label="Domains">{!domains.data ? <span className="small muted">{domains.error ? 'unavailable' : '…'}</span>
      : domains.data.length === 0 ? <span className="small muted">none</span>
      : domains.data.map((d) => <a key={d.id} className="domain-link" href={`${d.https ? 'https' : 'http'}://${d.domain}`} target="_blank" rel="noreferrer"
        onClick={(e) => e.stopPropagation()}>{d.domain}</a>)}</td>
    <td data-label="Server" className="small">{server}</td>
  </tr>;
}
