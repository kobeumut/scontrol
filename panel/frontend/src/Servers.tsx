import { FormEvent, useState } from 'react';
import { api, formatTime, LOCAL_SERVER_ID, Server, useApi } from './api';
import { ErrorLine, StateView, useAction } from './ui';

type Enrolled = { server: Server; certificate_pem: string | null; ca_pem: string | null };
type CertificateInfo = { certificate_pem: string; ca_pem: string };
type CheckResult = { ok: boolean; message: string | null; server: Server };

const TRANSPORT_LABELS: Record<Server['transport'], string> = {
  local: 'this host (socket)', mtls: 'mutual TLS', ssh_socket: 'SSH-forwarded socket',
};

/** Remote agents: enrollment from a CSR, certificate rotation, draining, and placement capacity. */
export function Servers({ canManage }: { canManage: boolean }) {
  const servers = useApi<Server[]>('/servers', 30000);
  const [adding, setAdding] = useState(false);
  const [bundle, setBundle] = useState<{ title: string } & CertificateInfo>();
  const [rotating, setRotating] = useState<Server>();
  const action = useAction();
  const [message, setMessage] = useState<string>();

  const check = (s: Server) => action.run(async () => {
    const result = await api<CheckResult>(`/servers/${s.id}/check`, { method: 'POST' });
    setMessage(result.ok ? `${s.name} answered.` : `${s.name} did not answer: ${result.message}`);
    servers.reload();
  });
  const toggleDrain = (s: Server) => action.run(async () => {
    await api(`/servers/${s.id}`, { method: 'PUT', body: { name: s.name, draining: !s.draining, max_services: s.max_services } });
    servers.reload();
  });
  const setCapacity = (s: Server) => {
    const raw = window.prompt(`Most services ${s.name} may hold (empty for no limit):`, s.max_services?.toString() ?? '');
    if (raw === null) return;
    const max = raw.trim() === '' ? null : Number(raw);
    if (max !== null && (!Number.isInteger(max) || max < 0)) { action.fail('Enter a whole number.'); return; }
    action.run(async () => {
      await api(`/servers/${s.id}`, { method: 'PUT', body: { name: s.name, draining: s.draining, max_services: max } });
      servers.reload();
    });
  };
  const showCertificate = (s: Server) => action.run(async () => {
    setBundle({ title: s.name, ...await api<CertificateInfo>(`/servers/${s.id}/certificate`) });
  });
  const remove = (s: Server) => {
    if (!window.confirm(`Remove server ${s.name}? Its certificate stops being accepted.`)) return;
    action.run(async () => { await api(`/servers/${s.id}`, { method: 'DELETE' }); servers.reload(); });
  };

  return <section className="panel">
    <div className="panel-title">
      <h2>Servers</h2>
      {canManage && !adding && <button className="action" onClick={() => { setAdding(true); setBundle(undefined); }}>Add server</button>}
    </div>
    <p className="hint">New services go to the least loaded online server that is not draining and has room, unless you pick one. A service stays on its server; its files, containers, and Nginx vhost live there. Point a service's domains at the server it runs on.</p>
    {adding && <AddServer onDone={(enrolled) => {
      setAdding(false); servers.reload();
      if (enrolled?.certificate_pem && enrolled.ca_pem) setBundle({ title: enrolled.server.name, certificate_pem: enrolled.certificate_pem, ca_pem: enrolled.ca_pem });
    }} />}
    {rotating && <RotateCertificate server={rotating} onDone={(info) => {
      setRotating(undefined); servers.reload();
      if (info) setBundle({ title: `${rotating.name} (rotated)`, ...info });
    }} />}
    {bundle && <CertificateBundle {...bundle} onClose={() => setBundle(undefined)} />}
    <StateView state={servers} />
    <ErrorLine error={action.error} />
    {message && <p className="state" role="status">{message}</p>}
    {servers.data && <table><thead><tr>
      <th>Name</th><th>Transport</th><th>Status</th><th>Services</th><th>Certificate</th><th />
    </tr></thead><tbody>
      {servers.data.map((s) => <tr key={s.id}>
        <td><strong>{s.name}</strong>{s.draining && <> <span className="badge">draining</span></>}
          <div className="mono small muted">{s.id === LOCAL_SERVER_ID ? 'panel host' : s.address}</div></td>
        <td className="small">{TRANSPORT_LABELS[s.transport]}</td>
        <td><span className={`badge ${s.status === 'online' ? 'succeeded' : s.status === 'offline' ? 'failed' : 'queued'}`}>{s.status}</span>
          <div className="small muted">{s.agent_version ?? ''}{s.docker ? ' · docker' : ''}</div>
          {s.last_error && <div className="small" style={{ color: 'var(--red)' }}>{s.last_error}</div>}
          {s.id !== LOCAL_SERVER_ID && <div className="small muted">seen {formatTime(s.last_seen_at)}</div>}</td>
        <td>{s.service_count}{s.max_services !== null ? ` / ${s.max_services}` : ''}</td>
        <td className="small">{s.transport !== 'mtls' ? '—' : <>
          expires {formatTime(s.cert_expires_at)}
          {s.rotation_pending && <div><span className="badge running">rotation pending</span></div>}
          <div className="mono small muted" title={s.cert_fingerprint ?? ''}>{s.cert_fingerprint?.slice(0, 16)}…</div>
        </>}</td>
        <td className="actions">
          <button className="link" disabled={action.busy} onClick={() => check(s)}>check</button>
          {canManage && <button className="link" disabled={action.busy} onClick={() => toggleDrain(s)}>{s.draining ? 'undrain' : 'drain'}</button>}
          {canManage && <button className="link" disabled={action.busy} onClick={() => setCapacity(s)}>capacity</button>}
          {s.transport === 'mtls' && <button className="link" disabled={action.busy} onClick={() => showCertificate(s)}>certificate</button>}
          {canManage && s.transport === 'mtls' && <button className="link" disabled={action.busy} onClick={() => { setRotating(s); setBundle(undefined); }}>rotate</button>}
          {canManage && s.id !== LOCAL_SERVER_ID && <button className="link danger" disabled={action.busy || s.service_count > 0}
            title={s.service_count > 0 ? 'Delete its services first' : undefined} onClick={() => remove(s)}>remove</button>}
        </td>
      </tr>)}
    </tbody></table>}
  </section>;
}

