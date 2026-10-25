import { infiniteQueryOptions, keepPreviousData, queryOptions } from '@tanstack/react-query'
import { useSyncExternalStore } from 'react'

export type Capabilities = {
  os: string
  arch: string
  abi: string
  tools: Record<string, string>
  gpu: string[]
  cpu_features?: string[]
  runtimes: string[]
  cpus: number
  memory_bytes: number
  containerized: boolean
  policy?: WorkerPolicy
}

/** A worker's command policy; an absent category is unrestricted. */
export type WorkerPolicy = {
  commands?: string[][]
  containers?: string[]
  environments?: { id: string; sha256: string }[]
  env?: string[]
  self_update?: boolean
  warnings?: string[]
}

export type WorkerInfo = {
  id: string
  capabilities: Capabilities
  max_jobs: number
  paused: boolean
  online: boolean
  last_seen: number
  version?: string
  updating?: string | null
  update_error?: string | null
}

export function workerUpdateProgress(worker: Pick<WorkerInfo, 'updating' | 'online'>, activeJobs: number) {
  if (!worker.updating) return null
  if (!worker.online) return {
    phase: 'reconnecting' as const,
    label: 'Awaiting reconnection',
    detail: `Waiting for this worker to reconnect and complete ${worker.updating}.`,
  }
  if (activeJobs > 0) return {
    phase: 'draining' as const,
    label: 'Waiting for jobs',
    detail: `Waiting for ${activeJobs} active ${activeJobs === 1 ? 'job' : 'jobs'} to finish. Then ${worker.updating} installs automatically.`,
  }
  return {
    phase: 'updating' as const,
    label: 'Updating',
    detail: `Updating to ${worker.updating}. The worker will restart and reconnect.`,
  }
}

export type FileEntry = { path: string; hash: string; size: number; mode: number }

export type JobSpec = {
  command: string[]
  priority?: number
  inputs?: FileEntry[]
  outputs?: string[]
  requirements?: Record<string, unknown>
  environment?: string | null
  container?: string | null
  env?: Record<string, string>
  timeout_secs?: number | null
}

export type Job = {
  id: string
  attempt_id?: string
  state: string
  spec: JobSpec
  worker_id?: string | null
  created_at: number
  updated_at: number
  exit_code?: number | null
  error?: string | null
  results?: FileEntry[]
  submitted_by?: string | null
}

export type JobSummary = Pick<Job, 'id' | 'state' | 'worker_id' | 'created_at' | 'updated_at' | 'submitted_by'> & {
  command: string
  environment?: string | null
}
export type JobSearch = { q?: string; state?: string; worker?: string }
export type JobStats = { states: Record<string, number>; workers: Record<string, { total: number; active: number }> }
export type JobList = { jobs: JobSummary[]; total: number; stats?: JobStats }

export type LogEntry = { job_id: string; attempt_id: string; seq: number; stream: string; text: string }

export type Environment = {
  id: string
  revision: string
  image?: string | null
}

export type EnvironmentStatus = {
  worker_id: string
  environment_id: string
  revision: string
  state: string
  error?: string | null
}

export type Role = 'admin' | 'requester' | 'worker'

export type Principal = { name: string; role: Role; token_id?: string; worker_id?: string }

export type TokenInfo = {
  id: string
  name: string
  role: Role
  worker_id?: string
  created_at: number
  created_by: string
  last_used_at?: number
  revoked_at?: number
}

export type AuditEvent = {
  seq: number
  at: number
  actor: string
  actor_token_id?: string
  role: Role
  action: string
  target: string
  detail?: string
}

export type Snapshot = {
  workers: WorkerInfo[]
  jobList: JobList
  environments: Environment[]
  environmentStatuses: EnvironmentStatus[]
}

const tokenKey = 'ylcc-dashboard-token'

// Persist across browser sessions on this device: the user explicitly prefers a saved
// login over re-pasting the pool token. Clearing on disconnect removes it.
export const readToken = () => sessionStorage.getItem(tokenKey) ?? localStorage.getItem(tokenKey) ?? ''
export const storeToken = (token: string) => {
  localStorage.setItem(tokenKey, token)
  sessionStorage.removeItem(tokenKey)
}
export const clearToken = () => {
  localStorage.removeItem(tokenKey)
  sessionStorage.removeItem(tokenKey)
}

