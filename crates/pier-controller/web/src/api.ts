export interface Session {
  username: string;
  expires_at: number;
  csrf_token: string;
}
export interface Variable {
  default: string | null;
}
export interface AppDefinition {
  name: string;
  version: string;
  source: 'git' | 'binary';
  variables: Record<string, Variable>;
}
export interface Blueprint {
  schema: number;
  name: string;
  variables: Record<string, Variable>;
  apps: { id: string; app: string; variables: Record<string, string> }[];
}
export interface Catalog {
  commit: string;
  apps: Record<string, AppDefinition>;
  blueprints: Record<string, Blueprint>;
}
export interface Agent {
  connection?: {
    mode: 'agent_to_controller' | 'controller_to_agent';
    proxy_configured: boolean;
    endpoint: string | null;
    state: 'connected' | 'reconnecting' | 'waiting';
    last_error: string | null;
  };
  id: string;
  name: string;
  online: boolean;
  info: { hostname: string; architecture: string; os_release: string } | null;
  last_seen: number | null;
  software: {
    version: string;
    package: PackageVersion | null;
    system: string | null;
    format: 'deb' | 'rpm' | null;
    supported: boolean;
    reason: string | null;
  } | null;
  upgrade: {
    target: AgentRelease | null;
    status: {
      release: AgentRelease;
      phase: 'waiting' | 'downloading' | 'installing' | 'restarting' | 'succeeded' | 'failed';
      error: string | null;
      updated_at: number;
    } | null;
    reason: string | null;
  };
  report: {
    capabilities?: string[];
    blueprints?: {
      id: string;
      blueprint: string;
      name: string;
      deployment_id: string | null;
      state: string;
      apps: Agent['report']['apps'];
      result: Agent['report']['result'];
    }[];
    deployment_id: string | null;
    apps: {
      instance: string;
      id: string;
      state: string;
      pid: number | null;
      restarts: number;
      exit_code: number | null;
    }[];
    result: { id: string; state: string; error: string | null } | null;
  };
}
export interface PackageVersion {
  version: string;
  revision: number;
}
export interface AgentRelease {
  package: PackageVersion;
  system: string;
  format: 'deb' | 'rpm';
  architecture: string;
  sha256: string;
  size: number;
}
export const packageVersion = (value?: PackageVersion | null) =>
  value ? `${value.version}-${value.revision}` : '—';
export const upgrading = (agent: Agent) =>
  ['installing', 'restarting'].includes(agent.upgrade?.status?.phase ?? '');
export const upgradeLabel = (agent: Agent) => {
  const phase = agent.upgrade?.status?.phase;
  if (phase)
    return {
      waiting: '等待部署结束',
      downloading: '下载中',
      installing: '安装中',
      restarting: '重启中',
      succeeded: '升级成功',
      failed: '升级失败，需手动恢复',
    }[phase];
  if (!agent.software) return '需手动升级一次';
  if (agent.upgrade?.reason || !agent.software.supported) return '自动升级不可用';
  return '已启用自动升级';
};
export interface Binding {
  id: string;
  agent_id: string;
  blueprint: string;
  variable_names: string[];
}
export interface Job {
  action: 'deploy' | 'stop';
  id: string;
  agent_id: string;
  blueprint: string;
  commit: string;
  state: string;
  error: string | null;
  created_at: number;
  plan: {
    architecture: string;
    apps: { id: string; instance: string; size: number; sha256: string }[];
  } | null;
}
export const terminal = new Set(['succeeded', 'failed', 'rolled_back', 'rollback_failed']);
let csrf = '';
export function setSession(value: Session | null) {
  csrf = value?.csrf_token ?? '';
}
export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}
export async function api<T>(
  path: string,
  method = 'GET',
  body?: unknown,
  authenticated = true,
  signal?: AbortSignal,
): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  if (method !== 'GET' && authenticated) headers['X-CSRF-Token'] = csrf;
  const response = await fetch(path, {
    method,
    headers,
    credentials: 'same-origin',
    cache: 'no-store',
    redirect: 'error',
    signal,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!response.ok) {
    if (response.status === 401 && authenticated)
      window.dispatchEvent(new Event('pier-session-expired'));
    let message = `请求失败（${response.status}）`;
    const text = await response.text();
    try {
      message = JSON.parse(text).error || message;
    } catch {
      if (text) message = text.slice(0, 400);
    }
    throw new ApiError(response.status, message);
  }
  return response.status === 204 ? (undefined as T) : (response.json() as Promise<T>);
}
export const date = (value: number | null) =>
  value ? new Date(value * 1000).toLocaleString('zh-CN') : '—';
