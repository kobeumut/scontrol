import { FormEvent, useState } from 'react';
import { api, Deployment, Domain, SecretChange, VhostOptions } from './api';
import { ErrorLine, useAction } from './ui';

const EMPTY: VhostOptions = { redirect: null, path_prefix: null, basic_auth: false, custom_cert: false };

/** Redirect, path prefix, basic auth, and custom certificate for one domain. */
export function DomainOptions({ projectId, domain, onSaved }: {
  projectId: string; domain: Domain; onSaved: (deployment?: Deployment) => void;
}) {
  const current = domain.options ?? EMPTY;
  const [redirectOn, setRedirectOn] = useState(current.redirect !== null);
  const [status, setStatus] = useState<301 | 302>(current.redirect?.status ?? 301);
  const [target, setTarget] = useState(current.redirect?.target ?? '');
  const [preserve, setPreserve] = useState(current.redirect?.preserve_path ?? true);
  const [prefix, setPrefix] = useState(current.path_prefix ?? '');
  const [authMode, setAuthMode] = useState<'keep' | 'set' | 'remove'>('keep');
  const [users, setUsers] = useState('');
  const [certMode, setCertMode] = useState<'keep' | 'set' | 'remove'>('keep');
  const [cert, setCert] = useState('');
  const [key, setKey] = useState('');
  const action = useAction();

  const save = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      let basic_auth: SecretChange<{ username: string; password: string }[]> = { action: 'keep' };
      if (authMode === 'remove') basic_auth = { action: 'remove' };
      if (authMode === 'set') {
        // One `user:password` per line; the password never leaves the request.
        const parsed = users.split('\n').map((l) => l.trim()).filter(Boolean).map((line) => {
          const at = line.indexOf(':');
          if (at < 1) throw new Error('Use one user:password per line.');
          return { username: line.slice(0, at), password: line.slice(at + 1) };
        });
        basic_auth = { action: 'set', value: parsed };
      }
      const certificate: SecretChange<{ certificate_pem: string; private_key_pem: string }> =
        certMode === 'set' ? { action: 'set', value: { certificate_pem: cert, private_key_pem: key } }
          : certMode === 'remove' ? { action: 'remove' } : { action: 'keep' };
      const options: VhostOptions = {
        redirect: redirectOn ? { status, target: target.trim(), preserve_path: preserve } : null,
        path_prefix: prefix.trim() || null,
        basic_auth: authMode === 'set' || (authMode === 'keep' && current.basic_auth),
        custom_cert: certMode === 'set' || (certMode === 'keep' && current.custom_cert),
      };
      const result = await api<{ deployment?: Deployment }>(
        `/compose-projects/${projectId}/domains/${domain.id}/options`,
        { method: 'PUT', body: { options, basic_auth, certificate } });
      setUsers(''); setCert(''); setKey(''); setAuthMode('keep'); setCertMode('keep');
      onSaved(result.deployment);
    });
  };

  return <form className="card" onSubmit={save}>
    <h4>Options for {domain.domain}</h4>
    <p className="hint">Nginx is validated with <span className="mono">nginx -t</span> on deploy and rolled back if it rejects the result. Passwords and keys are write-only.</p>
    <label className="check"><input type="checkbox" checked={redirectOn} onChange={(e) => setRedirectOn(e.target.checked)} /> Redirect all requests</label>
    {redirectOn && <div className="inline-form">
      <select aria-label="Redirect status" value={status} onChange={(e) => setStatus(Number(e.target.value) as 301 | 302)}>
        <option value={301}>301 permanent</option><option value={302}>302 temporary</option>
      </select>
      <input required aria-label="Redirect target" className="mono" placeholder="https://example.com" value={target} onChange={(e) => setTarget(e.target.value)} />
      <label className="check"><input type="checkbox" checked={preserve} onChange={(e) => setPreserve(e.target.checked)} /> keep path</label>
    </div>}
    {!redirectOn && <label>Path prefix <span className="muted small">(empty serves at /)</span>
      <input className="mono" placeholder="/app/" value={prefix} onChange={(e) => setPrefix(e.target.value)} /></label>}
    {!redirectOn && <>
      <label>HTTP Basic Auth {current.basic_auth && <span className="muted small">(enabled)</span>}
        <select value={authMode} onChange={(e) => setAuthMode(e.target.value as typeof authMode)}>
          <option value="keep">keep as is</option><option value="set">set users</option>
          {current.basic_auth && <option value="remove">disable</option>}
        </select></label>
      {authMode === 'set' && <textarea required aria-label="Basic auth users" className="mono" rows={3} placeholder={'alice:a-long-password'} value={users} onChange={(e) => setUsers(e.target.value)} />}
    </>}
    {domain.https && <>
      <label>Custom TLS certificate {current.custom_cert && <span className="muted small">(uploaded)</span>}
        <select value={certMode} onChange={(e) => setCertMode(e.target.value as typeof certMode)}>
          <option value="keep">keep as is</option><option value="set">upload</option>
          {current.custom_cert && <option value="remove">use Let's Encrypt again</option>}
        </select></label>
      {certMode === 'set' && <>
        <textarea required aria-label="Certificate PEM" className="mono" rows={4} placeholder="-----BEGIN CERTIFICATE-----" value={cert} onChange={(e) => setCert(e.target.value)} />
        <textarea required aria-label="Private key PEM" className="mono" rows={4} placeholder="-----BEGIN PRIVATE KEY-----" value={key} onChange={(e) => setKey(e.target.value)} autoComplete="off" />
      </>}
    </>}
    <button className="action" disabled={action.busy}>Save and redeploy</button>
    <ErrorLine error={action.error} />
  </form>;
}
