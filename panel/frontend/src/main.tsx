import { useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import './styles.css';

type Capability = { docker: boolean; mail: boolean; antivirus: boolean; version: string };
type Site = { id: string; domain: string; status: string; ssl_enabled: boolean; runtime: { kind: string } };
type ComposeProject = { id: string; name: string; slug: string; status: string; source: { kind: string } };

type ApiState<T> = { loading: boolean; data?: T; error?: string };

async function api<T>(path: string): Promise<T> {
  const response = await fetch(`/api${path}`, { credentials: 'same-origin' });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new Error(body.error || `Request failed (${response.status})`);
  }
  return response.json() as Promise<T>;
}

function useApi<T>(path: string) {
  const [state, setState] = useState<ApiState<T>>({ loading: true });
  useEffect(() => {
    let active = true;
    api<T>(path).then((data) => active && setState({ loading: false, data }))
      .catch((error: Error) => active && setState({ loading: false, error: error.message }));
    return () => { active = false; };
  }, [path]);
  return state;
}

function StateView({ state }: { state: ApiState<unknown> }) {
  if (state.loading) return <p className="state">Loading operational state…</p>;
  if (state.error) return <p className="state error">{state.error}. Sign in to access panel data.</p>;
  return null;
}

function App() {
  const health = useApi<{ status: string; capabilities?: Capability }>('/health/agent');
  const sites = useApi<Site[]>('/sites/');
  const projects = useApi<ComposeProject[]>('/compose-projects/');
  const [active, setActive] = useState<'sites' | 'projects'>('sites');

  return <main className="shell">
    <header className="masthead">
      <div><span className="mark">S/</span><span className="product">scontrol</span></div>
      <div className="system-state"><i className={health.data?.status === 'ok' ? 'lamp up' : 'lamp'} /> agent {health.data?.status ?? 'checking'}</div>
    </header>

    <section className="hero">
      <p className="eyebrow">SERVER OPERATIONS / LOCAL FIRST</p>
      <h1>Clear control.<br /><em>Minimal surface.</em></h1>
      <p className="intro">Sites, deployments and host services in one deliberately compact control plane.</p>
    </section>

    <section className="capabilities" aria-label="Installed capabilities">
      {(['docker', 'mail', 'antivirus'] as const).map((feature) => <div className="cap" key={feature}>
        <span>{feature}</span><strong>{health.data?.capabilities?.[feature] ? 'installed' : 'not selected'}</strong>
      </div>)}
      <div className="cap"><span>agent</span><strong>{health.data?.capabilities?.version ?? '—'}</strong></div>
    </section>

    <nav className="tabs" aria-label="Primary content">
      <button className={active === 'sites' ? 'selected' : ''} onClick={() => setActive('sites')}>Hosted sites</button>
      <button className={active === 'projects' ? 'selected' : ''} onClick={() => setActive('projects')}>Compose projects</button>
    </nav>

    {active === 'sites' ? <section className="panel">
      <div className="panel-title"><h2>Hosted sites</h2><button className="action" disabled>New site</button></div>
      <StateView state={sites} />
      {sites.data && <table><thead><tr><th>Domain</th><th>Runtime</th><th>Status</th><th>TLS</th></tr></thead><tbody>
        {sites.data.length === 0 ? <tr><td colSpan={4} className="empty">No sites provisioned.</td></tr> : sites.data.map((site) => <tr key={site.id}><td>{site.domain}</td><td>{site.runtime.kind}</td><td><span className={`badge ${site.status}`}>{site.status}</span></td><td>{site.ssl_enabled ? 'enabled' : '—'}</td></tr>)}
      </tbody></table>}
    </section> : <section className="panel">
      <div className="panel-title"><h2>Compose projects</h2><button className="action" disabled>Deploy project</button></div>
      <p className="hint">Dokploy-inspired deployments: isolated source, write-only environment, Compose project identity and bounded logs.</p>
      <StateView state={projects} />
      {projects.data && <table><thead><tr><th>Project</th><th>Slug</th><th>Source</th><th>Status</th></tr></thead><tbody>
        {projects.data.length === 0 ? <tr><td colSpan={4} className="empty">No Compose projects deployed.</td></tr> : projects.data.map((project) => <tr key={project.id}><td>{project.name}</td><td className="mono">{project.slug}</td><td>{project.source.kind}</td><td><span className={`badge ${project.status}`}>{project.status}</span></td></tr>)}
      </tbody></table>}
    </section>}
  </main>;
}

createRoot(document.getElementById('root')!).render(<App />);
