import { useCallback, useEffect, useState } from 'react';

export type BuildTool = 'nixpacks' | 'railpack' | 'buildpacks';

export type Capability = { docker: boolean; mail: boolean; antivirus: boolean; version: string; build_tools?: BuildTool[] };
export type User = { id: string; username: string; email: string; is_admin: boolean; totp_enabled: boolean };
export type Site = { id: string; domain: string; status: string; ssl_enabled: boolean; runtime: { kind: string } };

export type ApplicationBuild =
  | { type: 'image'; image: string }
  | { type: 'dockerfile'; repository_url: string; branch: string; context_path: string; dockerfile_path: string; target?: string | null }
  | { type: 'builder'; builder: BuildTool; repository_url: string; branch: string; context_path: string };

export const BUILD_TOOL_LABELS: Record<BuildTool, string> = {
  nixpacks: 'Nixpacks', railpack: 'Railpack', buildpacks: 'Buildpacks',
};

/** Static files served by the host Nginx; `build` runs in a throwaway container. */
export type StaticSiteSpec = {
  repository_url: string; branch: string; publish_path: string; spa: boolean;
  build?: { image: string; commands: string[][] } | null;
};

export type RestartPolicy = 'no' | 'always' | 'unless-stopped' | 'on-failure';

export type ApplicationSpec = {
  build: ApplicationBuild;
  service_name: string;
  container_port?: number | null;
  restart: RestartPolicy;
  memory_limit_mb?: number | null;
  cpu_limit_millis?: number | null;
  health_check?: { command: string[]; interval_secs: number; timeout_secs: number; retries: number; start_period_secs: number } | null;
  volumes: { name: string; target: string }[];
};

export type ComposeSource =
  | { kind: 'git'; repository_url: string; branch: string; compose_path: string }
  | { kind: 'raw'; compose_yaml: string }
  | ({ kind: 'application' } & ApplicationSpec)
  | ({ kind: 'static' } & StaticSiteSpec)
  | ({ kind: 'database' } & DatabaseSpec);

export type DatabaseEngine = 'postgres' | 'mysql' | 'mariadb' | 'mongo' | 'redis';

export const DATABASE_ENGINE_LABELS: Record<DatabaseEngine, string> = {
  postgres: 'PostgreSQL', mysql: 'MySQL', mariadb: 'MariaDB', mongo: 'MongoDB', redis: 'Redis',
};

export type DatabaseSpec = {
  engine: DatabaseEngine;
  version: string;
  database_name: string;
  username: string;
  memory_limit_mb?: number | null;
  cpu_limit_millis?: number | null;
};

/** Repository and branch for sources that deploy from Git. */
export function gitCheckout(source: ComposeSource): { repository_url: string; branch: string } | null {
  if (source.kind === 'git' || source.kind === 'static') return source;
  if (source.kind === 'application' && source.build.type !== 'image') return source.build;
  return null;
}

export function sourceLabel(source: ComposeSource): string {
  if (source.kind === 'git') return `git · ${source.branch}`;
  if (source.kind === 'raw') return 'raw';
  if (source.kind === 'static') return `static · ${source.branch}`;
  if (source.kind === 'database') return `database · ${DATABASE_ENGINE_LABELS[source.engine]} ${source.version}`;
  if (source.build.type === 'builder') return `app · ${BUILD_TOOL_LABELS[source.build.builder]} · ${source.build.branch}`;
  return source.build.type === 'image' ? 'app · image' : `app · Dockerfile · ${source.build.branch}`;
}

export type DeploymentStatus = 'queued' | 'running' | 'succeeded' | 'failed' | 'stopped' | 'cancelled';

export type ComposeProject = {
  id: string; name: string; slug: string; status: DeploymentStatus; source: ComposeSource;
  /** Environment (of a project) the service inherits variables from. */
  environment_id: string;
  /** False for a service that predates layering: nothing is inherited until its variables are saved. */
  env_layered: boolean;
  created_at: string; updated_at: string;
};

export type Environment = {
  id: string; project_id: string; name: string; slug: string; service_count: number;
  created_at: string; updated_at: string;
};

export type Project = {
  id: string; name: string; slug: string; owner_user_id: string; environments: Environment[];
  created_at: string; updated_at: string;
};

/** Least to most specific; a later layer overrides an earlier one. */
export type EnvLayer = 'project' | 'environment' | 'service';
export const ENV_LAYERS: EnvLayer[] = ['project', 'environment', 'service'];

/** Key names only: stored values are write-only and never returned. */
export type EnvKeyInfo = { key: string; source: EnvLayer; overrides: EnvLayer[] };
export type EnvLayerKeys = { layer: EnvLayer; keys: string[] };
export type LayerVariables = { layers: EnvLayerKeys[]; variables: EnvKeyInfo[]; layered?: boolean };
/** `value` omitted keeps the stored value of an existing key. */
export type VariableInput = { key: string; value?: string };

