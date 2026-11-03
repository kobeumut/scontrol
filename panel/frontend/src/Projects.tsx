import { FormEvent, useState } from 'react';
import { api, Project, useApi } from './api';
import { LayeredVariables } from './Variables';

const toSlug = (name: string) => name.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 48);

/**
 * Projects → environments → services. Variables set on a project apply to
 * every service in it; an environment's override the project's; a service's
 * own override both (merged at each deployment).
 */
export function Projects() {
  const projects = useApi<Project[]>('/projects');
  const [openId, setOpenId] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const open = projects.data?.find((p) => p.id === openId);

  if (open) return <ProjectDetail project={open} onBack={() => setOpenId(null)} onChanged={projects.reload} />;
  return <section className="panel">
    <div className="panel-title">
      <h2>Projects</h2>
      <button className="action" onClick={() => setCreating((c) => !c)}>{creating ? 'Close' : 'New project'}</button>
    </div>
    <p className="hint">A project groups environments (production, staging, …); each environment holds Compose services. Variables are inherited project → environment → service, and the most specific layer wins.</p>
    {creating && <CreateProject onCreated={(id) => { setCreating(false); projects.reload(); setOpenId(id); }} />}
    {projects.loading && <p className="state">Loading…</p>}
    {projects.error && <p className="state error">{projects.error}</p>}
    {projects.data && <table><thead><tr><th>Project</th><th>Slug</th><th>Environments</th><th>Services</th></tr></thead><tbody>
      {projects.data.length === 0
        ? <tr><td colSpan={4} className="empty">No projects yet. Creating a Compose project adds a Default project.</td></tr>
        : projects.data.map((p) => <tr key={p.id} className="clickable" onClick={() => setOpenId(p.id)}>
          <td><button className="link strong" onClick={() => setOpenId(p.id)}>{p.name}</button></td>
          <td className="mono">{p.slug}</td>
          <td>{p.environments.map((e) => e.name).join(', ')}</td>
          <td>{p.environments.reduce((n, e) => n + e.service_count, 0)}</td>
        </tr>)}
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
      <label>Slug<input required className="mono" pattern="[a-z][a-z0-9-]{2,47}" value={slug} onChange={(e) => setSlug(e.target.value)} /></label>
    </div>
    <p className="hint">The project starts with a <span className="mono">production</span> environment.</p>
    {error && <p className="state error" role="alert">{error}</p>}
    <button className="action" disabled={busy}>{busy ? 'Creating…' : 'Create project'}</button>
  </form>;
}

function ProjectDetail({ project, onBack, onChanged }: { project: Project; onBack: () => void; onChanged: () => void }) {
  const [envId, setEnvId] = useState<string>(project.environments[0]?.id ?? '');
  const [newEnv, setNewEnv] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const environment = project.environments.find((e) => e.id === envId) ?? project.environments[0];
  const base = `/projects/${project.id}`;

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
  const removeProject = () => {
    if (window.prompt(`Type ${project.slug} to delete this project, its environments and variables.`) !== project.slug) return;
    run(async () => { await api(base, { method: 'DELETE' }); onBack(); });
  };

  return <section className="panel">
    <button className="link" onClick={onBack}>← All projects</button>
    <div className="panel-title">
      <h2>{project.name} <span className="mono muted">{project.slug}</span></h2>
      <button className="ghost danger" disabled={busy} onClick={removeProject}>Delete project</button>
    </div>
    {error && <p className="state error" role="alert">{error}</p>}

    <LayeredVariables key={`p-${project.id}`} path={`${base}/variables`} layer="project" title="Project variables"
      hint="Write-only. Shared by every environment and service of this project unless they override a key." />

    <h3>Environments</h3>
    <div className="segmented" role="tablist" aria-label="Environment">
      {project.environments.map((e) => <button type="button" key={e.id} role="tab" aria-selected={environment?.id === e.id}
        className={environment?.id === e.id ? 'selected' : ''} onClick={() => setEnvId(e.id)}>
        {e.name} <span className="muted small">{e.service_count} service{e.service_count === 1 ? '' : 's'}</span>
      </button>)}
    </div>
    <form className="toolbar" onSubmit={addEnvironment}>
      <input aria-label="New environment name" placeholder="staging" maxLength={100} value={newEnv} onChange={(e) => setNewEnv(e.target.value)} />
      <button className="ghost" disabled={busy || !newEnv.trim()}>Add environment</button>
      <span className="spacer" />
      {environment && <button type="button" className="ghost danger" disabled={busy || environment.service_count > 0}
        title={environment.service_count > 0 ? 'Delete its services first' : undefined} onClick={removeEnvironment}>Delete {environment.name}</button>}
    </form>

    {environment && <LayeredVariables key={`e-${environment.id}`} path={`${base}/environments/${environment.id}/variables`}
      layer="environment" title={`${environment.name} variables`}
      hint="Write-only. Override project variables for the services of this environment; a service can override them again." />}
  </section>;
}
