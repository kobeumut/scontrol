import { FormEvent, useState } from 'react';
import { ScopePicker } from './Access';
import { AccessMe, api, ApiKey, formatTime, Grant, PermissionInfo, Project, Scope, useApi } from './api';
import { ErrorLine, StateView, useAction } from './ui';

function describeScope(scope: Scope, projects: Project[]): string {
  if (scope.kind === 'global') return 'global';
  const project = projects.find((p) => p.id === scope.project_id);
  const name = project?.name ?? scope.project_id.slice(0, 8);
  if (scope.kind === 'project') return name;
  const env = project?.environments.find((e) => e.id === scope.environment_id);
  return `${name} / ${env?.name ?? scope.environment_id.slice(0, 8)}`;
}

/** API keys for CI and automation. A token is shown once; the panel keeps only an Argon2id hash. */
export function ApiKeys({ me }: { me?: AccessMe }) {
  const keys = useApi<ApiKey[]>('/api-keys');
  const projects = useApi<Project[]>('/projects');
  const catalog = useApi<PermissionInfo[]>('/access/permissions');
  const [creating, setCreating] = useState(false);
  const [token, setToken] = useState<{ name: string; token: string }>();
  const action = useAction();

  const revoke = (k: ApiKey) => {
    if (!window.confirm(`Revoke ${k.name}? Anything using it stops working immediately.`)) return;
    action.run(async () => { await api(`/api-keys/${k.id}`, { method: 'DELETE' }); keys.reload(); });
  };

  return <section className="panel">
    <div className="panel-title">
      <h2>API keys</h2>
      {!creating && <button className="action" onClick={() => { setCreating(true); setToken(undefined); }}>New key</button>}
    </div>
    <p className="hint">Send a key as <span className="mono">Authorization: Bearer sck_…</span>. A key holds only the permissions you give it, never more than you hold yourself, and loses them when you do. Keys cannot manage keys, roles, or server-wide settings.</p>
    {token && <div className="card">
      <strong>Copy the token for {token.name} now. It is not shown again.</strong>
      <div className="secret mono">{token.token}</div>
      <div className="toolbar"><button className="ghost" onClick={() => navigator.clipboard?.writeText(token.token)}>Copy</button>
        <button className="ghost" onClick={() => setToken(undefined)}>Done</button></div>
    </div>}
    {creating && projects.data && catalog.data && <KeyForm projects={projects.data} catalog={catalog.data.filter((p) => p.api_keys)} me={me}
      onDone={(created) => { setCreating(false); keys.reload(); if (created) setToken(created); }} />}
    <StateView state={keys} />
    <ErrorLine error={action.error} />
    {keys.data && <table className="compact"><thead><tr><th>Name</th><th>Permissions</th><th>Last used</th><th>Expires</th><th /></tr></thead><tbody>
      {keys.data.length === 0 ? <tr><td colSpan={5} className="empty">No API keys.</td></tr>
        : keys.data.map((k) => <tr key={k.id} className={k.revoked_at ? 'muted' : ''}>
          <td><strong>{k.name}</strong><div className="mono small muted">sck_{k.prefix}_…</div></td>
          <td className="small">{k.grants.map((g, i) => <div key={i}><span className="mono">{g.permission}</span> · {describeScope(g.scope, projects.data ?? [])}</div>)}</td>
          <td className="small">{formatTime(k.last_used_at)}</td>
          <td className="small">{k.revoked_at ? <span className="badge failed">revoked</span> : k.expires_at ? formatTime(k.expires_at) : 'never'}</td>
          <td className="actions">{!k.revoked_at && <button className="link danger" disabled={action.busy} onClick={() => revoke(k)}>revoke</button>}</td>
        </tr>)}
    </tbody></table>}
  </section>;
}

function KeyForm({ projects, catalog, me, onDone }: {
  projects: Project[]; catalog: PermissionInfo[]; me?: AccessMe; onDone: (created?: { name: string; token: string }) => void;
}) {
  const [name, setName] = useState('');
  const [expires, setExpires] = useState('90');
  const [grants, setGrants] = useState<Grant[]>([]);
  const [permission, setPermission] = useState(catalog.find((p) => p.key === 'services:deploy')?.key ?? catalog[0]?.key);
  const [scope, setScope] = useState<Scope>({ kind: 'global' });
  const action = useAction();
  const info = catalog.find((p) => p.key === permission);

  const add = () => {
    if (!permission) return;
    const effective: Scope = info?.scopable ? scope : { kind: 'global' };
    if (!grants.some((g) => g.permission === permission && JSON.stringify(g.scope) === JSON.stringify(effective))) {
      setGrants([...grants, { permission, scope: effective }]);
    }
  };
  const submit = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      const created = await api<{ token: string }>('/api-keys', {
        method: 'POST', body: { name: name.trim(), grants, expires_in_days: expires ? Number(expires) : null },
      });
      onDone({ name: name.trim(), token: created.token });
    });
  };

  return <form className="card" onSubmit={submit}>
    <div className="grid2">
      <label>Name<input required maxLength={100} placeholder="github-actions deploy" value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label>Expires after (days)<input type="number" min={1} max={3650} placeholder="never" value={expires} onChange={(e) => setExpires(e.target.value)} /></label>
    </div>
    <fieldset><legend>Permissions</legend>
      <label>Permission<select value={permission} onChange={(e) => setPermission(e.target.value as typeof permission)}>
        {catalog.map((p) => <option key={p.key} value={p.key}>{p.key} — {p.description}</option>)}
      </select></label>
      {info?.scopable ? <ScopePicker projects={projects} value={scope} onChange={setScope} />
        : <p className="hint">{permission} exists only globally.</p>}
      <div className="toolbar"><button type="button" className="ghost" onClick={add}>Add permission</button></div>
      {grants.length > 0 && <table className="compact"><tbody>
        {grants.map((g, i) => <tr key={i}><td className="mono small">{g.permission}</td><td className="small">{describeScope(g.scope, projects)}</td>
          <td className="actions"><button type="button" className="link danger" onClick={() => setGrants(grants.filter((_, j) => j !== i))}>remove</button></td></tr>)}
      </tbody></table>}
    </fieldset>
    {me && !me.superuser && <p className="hint">You can only give a key permissions your roles grant you at that scope.</p>}
    <ErrorLine error={action.error} />
    <div className="toolbar">
      <button className="action" disabled={action.busy || grants.length === 0}>Create key</button>
      <button type="button" className="ghost" onClick={() => onDone()}>Cancel</button>
    </div>
  </form>;
}
