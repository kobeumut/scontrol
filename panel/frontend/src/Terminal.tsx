import { FitAddon } from '@xterm/addon-fit';
import { Terminal as XTerm } from '@xterm/xterm';
import '@xterm/xterm/css/xterm.css';
import { useEffect, useRef, useState } from 'react';

type Shell = 'sh' | 'bash' | 'ash';

/**
 * Interactive shell inside a service container (admin only). The server runs
 * `docker exec -it` for the chosen service; there is no host shell.
 */
export function Terminal({ projectId, services }: { projectId: string; services: string[] }) {
  const [service, setService] = useState(services[0] ?? '');
  const [shell, setShell] = useState<Shell>('sh');
  const [open, setOpen] = useState(false);
  const [note, setNote] = useState<string>();
  const host = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open || !host.current || !service) return;
    const term = new XTerm({ cursorBlink: true, fontFamily: 'monospace', fontSize: 13 });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    fit.fit();
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    const query = new URLSearchParams({ service, shell, cols: String(term.cols), rows: String(term.rows) });
    const socket = new WebSocket(`${scheme}://${location.host}/api/compose-projects/${projectId}/terminal?${query}`);
    socket.binaryType = 'arraybuffer';
    const encoder = new TextEncoder();
    socket.onmessage = (event) => {
      if (typeof event.data === 'string') {
        const message = JSON.parse(event.data) as { type: string; code?: number };
        if (message.type === 'exit') { setNote(`Shell exited (${message.code}).`); }
      } else {
        term.write(new Uint8Array(event.data as ArrayBuffer));
      }
    };
    socket.onerror = () => setNote('Could not open a terminal for this service (is its container running?).');
    socket.onclose = () => setOpen(false);
    const input = term.onData((data) => { if (socket.readyState === WebSocket.OPEN) socket.send(encoder.encode(data)); });
    const resize = () => {
      fit.fit();
      if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify({ type: 'resize', cols: term.cols, rows: term.rows }));
    };
    window.addEventListener('resize', resize);
    return () => {
      window.removeEventListener('resize', resize);
      input.dispose(); socket.close(); term.dispose();
    };
  }, [open, projectId, service, shell]);

  return <>
    <h3>Terminal</h3>
    <p className="hint">An interactive shell inside the service's container. Opening and closing a session is recorded in the audit log.</p>
    <div className="inline-form">
      <select aria-label="Service" value={service} onChange={(e) => setService(e.target.value)} disabled={open}>
        {services.map((s) => <option key={s}>{s}</option>)}
      </select>
      <select aria-label="Shell" value={shell} onChange={(e) => setShell(e.target.value as Shell)} disabled={open}>
        <option>sh</option><option>bash</option><option>ash</option>
      </select>
      {open
        ? <button className="ghost" onClick={() => setOpen(false)}>Close terminal</button>
        : <button className="action" disabled={!service} onClick={() => { setNote(undefined); setOpen(true); }}>Open terminal</button>}
    </div>
    {note && <p className="state">{note}</p>}
    {open && <div ref={host} className="terminal" />}
  </>;
}