export const storeSessionToken = (token: string) => {
  sessionStorage.setItem(tokenKey, token)
  localStorage.removeItem(tokenKey)
}

/** Hashes a user-chosen password in the browser so the raw password is not sent in transit. */
export async function hashPassword(value: string): Promise<string> {
  if (!(typeof crypto !== 'undefined' && crypto.subtle)) {
    throw new Error('Password login requires a secure browser context.')
  }
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(value))
  return Array.from(new Uint8Array(digest))
    .map((byte) => byte.toString(16).padStart(2, '0'))
    .join('')
}


/** Moves a `#token=...` handoff from `ylcc dashboard` into sessionStorage and strips it from the URL. */
export function consumeHashToken() {
  const hashToken = new URLSearchParams(window.location.hash.replace(/^#/, '')).get('token')
  if (!hashToken) return
  storeToken(hashToken)
  window.history.replaceState(null, '', window.location.pathname + window.location.search)
}

export class AuthError extends Error {
  constructor() {
    super('This token or password was rejected. Enter the Hub token or dashboard password.')
  }
}

/** A valid token whose role cannot read this endpoint; the session stays connected. */
export class ForbiddenError extends Error {}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers)
  headers.set('Authorization', `Bearer ${readToken()}`)
  const response = await fetch(path, {
    ...init,
    headers,
    cache: 'no-store',
  })
  if (!response.ok) {
    if (response.status === 401) throw new AuthError()
    if (response.status === 403) {
      const body = (await response.json().catch(() => null)) as { error?: string } | null
      throw new ForbiddenError(body?.error ?? 'This action requires an admin token.')
    }
    let message = await response.text()
    try {
      const body = JSON.parse(message) as { error?: unknown }
      if (typeof body.error === 'string') message = body.error
    } catch { /* Non-JSON responses retain their original diagnostic. */ }
    throw new Error(message || `Hub returned ${response.status}`)
  }
  if (response.status === 204) return undefined as unknown as T
  return response.json() as Promise<T>
}

export const dashboardPasswordStatus = () => get<{ set: boolean }>('/v1/dashboard-password')
export const jobPageSize = 50

function fetchJobList(search: JobSearch = {}, offset = 0) {
  const params = new URLSearchParams({ limit: String(jobPageSize), offset: String(offset) })
  for (const [key, value] of Object.entries(search)) if (value) params.set(key, value)
  return get<JobList>(`/v1/jobs/summary?${params}`)
}

export const jobsHistoryQuery = (search: JobSearch = {}) => infiniteQueryOptions({
  queryKey: ['jobs-history', search],
  queryFn: ({ pageParam }) => fetchJobList(search, pageParam),
  initialPageParam: 0,
  getNextPageParam: (lastPage, _pages, lastOffset) =>
    lastOffset + jobPageSize < lastPage.total && lastPage.jobs.length
      ? lastOffset + jobPageSize : undefined,
  refetchOnMount: false,
  refetchOnWindowFocus: false,
})

export const jobListQuery = (search: JobSearch) => queryOptions({
  queryKey: ['jobs-list', search],
  queryFn: () => fetchJobList(search),
  refetchInterval: pollMs,
})