function AddServer({ onDone }: { onDone: (enrolled?: Enrolled) => void }) {
  const [transport, setTransport] = useState<'mtls' | 'ssh_socket'>('mtls');
  const [name, setName] = useState('');
  const [address, setAddress] = useState('');
  const [csr, setCsr] = useState('');
  const [max, setMax] = useState('');
  const action = useAction();
  const host = transport === 'mtls' ? address.replace(/:\d+$/, '').replace(/^\[|\]$/g, '') || '<host>' : '';

  const submit = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      onDone(await api<Enrolled>('/servers', {
        method: 'POST',
        body: { name: name.trim(), transport, address: address.trim(), csr_pem: transport === 'mtls' ? csr : undefined, max_services: max ? Number(max) : null },
      }));
    });
  };

  return <form className="card" onSubmit={submit}>
    <div className="segmented" role="group" aria-label="Transport">
      <button type="button" className={transport === 'mtls' ? 'selected' : ''} onClick={() => setTransport('mtls')}>Mutual TLS</button>
      <button type="button" className={transport === 'ssh_socket' ? 'selected' : ''} onClick={() => setTransport('ssh_socket')}>SSH-forwarded socket</button>
    </div>
    <div className="grid3">
      <label>Name<input required className="mono" pattern="[a-z0-9][a-z0-9-]*[a-z0-9]" maxLength={40} placeholder="web-2" value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label>{transport === 'mtls' ? 'Address (host:port)' : 'Forwarded socket'}
        <input required className="mono" placeholder={transport === 'mtls' ? 'web-2.example.com:7843' : `/run/scontrol/remote/${name || '<name>'}.sock`}
          value={address} onChange={(e) => setAddress(e.target.value)} /></label>
      <label>Max services<input type="number" min={0} placeholder="no limit" value={max} onChange={(e) => setMax(e.target.value)} /></label>
    </div>
    {transport === 'mtls' ? <>
      <p className="hint">On the new server, install panel-agent and run <span className="mono">panel-agent remote-init --host {host}</span>. It writes a private key that never leaves that host and prints a certificate signing request. Paste the request here; the panel signs it for this address only.</p>
      <label>Certificate signing request<textarea required className="mono" rows={7} placeholder="-----BEGIN CERTIFICATE REQUEST-----" value={csr} onChange={(e) => setCsr(e.target.value)} /></label>
    </> : <p className="hint">Keep one SSH connection forwarding the remote agent socket here, for example a systemd unit running <span className="mono">ssh -N -L /run/scontrol/remote/{name || '<name>'}.sock:/run/scontrol/agent.sock root@host</span> as the scontrol user. The panel talks to it like the local socket.</p>}
    <ErrorLine error={action.error} />
    <div className="toolbar">
      <button className="action" disabled={action.busy}>{transport === 'mtls' ? 'Sign and add' : 'Add server'}</button>
      <button type="button" className="ghost" onClick={() => onDone()}>Cancel</button>
    </div>
  </form>;
}

