import { FormEvent, useState } from 'react';
import { api, User } from './api';
import { ErrorLine, useAction } from './ui';

/** The signed-in user's own account: two-factor authentication. */
export function Account({ user, onChanged }: { user: User; onChanged: () => void }) {
  const [enrollment, setEnrollment] = useState<{ secret: string; otpauth_url: string }>();
  const [code, setCode] = useState('');
  const action = useAction();

  const start = () => action.run(async () => {
    setEnrollment(await api('/auth/totp/enroll', { method: 'POST' }));
    setCode('');
  });
  const verify = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      await api('/auth/totp/verify', { method: 'POST', body: { code } });
      setEnrollment(undefined);
      onChanged();
    });
  };

  return <section className="panel">
    <div className="panel-title"><h2>Account</h2></div>
    <table><tbody>
      <tr><th>Username</th><td className="mono">{user.username}</td></tr>
      <tr><th>Email</th><td>{user.email}</td></tr>
      <tr><th>Role</th><td>{user.is_admin ? 'installation admin' : 'user'}</td></tr>
      <tr><th>Two-factor authentication</th><td>{user.totp_enabled ? 'enabled' : 'not enabled'}</td></tr>
    </tbody></table>

    <h3>Two-factor authentication</h3>
    {user.totp_enabled
      ? <p className="hint">Signing in asks for a 6-digit code from your authenticator app. To move to another device, ask an installation admin to reset 2FA (Access → Users), then set it up again here.</p>
      : !enrollment ? <>
        <p className="hint">Add a time-based one-time code (TOTP) from an authenticator app as a second step when signing in.</p>
        <button className="action" disabled={action.busy} onClick={start}>Set up 2FA</button>
      </> : <form className="card" onSubmit={verify}>
        <p>Add this key to your authenticator app (or open the setup link on the device that has it), then enter the code it shows. 2FA turns on only after the code is accepted.</p>
        <label>Setup key<div className="secret mono" data-testid="totp-secret">{enrollment.secret}</div></label>
        <p className="small"><a href={enrollment.otpauth_url}>Open in authenticator app</a></p>
        <label>6-digit code<input required autoFocus inputMode="numeric" autoComplete="one-time-code" pattern="[0-9]{6}" value={code} onChange={(e) => setCode(e.target.value)} /></label>
        <div className="toolbar">
          <button className="action" disabled={action.busy}>Enable 2FA</button>
          <button type="button" className="ghost" onClick={() => setEnrollment(undefined)}>Cancel</button>
        </div>
      </form>}
    <ErrorLine error={action.error} />
  </section>;
}
