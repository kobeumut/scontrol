import { FormEvent, useState } from 'react';
import { api, formatTime, useApi } from './api';

type Kind = 'slack' | 'discord' | 'telegram' | 'ntfy' | 'gotify' | 'webhook' | 'email';

type Channel = {
  id: string; name: string; kind: Kind; has_secret: boolean; events: string[]; enabled: boolean;
  last_sent_at: string | null; last_error: string | null;
  chat_id?: string; server_url?: string; topic?: string; url?: string;
  host?: string; port?: number; security?: 'tls' | 'starttls' | 'none'; username?: string | null; from?: string; to?: string[];
};

/** Settings fields per kind, and what the kind's single write-only secret is. */
const KINDS: Record<Kind, { label: string; secret: string; secretRequired: boolean; fields: [string, string][] }> = {
  slack: { label: 'Slack', secret: 'Webhook URL', secretRequired: true, fields: [] },
  discord: { label: 'Discord', secret: 'Webhook URL', secretRequired: true, fields: [] },
  telegram: { label: 'Telegram', secret: 'Bot token', secretRequired: true, fields: [['chat_id', 'Chat ID']] },
  ntfy: { label: 'ntfy', secret: 'Access token (optional)', secretRequired: false, fields: [['server_url', 'Server URL'], ['topic', 'Topic']] },
  gotify: { label: 'Gotify', secret: 'Application token', secretRequired: true, fields: [['server_url', 'Server URL']] },
  webhook: { label: 'Webhook', secret: 'Signing secret (optional)', secretRequired: false, fields: [['url', 'URL']] },
  email: { label: 'Email (SMTP)', secret: 'SMTP password', secretRequired: false, fields: [['host', 'SMTP host'], ['port', 'Port'], ['username', 'Username (optional)'], ['from', 'From'], ['to', 'To (comma-separated)']] },
};

const EVENT_LABELS: Record<string, string> = {
  deploy_succeeded: 'Deploy succeeded', deploy_failed: 'Deploy failed', deploy_cancelled: 'Deploy cancelled',
};

export function Notifications() {
  const channels = useApi<Channel[]>('/notifications');
  const [editing, setEditing] = useState<Channel | 'new' | null>(null);
  const [message, setMessage] = useState<string>();
  const [busy, setBusy] = useState(false);

  const run = async (action: () => Promise<string | undefined>) => {
    setBusy(true); setMessage(undefined);
    try { setMessage(await action()); } catch (e) { setMessage((e as Error).message); } finally { setBusy(false); }
  };
  const test = (c: Channel) => run(async () => {
    const result = await api<{ delivered: boolean; error?: string }>(`/notifications/${c.id}/test`, { method: 'POST' });
    channels.reload();
    return result.delivered ? `Test sent to ${c.name}.` : `Test to ${c.name} failed: ${result.error}`;
  });
  const remove = (c: Channel) => {
    if (!window.confirm(`Delete notification channel ${c.name}?`)) return;
    run(async () => { await api(`/notifications/${c.id}`, { method: 'DELETE' }); channels.reload(); return undefined; });
  };

  return <section className="panel">
    <div className="panel-title">
      <h2>Notifications</h2>
      {editing === null && <button className="action" onClick={() => setEditing('new')}>Add channel</button>}
    </div>
    <p className="hint">Deployment results are sent to every enabled channel subscribed to the event. Secrets are write-only and stored encrypted.</p>
    {editing !== null && <ChannelForm channel={editing === 'new' ? undefined : editing}
      onDone={() => { setEditing(null); channels.reload(); }} />}
    {channels.loading && <p className="state">Loading…</p>}
    {channels.error && <p className="state error">{channels.error}</p>}
    {message && <p className="state" role="status">{message}</p>}
    {channels.data && (channels.data.length === 0 ? <p className="state">No channels yet.</p>
      : <table><thead><tr><th>Name</th><th>Kind</th><th>Events</th><th>Last result</th><th /></tr></thead><tbody>
        {channels.data.map((c) => <tr key={c.id}>
          <td>{c.name}{!c.enabled && <span className="muted small"> (disabled)</span>}</td>
          <td>{KINDS[c.kind].label}</td>
          <td className="small">{c.events.map((e) => EVENT_LABELS[e] ?? e).join(', ')}</td>
          <td className="small">{c.last_error ? <span className="state error">{c.last_error}</span>
            : c.last_sent_at ? `sent ${formatTime(c.last_sent_at)}` : '—'}</td>
          <td>
            <button className="link" disabled={busy} onClick={() => test(c)}>test</button>{' '}
            <button className="link" disabled={busy} onClick={() => setEditing(c)}>edit</button>{' '}
            <button className="link danger" disabled={busy} onClick={() => remove(c)}>delete</button>
          </td>
        </tr>)}
      </tbody></table>)}
  </section>;
}