export type Deployment = {
  id: string; project_id: string; status: DeploymentStatus; trigger: string; commit_sha: string | null;
  queued_at: string; started_at: string | null; finished_at: string | null; error_message: string | null;
};

export type ServiceInfo = {
  name: string; container: string; image: string; state: string; status: string;
  health: string; exit_code: number; ports: string[];
};

export type Domain = {
  id: string; domain: string; service_name: string; container_port: number; host_port: number; https: boolean;
};

/** Non-secret Git access state; tokens are write-only and never returned. */
export type GitAccess = {
  transport: 'ssh' | 'https' | null; host: string | null;
  deploy_public_key: string | null; token_host: string | null;
  github_app: { id: string; name: string; app_id: number; installation_id: number } | null;
};

/** A registered GitHub App; its private key is write-only. */
export type GitHubApp = { id: string; name: string; app_id: number; created_at: string };

/** GitHub App tokens apply only to https://github.com/<owner>/<repo> remotes. */
export const isGitHubHttps = (url: string) => /^https:\/\/github\.com\/[^/\s]+\/[^/\s]+?(\.git)?\/?$/i.test(url.trim());

/** Mirrors the API's transport check closely enough to pick the right form. */
export const gitTransport = (url: string): 'ssh' | 'https' | null =>
  url.startsWith('https://') ? 'https' : url.startsWith('ssh://') || /^[^/@:\s]+@[^/:\s]+:/.test(url) ? 'ssh' : null;

export type LogChunk = { content: string; next_offset: number; eof: boolean; status: DeploymentStatus };

/** Cron (converted to systemd `OnCalendar=`) or a systemd calendar expression. */
export type JobSchedule = { kind: 'cron' | 'calendar'; expression: string };
export type JobRunStatus = 'running' | 'succeeded' | 'failed';

/** A command run by a systemd timer inside a service container; `command` is argv, never a shell string. */
export type ScheduledJob = {
  id: string; compose_project_id: string; name: string; service_name: string; command: string[];
  schedule: JobSchedule; timezone: string; on_calendar: string[]; timeout_secs: number; enabled: boolean;
  last_run_at: string | null; last_run_status: JobRunStatus | null; created_at: string; updated_at: string;
};

export type JobInput = Pick<ScheduledJob, 'name' | 'service_name' | 'command' | 'schedule' | 'timezone' | 'timeout_secs' | 'enabled'>;

export type JobRun = {
  id: string; job_id: string; trigger: 'schedule' | 'manual'; status: JobRunStatus;
  started_at: string; finished_at: string | null; exit_code: number | null; error_message: string | null;
  log_bytes: number; log_truncated: boolean;
};

export type JobLogChunk = { content: string; next_offset: number; eof: boolean; status: JobRunStatus };

export class ApiError extends Error {
  constructor(message: string, readonly status: number) { super(message); }
}

/** JSON request against the panel API; 204 resolves to `undefined`. */
export async function api<T>(path: string, init: { method?: string; body?: unknown } = {}): Promise<T> {
  const response = await fetch(`/api${path}`, {
    method: init.method ?? 'GET',
    credentials: 'same-origin',
    headers: init.body === undefined ? undefined : { 'content-type': 'application/json' },
    body: init.body === undefined ? undefined : JSON.stringify(init.body),
  });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new ApiError(body.error || `Request failed (${response.status})`, response.status);
  }
  if (response.status === 204) return undefined as T;
  return response.json() as Promise<T>;
}

export type ApiState<T> = { loading: boolean; data?: T; error?: string; reload: () => void };

/** Loads `path` (skipped when null) and reloads on demand or every `pollMs`. */
export function useApi<T>(path: string | null, pollMs?: number): ApiState<T> {
  const [state, setState] = useState<Omit<ApiState<T>, 'reload'>>({ loading: path !== null });
  const [tick, setTick] = useState(0);
  const reload = useCallback(() => setTick((t) => t + 1), []);

  useEffect(() => {
    if (path === null) return;
    let active = true;
    api<T>(path)
      .then((data) => active && setState({ loading: false, data }))
      .catch((error: Error) => active && setState((s) => ({ ...s, loading: false, error: error.message })));
    return () => { active = false; };
  }, [path, tick]);

  useEffect(() => {
    if (!pollMs || path === null) return;
    const timer = window.setInterval(reload, pollMs);
    return () => window.clearInterval(timer);
  }, [pollMs, path, reload]);

  return { ...state, reload };
}

export const isActive = (status: DeploymentStatus) => status === 'queued' || status === 'running';

export function formatTime(value: string | null): string {
  return value ? new Date(value).toLocaleString() : '—';
}

export function duration(start: string | null, end: string | null): string {
  if (!start) return '—';
  const seconds = Math.max(0, Math.round(((end ? new Date(end) : new Date()).getTime() - new Date(start).getTime()) / 1000));
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}
