import { FormEvent, useMemo, useState } from 'react';
import { api, Permission, PermissionInfo, Project, Role, RoleBinding, Scope, useApi } from './api';
import { ErrorLine, StateView, useAction } from './ui';

type UserSummary = { id: string; username: string; is_admin: boolean };

/** Picks global, project, or environment scope. */
export function ScopePicker({ projects, value, onChange }: { projects: Project[]; value: Scope; onChange: (scope: Scope) => void }) {
  const projectId = value.kind === 'global' ? '' : value.project_id;
  const environmentId = value.kind === 'environment' ? value.environment_id : '';
  const project = projects.find((p) => p.id === projectId);
  return <div className="grid2">
    <label>Project
      <select value={projectId} onChange={(e) => onChange(e.target.value ? { kind: 'project', project_id: e.target.value } : { kind: 'global' })}>
        <option value="">everywhere (global)</option>
        {projects.map((p) => <option key={p.id} value={p.id}>{p.name}</option>)}
      </select>
    </label>
    <label>Environment
      <select value={environmentId} disabled={!project}
        onChange={(e) => onChange(e.target.value ? { kind: 'environment', project_id: projectId, environment_id: e.target.value } : { kind: 'project', project_id: projectId })}>
        <option value="">whole project</option>
        {project?.environments.map((env) => <option key={env.id} value={env.id}>{env.name}</option>)}
      </select>
    </label>
  </div>;
}

export function scopeLabel(binding: Pick<RoleBinding, 'project_name' | 'environment_name'>): string {
  if (!binding.project_name) return 'global';
  return binding.environment_name ? `${binding.project_name} / ${binding.environment_name}` : binding.project_name;
}

/** Roles and role bindings. Installation admins always hold every permission. */
export function Access() {
  const roles = useApi<Role[]>('/access/roles');
  const bindings = useApi<RoleBinding[]>('/access/bindings');
  const users = useApi<UserSummary[]>('/access/users');
  const projects = useApi<Project[]>('/projects');
  const catalog = useApi<PermissionInfo[]>('/access/permissions');
  const [editing, setEditing] = useState<Role | 'new' | null>(null);
  const action = useAction();

  const removeBinding = (b: RoleBinding) => {
    if (!window.confirm(`Remove ${b.role_name} from ${b.username} (${scopeLabel(b)})?`)) return;
    action.run(async () => { await api(`/access/bindings/${b.id}`, { method: 'DELETE' }); bindings.reload(); });
  };
  const removeRole = (r: Role) => {
    if (!window.confirm(`Delete role ${r.name}? Its bindings go with it.`)) return;
    action.run(async () => { await api(`/access/roles/${r.id}`, { method: 'DELETE' }); roles.reload(); bindings.reload(); });
  };

  if (bindings.error) return <section className="panel"><p className="state">{bindings.error === 'forbidden' ? 'Managing access needs the access:manage permission.' : bindings.error}</p></section>;

  return <section className="panel">
    <div className="panel-title"><h2>Access</h2></div>
    <p className="hint">A binding gives a user a role everywhere, in one project, or in one environment. Checks fail closed: without a binding that covers a resource, the request is refused. Users marked admin hold every permission. You can only hand out permissions you hold yourself.</p>

    <h3>Role bindings</h3>
    {users.data && roles.data && projects.data && <BindingForm users={users.data} roles={roles.data} projects={projects.data} onDone={bindings.reload} />}
    <StateView state={bindings} />
    <ErrorLine error={action.error} />
    {bindings.data && <table className="compact"><thead><tr><th>User</th><th>Role</th><th>Scope</th><th /></tr></thead><tbody>
      {bindings.data.length === 0 ? <tr><td colSpan={4} className="empty">No bindings. Only admins have access.</td></tr>
        : bindings.data.map((b) => <tr key={b.id}>
          <td>{b.username}</td><td><span className="badge">{b.role_name}</span></td><td>{scopeLabel(b)}</td>
          <td className="actions"><button className="link danger" disabled={action.busy} onClick={() => removeBinding(b)}>remove</button></td>
        </tr>)}
    </tbody></table>}

    <div className="panel-title subsection"><h3>Roles</h3>
      {editing === null && <button className="ghost" onClick={() => setEditing('new')}>New role</button>}</div>
    {editing !== null && catalog.data && <RoleForm role={editing === 'new' ? undefined : editing} catalog={catalog.data}
      onDone={() => { setEditing(null); roles.reload(); }} />}
    <StateView state={roles} />
    {roles.data && <table className="compact"><thead><tr><th>Role</th><th>Permissions</th><th /></tr></thead><tbody>
      {roles.data.map((r) => <tr key={r.id}>
        <td><strong>{r.name}</strong>{r.builtin && <span className="muted small"> · built-in</span>}<div className="small muted">{r.description}</div></td>
        <td className="mono small">{r.permissions.join(', ')}</td>
        <td className="actions">{!r.builtin && <>
          <button className="link" onClick={() => setEditing(r)}>edit</button>
          <button className="link danger" disabled={action.busy} onClick={() => removeRole(r)}>delete</button>
        </>}</td>
      </tr>)}
    </tbody></table>}
  </section>;
}

