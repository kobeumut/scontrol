import { FormEvent, useEffect, useState } from 'react';
import { api, useApi } from './api';
import { ErrorLine, StateView, useAction } from './ui';

interface PreviewSettings {
  enabled: boolean;
  base_domain: string;
  service_name: string;
  container_port: number;
  https: boolean;
  wildcard_certificate: string | null;
  max_active: number;
  ttl_hours: number;
  memory_mb: number;
  cpu_millis: number;
  has_comment_token: boolean;
  updated_at: string;
}

interface Preview {
  id: string;
  preview_project_id: string;
  slug: string;
  provider: 'github' | 'gitlab' | 'gitea';
  pr_number: number;
  head_branch: string;
  head_sha: string;
  status: 'active' | 'closing';
  close_reason: string | null;
  project_status: string;
  domain: string | null;
  last_activity_at: string;
  created_at: string;
}

interface PreviewOverview {
  available: boolean;
  unavailable_reason: string | null;
  webhook_enabled: boolean;
  settings: PreviewSettings | null;
  previews: Preview[];
}

interface Form {
  enabled: boolean;
  base_domain: string;
  service_name: string;
  container_port: string;
  https: boolean;
  wildcard_certificate: string;
  max_active: string;
  ttl_hours: string;
  memory_mb: string;
  cpus: string;
}

function toForm(settings: PreviewSettings | null): Form {
  return {
    enabled: settings?.enabled ?? false,
    base_domain: settings?.base_domain ?? '',
    service_name: settings?.service_name ?? '',
    container_port: String(settings?.container_port ?? 80),
    https: settings?.https ?? false,
    wildcard_certificate: settings?.wildcard_certificate ?? '',
    max_active: String(settings?.max_active ?? 5),
    ttl_hours: String(settings?.ttl_hours ?? 48),
    memory_mb: String(settings?.memory_mb ?? 512),
    cpus: String((settings?.cpu_millis ?? 500) / 1000),
  };
}

/**
 * Pull request previews: each open pull request from the same repository
 * gets `<slug>-pr-<n>` on `<slug>-pr-<n>.<base domain>`, removed when the
 * request closes or after the TTL. Events arrive on the push webhook.
 */