function ChannelForm({ channel, onDone }: { channel?: Channel; onDone: () => void }) {
  const [kind, setKind] = useState<Kind>(channel?.kind ?? 'slack');
  const [name, setName] = useState(channel?.name ?? '');
  const [fields, setFields] = useState<Record<string, string>>(() => ({
    chat_id: channel?.chat_id ?? '', server_url: channel?.server_url ?? '', topic: channel?.topic ?? '',
    url: channel?.url ?? '', host: channel?.host ?? '', port: String(channel?.port ?? 587),
    username: channel?.username ?? '', from: channel?.from ?? '', to: channel?.to?.join(', ') ?? '',
  }));
  const [security, setSecurity] = useState(channel?.security ?? 'starttls');
  const [events, setEvents] = useState<string[]>(channel?.events ?? ['deploy_failed']);
  const [enabled, setEnabled] = useState(channel?.enabled ?? true);
  const [secret, setSecret] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const spec = KINDS[kind];
  // An existing secret is kept on save unless a new one is typed (same kind only).
  const keepsSecret = channel?.has_secret && channel.kind === kind;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    const body: Record<string, unknown> = { name, kind, events, enabled };
    for (const [key] of spec.fields) {
      if (key === 'port') body.port = Number(fields.port);
      else if (key === 'to') body.to = fields.to.split(',').map((s) => s.trim()).filter(Boolean);
      else if (key === 'username') body.username = fields.username.trim() || null;
      else body[key] = fields[key].trim();
    }
    if (kind === 'email') body.security = security;
    if (secret || !keepsSecret) body.secret = secret;
    setBusy(true); setError(undefined);
    try {
      await api(channel ? `/notifications/${channel.id}` : '/notifications', { method: channel ? 'PUT' : 'POST', body });
      onDone();
    } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };

  return <form className="card" onSubmit={submit}>
    <div className="grid2">
      <label>Name<input required maxLength={100} value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label>Kind<select value={kind} onChange={(e) => setKind(e.target.value as Kind)}>
        {(Object.keys(KINDS) as Kind[]).map((k) => <option key={k} value={k}>{KINDS[k].label}</option>)}
      </select></label>
      {spec.fields.map(([key, label]) => <label key={key}>{label}
        <input className={key === 'chat_id' || key === 'topic' ? 'mono' : undefined} value={fields[key]}
          type={key === 'port' ? 'number' : 'text'} onChange={(e) => setFields({ ...fields, [key]: e.target.value })} />
      </label>)}
      {kind === 'email' && <label>Security<select value={security} onChange={(e) => setSecurity(e.target.value as typeof security)}>
        <option value="starttls">STARTTLS</option><option value="tls">TLS</option><option value="none">none (loopback relay only)</option>
      </select></label>}
      <label>{spec.secret}<input type="password" autoComplete="off" value={secret}
        required={spec.secretRequired && !keepsSecret}
        placeholder={keepsSecret ? 'stored; leave empty to keep' : ''} onChange={(e) => setSecret(e.target.value)} /></label>
    </div>
    <div className="toolbar">
      {Object.entries(EVENT_LABELS).map(([key, label]) => <label className="check" key={key}>
        <input type="checkbox" checked={events.includes(key)}
          onChange={(e) => setEvents(e.target.checked ? [...events, key] : events.filter((x) => x !== key))} /> {label}
      </label>)}
      <label className="check"><input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} /> enabled</label>
    </div>
    {error && <p className="state error" role="alert">{error}</p>}
    <div className="toolbar">
      <button className="action" disabled={busy}>{channel ? 'Save channel' : 'Create channel'}</button>
      <button type="button" className="ghost" onClick={onDone}>Cancel</button>
    </div>
  </form>;
}