function BindingForm({ users, roles, projects, onDone }: { users: UserSummary[]; roles: Role[]; projects: Project[]; onDone: () => void }) {
  const [userId, setUserId] = useState('');
  const [roleId, setRoleId] = useState('');
  const [scope, setScope] = useState<Scope>({ kind: 'global' });
  const action = useAction();
  const submit = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      await api('/access/bindings', { method: 'POST', body: { user_id: userId, role_id: roleId, scope } });
      onDone();
    });
  };
  return <form className="card" onSubmit={submit}>
    <div className="grid2">
      <label>User<select required value={userId} onChange={(e) => setUserId(e.target.value)}>
        <option value="" disabled>choose…</option>
        {users.map((u) => <option key={u.id} value={u.id}>{u.username}{u.is_admin ? ' (admin)' : ''}</option>)}
      </select></label>
      <label>Role<select required value={roleId} onChange={(e) => setRoleId(e.target.value)}>
        <option value="" disabled>choose…</option>
        {roles.map((r) => <option key={r.id} value={r.id}>{r.name}</option>)}
      </select></label>
    </div>
    <ScopePicker projects={projects} value={scope} onChange={setScope} />
    <ErrorLine error={action.error} />
    <div className="toolbar"><button className="action" disabled={action.busy || !userId || !roleId}>Add binding</button></div>
  </form>;
}

function RoleForm({ role, catalog, onDone }: { role?: Role; catalog: PermissionInfo[]; onDone: () => void }) {
  const [name, setName] = useState(role?.name ?? '');
  const [description, setDescription] = useState(role?.description ?? '');
  const [selected, setSelected] = useState<Set<Permission>>(new Set(role?.permissions ?? []));
  const action = useAction();
  const groups = useMemo(() => {
    const byResource = new Map<string, PermissionInfo[]>();
    for (const p of catalog) {
      const resource = p.key.split(':')[0];
      byResource.set(resource, [...(byResource.get(resource) ?? []), p]);
    }
    return [...byResource.entries()];
  }, [catalog]);
  const toggle = (p: Permission) => setSelected((s) => { const next = new Set(s); if (next.has(p)) next.delete(p); else next.add(p); return next; });
  const submit = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      await api(role ? `/access/roles/${role.id}` : '/access/roles', { method: role ? 'PUT' : 'POST', body: { name: name.trim(), description, permissions: [...selected] } });
      onDone();
    });
  };
  return <form className="card" onSubmit={submit}>
    <div className="grid2">
      <label>Name<input required className="mono" pattern="[a-z0-9][a-z0-9-]*[a-z0-9]" maxLength={40} value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label>Description<input maxLength={200} value={description} onChange={(e) => setDescription(e.target.value)} /></label>
    </div>
    {groups.map(([resource, permissions]) => <fieldset key={resource}><legend>{resource}</legend>
      {permissions.map((p) => <label key={p.key} className="check" title={p.description}>
        <input type="checkbox" checked={selected.has(p.key)} onChange={() => toggle(p.key)} />
        <span className="mono">{p.key}</span> <span className="muted small">{p.description}{p.scopable ? '' : ' (global bindings only)'}</span>
      </label>)}
    </fieldset>)}
    <ErrorLine error={action.error} />
    <div className="toolbar">
      <button className="action" disabled={action.busy || selected.size === 0}>{role ? 'Save role' : 'Create role'}</button>
      <button type="button" className="ghost" onClick={onDone}>Cancel</button>
    </div>
  </form>;
}