export async function post<T = void>(path: string, body?: unknown): Promise<T> {
  return request<T>(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
}

export async function del<T = void>(path: string): Promise<T> {
  return request<T>(path, { method: 'DELETE' })
}

async function get<T>(path: string): Promise<T> {
  return request<T>(path)
}

export const pollMs = 5000

export const latestReleaseQuery = queryOptions({
  queryKey: ['latest-release'],
  queryFn: () => get<{ version: string }>('/v1/releases/latest'),
  staleTime: 300_000,
  refetchInterval: 300_000,
  retry: 1,
})

export const snapshotQuery = queryOptions({
  queryKey: ['snapshot'],
  queryFn: async (): Promise<Snapshot> => {
    const [workers, jobList, environments, environmentStatuses] = await Promise.all([
      get<WorkerInfo[]>('/v1/workers'),
      fetchJobList(),
      get<Environment[]>('/v1/environments'),
      get<EnvironmentStatus[]>('/v1/environments/status'),
    ])
    return { workers, jobList, environments, environmentStatuses }
  },
  refetchInterval: pollMs,
  placeholderData: keepPreviousData,
})

export const whoamiQuery = queryOptions({
  queryKey: ['whoami'],
  queryFn: () => get<Principal>('/v1/whoami'),
  staleTime: Infinity,
  retry: (count, error) => !(error instanceof AuthError) && count < 2,
})

export const auditQuery = queryOptions({
  queryKey: ['audit'],
  queryFn: () => get<AuditEvent[]>('/v1/audit?limit=500'),
  retry: (count, error) => !(error instanceof ForbiddenError || error instanceof AuthError) && count < 3,
  refetchInterval: (query) => (query.state.error instanceof ForbiddenError ? false : pollMs),
  placeholderData: keepPreviousData,
})

export const tokensQuery = queryOptions({
  queryKey: ['tokens'],
  queryFn: () => get<TokenInfo[]>('/v1/tokens'),
  retry: (count, error) => !(error instanceof ForbiddenError || error instanceof AuthError) && count < 3,
  refetchInterval: (query) => (query.state.error instanceof ForbiddenError ? false : pollMs),
  placeholderData: keepPreviousData,
})

export const jobQuery = (jobId: string) =>
  queryOptions({
    queryKey: ['job', jobId],
    queryFn: () => get<Job>(`/v1/jobs/${encodeURIComponent(jobId)}`),
    refetchInterval: pollMs,
  })

export const jobLogsQuery = (jobId: string, active: boolean) =>
  queryOptions({
    queryKey: ['job-logs', jobId],
    queryFn: () => get<LogEntry[]>(`/v1/jobs/${encodeURIComponent(jobId)}/logs`),
    refetchInterval: active ? pollMs : false,
    placeholderData: keepPreviousData,
  })

export type WorkerMetrics = {
  at: number
  cpu_percent: number
  memory_used: number
  memory_total: number
  load: number
  disk_percent: number
  disk_used?: number
  disk_total?: number
}

/** Averaged samples for the last `range` seconds; workers report every 15 s. */
export const workerMetricsQuery = (workerId: string, range: number) =>
  queryOptions({
    queryKey: ['worker-metrics', workerId, range],
    queryFn: () => get<WorkerMetrics[]>(`/v1/workers/${encodeURIComponent(workerId)}/metrics?range=${range}`),
    refetchInterval: 15000,
    placeholderData: keepPreviousData,
  })

export const errorMessage = (error: unknown) => (error instanceof Error ? error.message : 'Could not reach the Hub.')

const toMs = (timestamp: number) => (timestamp < 1_000_000_000_000 ? timestamp * 1000 : timestamp)

export function formatTime(timestamp: number): string {
  return new Intl.DateTimeFormat('en', {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  }).format(new Date(toMs(timestamp)))
}

const relative = new Intl.RelativeTimeFormat('en', { numeric: 'auto' })

export function formatRelative(timestamp: number, now: number): string {
  const seconds = Math.round((toMs(timestamp) - now) / 1000)
  if (Math.abs(seconds) < 10) return 'just now'
  for (const [unit, size] of [['day', 86400], ['hour', 3600], ['minute', 60]] as const) {
    if (Math.abs(seconds) >= size) return relative.format(Math.round(seconds / size), unit)
  }
  return relative.format(seconds, 'second')
}

export function formatDuration(fromTimestamp: number, toTimestamp: number): string {
  const seconds = Math.max(0, Math.round((toMs(toTimestamp) - toMs(fromTimestamp)) / 1000))
  if (seconds < 60) return `${seconds}s`
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`
}

export function formatMemory(bytes: number): string {
  if (!bytes) return '—'
  const gibibytes = bytes / 1024 ** 3
  return `${gibibytes < 10 ? gibibytes.toFixed(1) : Math.round(gibibytes)} GiB`
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  const units = ['KiB', 'MiB', 'GiB', 'TiB']
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`
}

// One shared clock so relative times stay fresh without a timer per row.
let now = Date.now()
const listeners = new Set<() => void>()
window.setInterval(() => {
  now = Date.now()
  listeners.forEach((listener) => listener())
}, pollMs)

export const useNow = () =>
  useSyncExternalStore(
    (listener) => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    () => now,
  )
