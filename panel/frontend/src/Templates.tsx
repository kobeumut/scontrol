import { FormEvent, useState } from 'react';
import { api, ComposeProject, Deployment, TemplateInfo, useApi } from './api';
import { ErrorLine, StateView, useAction } from './ui';

type Installed = { project: ComposeProject; deployment: Deployment | null; suggested_domain: TemplateInfo['suggested_domain'] };

const slugify = (value: string) => value.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 40);

/** Vetted application catalogue: one-click install with generated secrets. */
export function Templates({ onInstalled }: { onInstalled?: (projectId: string) => void }) {
  const catalogue = useApi<TemplateInfo[]>('/templates');
  const [chosen, setChosen] = useState<TemplateInfo>();
  const [name, setName] = useState('');
  const [slug, setSlug] = useState('');
  const [inputs, setInputs] = useState<Record<string, string>>({});
  const [done, setDone] = useState<Installed>();
  const action = useAction();

  const choose = (template: TemplateInfo) => {
    setChosen(template); setDone(undefined);
    setName(template.name); setSlug(slugify(template.id));
    setInputs(Object.fromEntries(template.inputs.map((i) => [i.name, i.default ?? ''])));
  };
  const install = (event: FormEvent) => {
    event.preventDefault();
    if (!chosen) return;
    action.run(async () => {
      const result = await api<Installed>(`/templates/${chosen.id}/install`, {
        method: 'POST', body: { name: name.trim(), slug, inputs },
      });
      setDone(result);
      onInstalled?.(result.project.id);
    });
  };

  return <section className="panel">
    <div className="panel-title"><h2>Template catalogue</h2></div>
    <p className="hint">Vetted applications that ship with the panel. Passwords and keys are generated at install and stored write-only in the project's environment.</p>
    <StateView state={catalogue} />
    <div className="cards">
      {catalogue.data?.map((t) => <article key={t.id} className={`card${chosen?.id === t.id ? ' selected' : ''}`}>
        <h3>{t.name}</h3>
        <p className="muted small">{t.category}</p>
        <p>{t.description}</p>
        <button className="action" onClick={() => choose(t)}>Install…</button>
      </article>)}
    </div>
    {chosen && <form className="card" onSubmit={install}>
      <h3>Install {chosen.name}</h3>
      <label>Project name<input required value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label>Slug<input required className="mono" pattern="[a-z0-9][a-z0-9-]{0,39}" value={slug} onChange={(e) => setSlug(e.target.value)} /></label>
      {chosen.inputs.map((input) => <label key={input.name}>{input.label}
        <input required value={inputs[input.name] ?? ''} onChange={(e) => setInputs({ ...inputs, [input.name]: e.target.value })} />
      </label>)}
      {chosen.generated.length > 0 && <p className="hint">Generated at install: <span className="mono">{chosen.generated.join(', ')}</span></p>}
      <button className="action" disabled={action.busy}>{action.busy ? 'Installing…' : 'Install and deploy'}</button>
      <ErrorLine error={action.error} />
    </form>}
    {done && <p className="state">
      Installed <strong>{done.project.name}</strong>; the first deployment is queued.
      {done.suggested_domain && <> Next, bind a domain to service <span className="mono">{done.suggested_domain.service}</span> port <span className="mono">{done.suggested_domain.port}</span> in the project's Domains section.</>}
    </p>}
  </section>;
}
