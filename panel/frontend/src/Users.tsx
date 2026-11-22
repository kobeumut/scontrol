import { FormEvent, useState } from 'react';
import { api, ApiError, formatTime, useApi, User } from './api';
import { ErrorLine, StateView, useAction } from './ui';

type PanelUser = User & { created_at: string };

/** Panel users (installation admins only): create, and reset a lost second factor. */
export function Users({ onChanged, currentUserId }: { onChanged?: () => void; currentUserId?: string }) {
  const users = useApi<PanelUser[]>('/admin/users');
  const [creating, setCreating] = useState(false);
  const action = useAction();
  const [message, setMessage] = useState<string>();

  const resetTotp = (user: PanelUser) => {
    if (!window.confirm(`Turn off two-factor authentication for ${user.username}? They can sign in with the password alone until they set it up again.`)) return;
    action.run(async () => { await api(`/admin/users/${user.id}/disable-totp`, { method: 'POST' }); users.reload(); });
  };
  const remove = (user: PanelUser) => {
    if (!window.confirm(`Delete user ${user.username}? Their sessions, API keys, and role bindings are removed. This cannot be undone.`)) return;
    setMessage(undefined);
    action.run(async () => {
      try {
        await api(`/admin/users/${user.id}`, { method: 'DELETE' });
      } catch (error) {
        // The user still owns sites or services: offer to remove them from
        // their hosts first, so nothing keeps running unmanaged.
        if (!(error instanceof ApiError && error.status === 409 && /site\(s\)/.test(error.message))) throw error;
        const typed = window.prompt(
          `${user.username} ${error.message.replace(/^user /, '').replace(/;.*$/, '')}. Deleting the user also deletes all of them: `
          + 'containers, vhosts, scheduled jobs, cron jobs and host databases are removed from their servers (named Docker volumes are kept). '
          + `Type ${user.username} to delete everything.`);
        if (typed === null) return;
        if (typed.trim() !== user.username) throw new Error('The username did not match; nothing was deleted.');
        const result = await api<{ removed: { sites: string[]; services: string[] } | null }>(
          `/admin/users/${user.id}?delete_resources=true`, { method: 'DELETE' });
        setMessage(`Deleted ${user.username} with ${result.removed?.sites.length ?? 0} site(s) and ${result.removed?.services.length ?? 0} service(s).`);
      }
      users.reload(); onChanged?.();
    });
  };

  return <section className="panel">
    <div className="panel-title">
      <h2>Users</h2>
      <button className="action" onClick={() => setCreating((c) => !c)}>{creating ? 'Close' : 'New user'}</button>
    </div>
    <p className="hint">A new user can sign in but sees nothing until a role binding below gives them access. Admins hold every permission. Deleting a user who owns sites or services removes those from their servers too, after you type the username. The last admin cannot be deleted.</p>
    {creating && <CreateUser onCreated={() => { setCreating(false); users.reload(); onChanged?.(); }} />}
    <StateView state={users} />
    <ErrorLine error={action.error} />
    {message && <p className="state" role="status">{message}</p>}
    {users.data && <table><thead><tr><th>Username</th><th>Email</th><th>Role</th><th>2FA</th><th>Created</th><th /></tr></thead><tbody>
      {users.data.map((user) => <tr key={user.id}>
        <td className="mono">{user.username}</td>
        <td>{user.email}</td>
        <td>{user.is_admin ? 'admin' : 'user'}</td>
        <td>{user.totp_enabled ? 'enabled' : '—'}</td>
        <td className="small">{formatTime(user.created_at)}</td>
        <td className="actions">
          {user.totp_enabled && <button className="link danger" disabled={action.busy} onClick={() => resetTotp(user)}>reset 2FA</button>}
          {user.id !== currentUserId && <button className="link danger" disabled={action.busy} onClick={() => remove(user)}>delete</button>}
        </td>
      </tr>)}
    </tbody></table>}
  </section>;
}

function CreateUser({ onCreated }: { onCreated: () => void }) {
  const [username, setUsername] = useState('');
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [isAdmin, setIsAdmin] = useState(false);
  const action = useAction();
  const submit = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      await api('/admin/users', { method: 'POST', body: { username: username.trim(), email: email.trim(), password, is_admin: isAdmin } });
      onCreated();
    });
  };
  return <form className="card" onSubmit={submit}>
    <div className="grid2">
      <label>Username<input required className="mono" autoComplete="off" pattern="[a-z][a-z0-9_\-]{2,31}"
        title="3 to 32 lowercase letters, digits, - or _, starting with a letter" value={username} onChange={(e) => setUsername(e.target.value)} /></label>
      <label>Email<input required type="email" autoComplete="off" value={email} onChange={(e) => setEmail(e.target.value)} /></label>
      <label>Password <span className="muted small">(at least 12 characters)</span>
        <input required type="password" autoComplete="new-password" minLength={12} value={password} onChange={(e) => setPassword(e.target.value)} /></label>
      <label className="check"><input type="checkbox" checked={isAdmin} onChange={(e) => setIsAdmin(e.target.checked)} /> Installation admin (every permission)</label>
    </div>
    <ErrorLine error={action.error} />
    <button className="action" disabled={action.busy}>{action.busy ? 'Creating…' : 'Create user'}</button>
  </form>;
}