function RotateCertificate({ server, onDone }: { server: Server; onDone: (info?: CertificateInfo) => void }) {
  const [csr, setCsr] = useState('');
  const action = useAction();
  const submit = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => onDone(await api<CertificateInfo>(`/servers/${server.id}/certificate`, { method: 'POST', body: { csr_pem: csr } })));
  };
  return <form className="card" onSubmit={submit}>
    <strong>Rotate the certificate of {server.name}</strong>
    <p className="hint">Run <span className="mono">panel-agent remote-init --host {server.address.replace(/:\d+$/, '')} --force</span> on the server and paste the new request. Until the agent presents the new certificate (or for seven days), the current one keeps working; the agent picks up a replaced certificate file without a restart.</p>
    <label>Certificate signing request<textarea required className="mono" rows={7} value={csr} onChange={(e) => setCsr(e.target.value)} /></label>
    <ErrorLine error={action.error} />
    <div className="toolbar">
      <button className="action" disabled={action.busy}>Sign new certificate</button>
      <button type="button" className="ghost" onClick={() => onDone()}>Cancel</button>
    </div>
  </form>;
}

function CertificateBundle({ title, certificate_pem, ca_pem, onClose }: { title: string; onClose: () => void } & CertificateInfo) {
  return <div className="card">
    <div className="panel-title"><strong>Install on {title}</strong><button className="ghost" onClick={onClose}>Done</button></div>
    <p className="hint">Save these next to the key in <span className="mono">/etc/scontrol/remote/</span> and add a <span className="mono">[remote]</span> section with <span className="mono">listen = "0.0.0.0:7843"</span> to agent.toml. Both files are public; the private key stays on the server.</p>
    <label>agent.pem<textarea readOnly className="mono" rows={6} value={certificate_pem} onFocus={(e) => e.currentTarget.select()} /></label>
    <label>ca.pem<textarea readOnly className="mono" rows={6} value={ca_pem} onFocus={(e) => e.currentTarget.select()} /></label>
  </div>;
}

/** Where a new service runs; empty lets the panel place it. */
export function ServerSelect({ value, onChange }: { value: string; onChange: (id: string) => void }) {
  const servers = useApi<Server[]>('/servers');
  if (servers.error || (servers.data && servers.data.length < 2)) return null;
  return <label>Server
    <select value={value} disabled={servers.loading} onChange={(e) => onChange(e.target.value)}>
      <option value="">automatic (least loaded)</option>
      {servers.data?.map((s) => <option key={s.id} value={s.id} disabled={s.status !== 'online' || s.draining}>
        {s.name}{s.status !== 'online' ? ` (${s.status})` : s.draining ? ' (draining)' : ''}
      </option>)}
    </select>
  </label>;
}