export function Previews({ projectId, onOpenProject }: { projectId: string; onOpenProject?: (id: string) => void }) {
  const overview = useApi<PreviewOverview>(`/compose-projects/${projectId}/previews`, 10000);
  const [form, setForm] = useState<Form>(toForm(null));
  const [dirty, setDirty] = useState(false);
  const [token, setToken] = useState('');
  const action = useAction();

  useEffect(() => {
    if (!dirty && overview.data) setForm(toForm(overview.data.settings));
  }, [overview.data, dirty]);

  const update = (patch: Partial<Form>) => { setForm({ ...form, ...patch }); setDirty(true); };

  const save = (event: FormEvent) => {
    event.preventDefault();
    if (overview.data?.settings?.enabled && !form.enabled
      && !window.confirm('Disabling previews removes every preview of this project. Continue?')) return;
    action.run(async () => {
      await api(`/compose-projects/${projectId}/previews`, {
        method: 'PUT',
        body: {
          enabled: form.enabled,
          base_domain: form.base_domain.trim(),
          service_name: form.service_name.trim(),
          container_port: Number(form.container_port),
          https: form.https,
          wildcard_certificate: form.wildcard_certificate.trim() || null,
          max_active: Number(form.max_active),
          ttl_hours: Number(form.ttl_hours),
          resource_limits: {
            memory_mb: Number(form.memory_mb),
            cpu_millis: Math.round(Number(form.cpus) * 1000),
          },
        },
      });
      setDirty(false);
      overview.reload();
    });
  };
  const saveToken = () => action.run(async () => {
    await api(`/compose-projects/${projectId}/previews/comment-token`, { method: 'PUT', body: { token: token.trim() } });
    setToken('');
    overview.reload();
  });
  const removeToken = () => action.run(async () => {
    await api(`/compose-projects/${projectId}/previews/comment-token`, { method: 'DELETE' });
    overview.reload();
  });
  const removePreview = (preview: Preview) => {
    if (!window.confirm(`Remove ${preview.slug}? Its containers and volumes are deleted; a new push recreates it.`)) return;
    action.run(async () => {
      await api(`/compose-projects/${projectId}/previews/${preview.pr_number}`, { method: 'DELETE' });
      overview.reload();
    });
  };

  const data = overview.data;
  const settings = data?.settings;
  return <>
    <h3>Pull request previews</h3>
    <p className="hint">Each pull request opened from this repository (never from a fork) is deployed as its own project on a
      subdomain, redeployed on every push, and removed when it is closed or merged, or after the TTL without activity.
      Previews use this service's variables and run with memory, CPU and process limits.</p>
    <StateView state={overview} />
    {data && !data.available && <p className="state">{data.unavailable_reason}</p>}
    {data && data.available && <div className="card">
      {!data.webhook_enabled && <p className="state error">Enable the push webhook below and subscribe it to pull request (GitHub/Gitea) or merge request (GitLab) events.</p>}
      <form onSubmit={save}>
        <label className="check"><input type="checkbox" checked={form.enabled} onChange={(e) => update({ enabled: e.target.checked })} /> enabled</label>
        <div className="inline-form">
          <label>Base domain
            <input required placeholder="preview.example.com" value={form.base_domain} onChange={(e) => update({ base_domain: e.target.value })} />
          </label>
          <label>Service
            <input required className="mono" placeholder="web" value={form.service_name} onChange={(e) => update({ service_name: e.target.value })} />
          </label>
          <label>Port
            <input required type="number" min={1} max={65535} className="port" value={form.container_port} onChange={(e) => update({ container_port: e.target.value })} />
          </label>
        </div>
        <p className="hint">Point a wildcard DNS record <span className="mono">*.{form.base_domain || 'preview.example.com'}</span> at this server.
          Previews are served on <span className="mono">&lt;slug&gt;-pr-&lt;n&gt;.{form.base_domain || 'preview.example.com'}</span>.</p>
        <div className="inline-form">
          <label className="check"><input type="checkbox" checked={form.https} onChange={(e) => update({ https: e.target.checked })} /> HTTPS</label>
          <label>Wildcard certificate (optional)
            <input placeholder="preview.example.com" disabled={!form.https} value={form.wildcard_certificate}
              onChange={(e) => update({ wildcard_certificate: e.target.value })} />
          </label>
        </div>
        <p className="hint">With a wildcard certificate name, previews use that existing certbot lineage (issued with a DNS
          challenge outside the panel). Without one, a certificate is requested for each preview host.</p>
        <div className="inline-form">
          <label>Active limit
            <input required type="number" min={1} max={50} className="port" value={form.max_active} onChange={(e) => update({ max_active: e.target.value })} />
          </label>
          <label>TTL (hours)
            <input required type="number" min={1} max={720} className="port" value={form.ttl_hours} onChange={(e) => update({ ttl_hours: e.target.value })} />
          </label>
          <label>Memory (MiB)
            <input required type="number" min={64} max={65536} className="port" value={form.memory_mb} onChange={(e) => update({ memory_mb: e.target.value })} />
          </label>
          <label>CPUs
            <input required type="number" min={0.1} max={64} step={0.05} className="port" value={form.cpus} onChange={(e) => update({ cpus: e.target.value })} />
          </label>
        </div>
        <div className="toolbar">
          <button className="action" disabled={action.busy || !dirty}>Save preview settings</button>
        </div>
      </form>

      {settings && <>
        <h4>Pull request comments</h4>
        <p className="hint">The preview link is posted on the pull request and updated on each deployment. GitHub projects linked
          to a GitHub App use it (the App needs pull request write access); otherwise store an API token with permission to
          comment. The token is write-only.</p>
        <div className="inline-form">
          <input type="password" aria-label="Comment token" placeholder={settings.has_comment_token ? 'token stored — replace' : 'API token'}
            autoComplete="off" value={token} onChange={(e) => setToken(e.target.value)} />
          <button type="button" className="ghost" disabled={action.busy || !token.trim()} onClick={saveToken}>Save token</button>
          {settings.has_comment_token && <button type="button" className="ghost danger" disabled={action.busy} onClick={removeToken}>Remove token</button>}
        </div>
      </>}

      {data.previews.length > 0 && <table><thead><tr><th>Request</th><th>Branch</th><th>URL</th><th>Status</th><th>Last activity</th><th /></tr></thead><tbody>
        {data.previews.map((p) => <tr key={p.id}>
          <td>#{p.pr_number} <span className="muted">{p.provider}</span></td>
          <td className="mono">{p.head_branch} <span className="muted">{p.head_sha.slice(0, 7)}</span></td>
          <td>{p.domain
            ? <a href={`${settings?.https ? 'https' : 'http'}://${p.domain}`} target="_blank" rel="noreferrer">{p.domain}</a>
            : '—'}</td>
          <td><span className={`badge ${p.status === 'closing' ? 'cancelled' : p.project_status}`}>{p.status === 'closing' ? `removing (${p.close_reason ?? '—'})` : p.project_status}</span></td>
          <td>{new Date(p.last_activity_at).toLocaleString()}</td>
          <td>
            {onOpenProject && <button className="link" onClick={() => onOpenProject(p.preview_project_id)}>open</button>}{' '}
            {p.status === 'active' && <button className="link danger" disabled={action.busy} onClick={() => removePreview(p)}>remove</button>}
          </td>
        </tr>)}
      </tbody></table>}
      {settings && data.previews.length === 0 && <p className="state">No previews yet.</p>}
    </div>}
    <ErrorLine error={action.error} />
  </>;
}
