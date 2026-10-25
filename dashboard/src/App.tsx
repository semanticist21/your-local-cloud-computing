import { lazy, Suspense, useCallback, useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react'
import { useInfiniteQuery, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Link,
  Outlet,
  RouterProvider,
  createHashHistory,
  createRootRouteWithContext,
  createRoute,
  createRouter,
  useNavigate,
  useRouterState,
} from '@tanstack/react-router'
import {
  Activity,
  ArrowLeft,
  Boxes,
  Check,
  CircleAlert,
  Clock3,
  Cpu,
  FileText,
  KeyRound,
  LayoutDashboard,
  ScrollText,
  LockKeyhole,
  LogOut,
  Radio,
  RefreshCw,
  Server,
  ShieldCheck,
  Terminal,
  Workflow,
  X,
} from 'lucide-react'
import { Badge, Button } from './components/ui'

// Recharts loads only when a worker page is opened.
const WorkerMetricsCharts = lazy(() => import('./components/WorkerMetrics'))
import {
  AuthError,
  auditQuery,
  clearToken,
  del,
  storeSessionToken,
  dashboardPasswordStatus,
  hashPassword,
  post,
  pollMs,
  ForbiddenError,
  errorMessage,
  formatBytes,
  formatDuration,
  formatMemory,
  formatRelative,
  formatTime,
  jobLogsQuery,
  latestReleaseQuery,
  jobQuery,
  jobListQuery,
  jobsHistoryQuery,
  readToken,
  snapshotQuery,
  storeToken,
  tokensQuery,
  useNow,
  whoamiQuery,
  workerUpdateProgress,
  type EnvironmentStatus,
  type JobSummary,
  type JobSearch,
  type JobStats,
  type Role,
  type WorkerInfo,
  type WorkerPolicy,
} from './lib/api'

const activeJobStates = new Set(['assigned', 'running'])
const terminalJobStates = new Set(['succeeded', 'failed', 'cancelled'])
const jobStates = ['queued', 'assigned', 'running', 'cancelling', 'succeeded', 'failed', 'cancelled']

// ---------- Routes ----------

const optionalString = (value: unknown) => (typeof value === 'string' && value ? value : undefined)

const rootRoute = createRootRouteWithContext<{ disconnect: (message?: string) => void }>()({ component: Shell })
const overviewRoute = createRoute({ getParentRoute: () => rootRoute, path: '/', component: OverviewPage })
const workersRoute = createRoute({ getParentRoute: () => rootRoute, path: '/workers', component: WorkersPage })
const workerRoute = createRoute({ getParentRoute: () => rootRoute, path: '/workers/$workerId', component: WorkerPage })
const jobsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/jobs',
  component: JobsPage,
  validateSearch: (search: Record<string, unknown>): JobSearch => ({
    q: optionalString(search.q),
    state: optionalString(search.state),
    worker: optionalString(search.worker),
  }),
})
const jobRoute = createRoute({ getParentRoute: () => rootRoute, path: '/jobs/$jobId', component: JobPage })
const environmentsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/environments', component: EnvironmentsPage })
const auditRoute = createRoute({ getParentRoute: () => rootRoute, path: '/audit', component: AuditPage })
const tokensRoute = createRoute({ getParentRoute: () => rootRoute, path: '/tokens', component: TokensPage })

const routeTree = rootRoute.addChildren([overviewRoute, workersRoute, workerRoute, jobsRoute, jobRoute, environmentsRoute, auditRoute, tokensRoute])

// Hash history: deep links survive any static host and never depend on server-side route fallback.
const createAppRouter = () =>
  createRouter({
    routeTree,
    history: createHashHistory(),
    context: { disconnect: () => {} },
    defaultNotFoundComponent: () => (
      <EmptyState icon={<CircleAlert />} title="Page not found" detail="This dashboard address does not exist.">
        <Link className="text-action" to="/">Go to overview</Link>
      </EmptyState>
    ),
  })

declare module '@tanstack/react-router' {
  interface Register {
    router: ReturnType<typeof createAppRouter>
  }
}

// ---------- App + session ----------

function App() {
  const queryClient = useQueryClient()
  const [token, setToken] = useState(readToken)
  const [authError, setAuthError] = useState('')
  const [router] = useState(createAppRouter)

  const disconnect = useCallback((message = '') => {
    clearToken()
    queryClient.clear()
    setToken('')
    setAuthError(message)
  }, [queryClient])

  if (!token) {
    return (
      <ConnectScreen
        error={authError}
        onConnect={(next, persist) => {
          if (persist) storeToken(next)
          else storeSessionToken(next)
          setAuthError('')
          setToken(next)
        }}
      />
    )
  }
  return <RouterProvider router={router} context={{ disconnect }} />
}

const isInsecureRemote = () =>
  window.location.protocol !== 'https:' && !['localhost', '127.0.0.1', '[::1]'].includes(window.location.hostname)

function ConnectScreen({ error, onConnect }: { error: string; onConnect: (token: string, persist?: boolean) => void }) {
  const [token, setToken] = useState('')
  const [localError, setLocalError] = useState('')
  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const value = token.trim()
    if (!value) return
    setLocalError('')
    try {
      const probe = async (secret: string) => {
        const response = await fetch('/v1/whoami', { headers: { Authorization: `Bearer ${secret}` } })
        return response.ok
      }
      if (await probe(value)) {
        onConnect(value, true)
        return
      }
      let hashed: string
      try {
        hashed = await hashPassword(value)
      } catch (hashError) {
        setLocalError(hashError instanceof Error ? hashError.message : 'Could not hash password.')
        return
      }
      if (await probe(hashed)) {
        onConnect(hashed, false)
        return
      }
      setLocalError('That token or password was rejected.')
    } catch {
      setLocalError('Could not reach the Hub.')
    }
  }

  return (
    <main className="connect-screen">
      <section className="connect-panel">
        <div className="connect-brand"><span className="brand-mark"><Workflow size={19} /></span><span>YLCC</span></div>
        <div className="connect-icon"><LockKeyhole size={21} /></div>
        <h1>Connect to your Hub</h1>
        <p className="connect-description">Enter the pool token, a requester token issued with `ylcc tokens create`, or your dashboard password. Find a token file path with `ylcc dashboard`; it is never displayed by ylcc. Your dashboard reads status from this address.</p>
        <div className="endpoint-field"><span className="online-dot" />{window.location.origin}</div>
        <form onSubmit={submit} className="connect-form">
          <label htmlFor="pool-token">Hub token or password</label>
          <input
            id="pool-token"
            type="password"
            autoComplete="off"
            autoFocus
            value={token}
            onChange={(event) => setToken(event.target.value)}
            placeholder="Paste token or password"
            required
          />
          {error && <p className="form-error" role="alert">{error}</p>}
          {localError && <p className="form-error" role="alert">{localError}</p>}
          {isInsecureRemote() && <p className="form-warning"><LockKeyhole size={14} /> HTTP is not encrypted. Connect over a trusted private network or HTTPS.</p>}
          <Button className="connect-submit" type="submit" disabled={!token.trim()}>
            Connect to Hub <Check size={16} />
          </Button>
        </form>
        <div className="session-note"><ShieldCheck size={15} /> Token or password stays in this browser and is cleared when you disconnect. Copy it from your local token file; never paste it into chat or logs.</div>
      </section>
      <span className="connect-version">YLCC · self-hosted compute</span>
    </main>
  )
}

// ---------- Shell ----------

const sectionLabels: Record<string, string> = { '': 'Overview', workers: 'Workers', jobs: 'Jobs', environments: 'Environments', audit: 'Audit log', tokens: 'Tokens' }
const roleLabels: Record<Role, string> = { admin: 'Admin', requester: 'Requester', worker: 'Worker' }

function Shell() {
  const { disconnect } = rootRoute.useRouteContext()
  const whoami = useQuery(whoamiQuery)
  const principal = whoami.data
  const workerToken = principal?.role === 'worker'
  // Resolve the role first so a worker token gets an explanation instead of a generic error.
  const snapshot = useQuery({ ...snapshotQuery, enabled: !whoami.isPending && !workerToken })
  const now = useNow()
  const pathname = useRouterState({ select: (state) => state.location.pathname })
  const [section = '', detail] = pathname.split('/').filter(Boolean)

  useEffect(() => {
    if (snapshot.error instanceof AuthError) disconnect(snapshot.error.message)
    else if (whoami.error instanceof AuthError) disconnect(whoami.error.message)
  }, [snapshot.error, whoami.error, disconnect])
  if (workerToken) {
    return (
      <main className="connect-screen">
        <EmptyState icon={<KeyRound />} title="This is a worker token" detail={`It lets worker ${principal.worker_id ?? ''} connect to the Hub but cannot view the pool. Use a requester or admin token for the dashboard.`}>
          <Button variant="outline" size="sm" onClick={() => disconnect()}>Use another token</Button>
        </EmptyState>
      </main>
    )
  }

  const data = snapshot.data
  const failed = snapshot.isError && !(snapshot.error instanceof AuthError)
  const retry = () => void snapshot.refetch()

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <Link className="brand" to="/">
          <span className="brand-mark"><Workflow size={18} strokeWidth={2.3} /></span>
          <span className="brand-name">ylcc<span>compute pool</span></span>
        </Link>
        <div className="nav-label">Workspace</div>
        <nav className="nav-list" aria-label="Dashboard views">
          <NavLink to="/" icon={<LayoutDashboard />} label="Overview" exact />
          <NavLink to="/workers" icon={<Server />} label="Workers" count={data?.workers.length} />
          <NavLink to="/jobs" icon={<Activity />} label="Jobs" count={data?.jobList.total} />
          <NavLink to="/environments" icon={<Boxes />} label="Environments" count={data?.environments.length} />
          {principal?.role === 'admin' && (
            <>
              <NavLink to="/audit" icon={<ScrollText />} label="Audit log" />
              <NavLink to="/tokens" icon={<KeyRound />} label="Tokens" />
            </>
          )}
        </nav>
        <div className="sidebar-bottom">
          <div className="hub-card">
            <span className="hub-card-icon"><Radio size={15} /></span>
            <div className="hub-card-copy">
              <span>Hub endpoint</span>
              <strong title={window.location.host}>{window.location.host}</strong>
            </div>
            <span className={failed ? 'online-dot online-dot-error' : 'online-dot'} aria-label={failed ? 'Unreachable' : 'Connected'} />
          </div>
          <button className="account-button" onClick={() => disconnect()}>
            <span className="account-avatar">{(principal?.name ?? 'Y').slice(0, 1).toUpperCase()}</span>
            <span className="account-copy"><strong>{principal ? `${principal.name} · ${roleLabels[principal.role]}` : 'Hub session'}</strong><small>Disconnect and clear token</small></span>
            <LogOut size={15} aria-hidden="true" />
          </button>
        </div>
      </aside>

      <main className="main-panel">
        <header className="topbar">
          <nav className="breadcrumb" aria-label="Breadcrumb">
            <span>YLCC</span>
            <span className="breadcrumb-slash">/</span>
            {detail ? (
              <>
                <Link to={`/${section}` as '/'}>{sectionLabels[section] ?? section}</Link>
                <span className="breadcrumb-slash">/</span>
                <strong className="breadcrumb-detail">{decodeURIComponent(detail)}</strong>
              </>
            ) : <strong>{sectionLabels[section] ?? 'Not found'}</strong>}
          </nav>
          <div className="topbar-actions">
            <span className="refresh-status" role="status" title={`Auto-refreshes every 5 seconds`}>
              <span className={failed ? 'status-dot status-dot-error' : 'status-dot'} />
              <span className="refresh-label">
                {failed ? 'Hub unreachable' : snapshot.dataUpdatedAt ? `Updated ${formatRelative(snapshot.dataUpdatedAt, now)}` : 'Connecting'}
              </span>
            </span>
            <Button variant="outline" size="sm" onClick={retry} disabled={snapshot.isFetching} title="Refresh now" aria-label="Refresh now">
              <RefreshCw size={14} className={snapshot.isFetching ? 'spin' : ''} />
              <span>Refresh</span>
            </Button>
          </div>
        </header>

        <div className="page-content">
          {isInsecureRemote() && (
            <div className="security-notice" role="status">
              <LockKeyhole size={16} />
              <span>This page is using HTTP. Use a trusted private network or HTTPS before entering a pool token.</span>
            </div>
          )}
          {failed && data && (
            <div className="error-notice" role="alert">
              <CircleAlert size={16} />
              <span>Hub unreachable: {errorMessage(snapshot.error)}. Showing data from {formatRelative(snapshot.dataUpdatedAt, now)}.</span>
              <button className="text-action notice-action" onClick={retry}>Retry</button>
            </div>
          )}
          {data ? <Outlet /> : failed ? (
            <EmptyState icon={<CircleAlert />} title="Could not reach the Hub" detail={errorMessage(snapshot.error)}>
              <Button variant="outline" size="sm" onClick={retry} disabled={snapshot.isFetching}>Try again</Button>
            </EmptyState>
          ) : <LoadingState />}
          <footer className="page-footer">
            <span><ShieldCheck size={14} /> Compute pool dashboard</span>
            <span>Auto-refreshes every 5 seconds</span>
          </footer>
        </div>
      </main>
    </div>
  )
}

function NavLink({ to, icon, label, count, exact }: { to: '/' | '/workers' | '/jobs' | '/environments' | '/audit' | '/tokens'; icon: ReactNode; label: string; count?: number; exact?: boolean }) {
  return (
    <Link to={to} className="nav-button" activeProps={{ className: 'nav-button-active' }} activeOptions={{ exact, includeSearch: false }}>
      {icon}<span>{label}</span>{count !== undefined && <small>{count}</small>}
    </Link>
  )
}

/** Pages render only inside Shell once the first snapshot has arrived. */
function useSnapshot() {
  return useQuery(snapshotQuery).data!
}

function PageHeading({ title, description, back, children }: { title: ReactNode; description?: ReactNode; back?: ReactNode; children?: ReactNode }) {
  return (
    <div className="page-heading">
      <div className="page-heading-copy">
        {back}
        <h1>{title}</h1>
        {description && <p>{description}</p>}
      </div>
      {children}
    </div>
  )
}

// ---------- Overview ----------

function OverviewPage() {
  const { workers, jobList, environmentStatuses } = useSnapshot()
  const jobs = jobList.jobs
  const onlineWorkers = workers.filter((worker) => worker.online)
  const activeCount = (jobList.stats?.states.assigned ?? 0) + (jobList.stats?.states.running ?? 0)
  const queuedCount = jobList.stats?.states.queued ?? 0
  const failedCount = jobList.stats?.states.failed ?? 0
  const jobsByWorker = countActiveJobs(jobList.stats)
  const schedulable = onlineWorkers.filter((worker) => !worker.paused && !worker.updating)
  const totalCapacity = schedulable.reduce((sum, worker) => sum + worker.max_jobs, 0)
  const availableSlots = schedulable.reduce((sum, worker) => sum + Math.max(worker.max_jobs - (jobsByWorker.get(worker.id) ?? 0), 0), 0)
  const readyStatuses = environmentStatuses.filter((status) => status.state === 'ready').length
  const recentJobs = [...jobs].sort((a, b) => b.updated_at - a.updated_at).slice(0, 6)

  return (
    <>
      <PageHeading title="Overview" description="A live view of your self-hosted compute pool." />
      <section className="metrics-strip" aria-label="Pool summary">
        <Link className="metric-item metric-link" to="/workers">
          <Metric icon={<Server />} label="Online workers" value={`${onlineWorkers.length} / ${workers.length}`} note={workers.length ? `${workers.length - onlineWorkers.length} offline` : 'No workers registered'} tone="blue" />
        </Link>
        <Link className="metric-item metric-link" to="/jobs" search={{ state: 'active' }}>
          <Metric icon={<Activity />} label="Active jobs" value={String(activeCount)} note={queuedCount ? `${queuedCount} waiting in queue` : 'Queue is clear'} tone="orange" />
        </Link>
        <div className="metric-item">
          <Metric icon={<Cpu />} label="Available slots" value={String(availableSlots)} note={`${totalCapacity} ready to accept jobs`} tone="green" />
        </div>
        <Link className="metric-item metric-link" to="/environments">
          <Metric icon={<Boxes />} label="Ready environments" value={String(readyStatuses)} note={`${environmentStatuses.length} worker installs reported`} tone="slate" />
        </Link>
      </section>

      {failedCount > 0 && (
        <Link className="security-notice notice-link" to="/jobs" search={{ state: 'failed' }}>
          <CircleAlert size={16} />
          <span>{failedCount} failed {failedCount === 1 ? 'job' : 'jobs'} in history. Review failures</span>
        </Link>
      )}

      <section className="content-section">
        <SectionHeading title="Worker fleet" detail={`${onlineWorkers.length} online`} action={<Link className="text-action" to="/workers">All workers<span aria-hidden="true">→</span></Link>} />
        {workers.length ? <WorkersTable workers={workers.slice(0, 6)} jobsByWorker={jobsByWorker} /> : (
          <EmptyState icon={<Server />} title="No workers connected" detail="Start a worker with this Hub address and pool token to see it here." />
        )}
      </section>

      <section className="content-section">
        <SectionHeading title="Recent jobs" detail={`${jobList.total} total`} action={<Link className="text-action" to="/jobs">All jobs<span aria-hidden="true">→</span></Link>} />
        {recentJobs.length ? <JobsTable jobs={recentJobs} /> : (
          <EmptyState icon={<Terminal />} title="No jobs yet" detail="Jobs submitted to this pool will appear here." />
        )}
      </section>
    </>
  )
}

function countActiveJobs(stats: JobStats | undefined) {
  return new Map(Object.entries(stats?.workers ?? {}).map(([worker, counts]) => [worker, counts.active]))
}

// ---------- Workers ----------

function WorkersPage() {
  const { workers, jobList } = useSnapshot()
  const sorted = [...workers].sort((a, b) => Number(b.online) - Number(a.online) || a.id.localeCompare(b.id))
  return (
    <>
      <PageHeading title="Workers" description="Connected machines and their available capacity." />
      <section className="content-section">
        <SectionHeading title="Registered workers" detail={`${workers.filter((worker) => worker.online).length} online now`} />
        {workers.length ? <WorkersTable workers={sorted} jobsByWorker={countActiveJobs(jobList.stats)} /> : (
          <EmptyState icon={<Server />} title="No workers registered" detail="Workers appear after they connect to this Hub." />
        )}
      </section>
    </>
  )
}

function WorkerPage() {
  const { workerId } = workerRoute.useParams()
  const { workers, jobList, environmentStatuses } = useSnapshot()
  const history = useQuery(jobListQuery({ worker: workerId }))
  const worker = workers.find((candidate) => candidate.id === workerId)
  const back = <Link className="back-link" to="/workers"><ArrowLeft size={14} /> Workers</Link>
  if (!worker) {
    return (
      <>
        <PageHeading title="Worker not found" back={back} />
        <EmptyState icon={<Server />} title={`No worker named ${workerId}`} detail="It may have been removed from the Hub." />
      </>
    )
  }
  const caps = worker.capabilities
  const workerJobs = [...(history.data?.jobs ?? [])].sort((a, b) => b.updated_at - a.updated_at)
  const counts = jobList.stats?.workers[worker.id]
  const statuses = environmentStatuses.filter((status) => status.worker_id === worker.id)
  const tools = Object.entries(caps.tools).sort(([a], [b]) => a.localeCompare(b))

  return (
    <>
      <PageHeading title={worker.id} back={back} description={<WorkerState worker={worker} activeJobs={counts?.active ?? 0} />}><ResumeWorker worker={worker} /></PageHeading>
      <section className="content-section">
        <dl className="detail-grid">
          <Detail label="Platform">{caps.os} · <code>{caps.arch}</code>{caps.abi && <> · <code>{caps.abi}</code></>}</Detail>
          <Detail label="Resources">{caps.cpus} CPU · {formatMemory(caps.memory_bytes)}</Detail>
          <Detail label="Running jobs">{counts?.active ?? 0} / {worker.max_jobs}</Detail>
          <Detail label="Last seen"><RelativeTime timestamp={worker.last_seen} /></Detail>
          <Detail label="GPU">{caps.gpu.length ? caps.gpu.join(', ') : 'None'}</Detail>
          <Detail label="Runtimes">{caps.runtimes.length ? caps.runtimes.join(', ') : 'None'}</Detail>
          <Detail label="Containerized">{caps.containerized ? 'Yes' : 'No'}</Detail>
          <Detail label="CPU features">{caps.cpu_features?.length ? `${caps.cpu_features.length} reported` : 'None reported'}</Detail>
          <Detail label="Command policy">{caps.policy ? <PolicyBadge policy={caps.policy} /> : <span className="muted-text">None (runs any job)</span>}</Detail>
          <Detail label="YLCC version"><WorkerUpdate worker={worker} activeJobs={counts?.active ?? 0} /></Detail>
        </dl>
      </section>

      <Suspense fallback={<LoadingState compact />}>
        <WorkerMetricsCharts workerId={worker.id} os={caps.os} />
      </Suspense>

      {caps.policy && <PolicySection policy={caps.policy} />}

      <section className="content-section">
        <SectionHeading
          title="Jobs on this worker"
          detail={`${counts?.active ?? 0} active · ${counts?.total ?? 0} total`}
          action={workerJobs.length > 0 && <Link className="text-action" to="/jobs" search={{ worker: worker.id }}>View in jobs<span aria-hidden="true">→</span></Link>}
        />
        {history.isPending ? <LoadingState compact /> : history.isError && !history.data ? (
          <EmptyState icon={<CircleAlert />} title="Could not load jobs" detail={errorMessage(history.error)}>
            <Button variant="outline" onClick={() => void history.refetch()}>Try again</Button>
          </EmptyState>
        ) : workerJobs.length ? <JobsTable jobs={workerJobs.slice(0, 10)} /> : (
          <EmptyState icon={<Terminal />} title="No jobs yet" detail="Jobs assigned to this worker will appear here." />
        )}
      </section>

      <section className="content-section">
        <SectionHeading title="Environments" detail={`${statuses.length} reported`} />
        {statuses.length ? (
          <div className="table-scroll"><table className="data-table">
            <thead><tr><th>Environment</th><th>Revision</th><th>State</th><th>Error</th></tr></thead>
            <tbody>{statuses.map((status) => (
              <tr key={`${status.environment_id}-${status.revision}`}>
                <td><strong>{status.environment_id}</strong></td>
                <td><code>{status.revision}</code></td>
                <td><StatusBadge state={status.state} /></td>
                <td className="truncate-cell" title={status.error ?? undefined}>{status.error ?? <span className="muted-text">—</span>}</td>
              </tr>
            ))}</tbody>
          </table></div>
        ) : <EmptyState icon={<Boxes />} title="No environments reported" detail="This worker has not prepared any environments." />}
      </section>

      <section className="content-section">
        <SectionHeading title="Tools" detail={`${tools.length} detected`} />
        {tools.length ? (
          <div className="table-scroll"><table className="data-table">
            <thead><tr><th>Tool</th><th>Version</th></tr></thead>
            <tbody>{tools.map(([name, version]) => <tr key={name}><td><strong>{name}</strong></td><td><code>{version}</code></td></tr>)}</tbody>
          </table></div>
        ) : <EmptyState icon={<Cpu />} title="No tools detected" detail="The worker did not report any toolchains." />}
      </section>
    </>
  )
}

function WorkersTable({ workers, jobsByWorker }: { workers: WorkerInfo[]; jobsByWorker: Map<string, number> }) {
  const navigate = useNavigate()
  return (
    <div className="table-scroll"><table className="data-table workers-table">
      <thead><tr><th>Worker</th><th>State</th><th>Platform</th><th>Resources</th><th>Jobs</th><th>Version / update</th><th>Last seen</th></tr></thead>
      <tbody>{workers.map((worker) => (
        <tr key={worker.id} className="clickable-row" onClick={() => navigate({ to: '/workers/$workerId', params: { workerId: worker.id } })}>
          <td><div className="worker-name"><span className={`worker-avatar worker-avatar-${worker.capabilities.os}`}>{osGlyph(worker.capabilities.os)}</span><Link className="table-link" to="/workers/$workerId" params={{ workerId: worker.id }}>{worker.id}</Link></div></td>
          <td onClick={(event) => event.stopPropagation()}><WorkerState worker={worker} activeJobs={jobsByWorker.get(worker.id) ?? 0} />{worker.capabilities.policy && <> <PolicyBadge policy={worker.capabilities.policy} /></>}<ResumeWorker worker={worker} /></td>
          <td><span className="platform-name">{worker.capabilities.os}</span><span className="platform-arch">{worker.capabilities.arch}</span></td>
          <td><div className="resource-line"><Cpu size={13} /> {worker.capabilities.cpus} CPU <span className="resource-divider">·</span> {formatMemory(worker.capabilities.memory_bytes)}</div></td>
          <td><span className="capacity-value">{jobsByWorker.get(worker.id) ?? 0}<span> / {worker.max_jobs}</span></span></td>
          <td onClick={(event) => event.stopPropagation()}><WorkerUpdate worker={worker} activeJobs={jobsByWorker.get(worker.id) ?? 0} /></td>
          <td className="time-cell"><RelativeTime timestamp={worker.last_seen} /></td>
        </tr>
      ))}</tbody>
    </table></div>
  )
}

function ResumeWorker({ worker }: { worker: WorkerInfo }) {
  const queryClient = useQueryClient()
  const whoami = useQuery(whoamiQuery)
  const [submitting, setSubmitting] = useState(false)
  const [error, setError] = useState<unknown>(null)
  useDisconnectOnAuthError(error)
  if (!worker.paused || whoami.data?.role !== 'admin') return null
  const resume = async () => {
    setSubmitting(true)
    setError(null)
    try {
      await post(`/v1/workers/${encodeURIComponent(worker.id)}/control`, { paused: false })
      await queryClient.invalidateQueries({ queryKey: ['snapshot'] })
    } catch (failure) { setError(failure) }
    finally { setSubmitting(false) }
  }
  return (
    <div className="worker-resume">
      <Button size="sm" variant="outline" disabled={submitting} onClick={() => void resume()}>{submitting ? 'Resuming…' : 'Resume'}</Button>
      {error != null && <span className="form-error" role="alert">{errorMessage(error)}</span>}
    </div>
  )
}

function WorkerUpdate({ worker, activeJobs }: { worker: WorkerInfo; activeJobs: number }) {
  const queryClient = useQueryClient()
  const whoami = useQuery(whoamiQuery)
  const admin = whoami.data?.role === 'admin'
  const release = useQuery({ ...latestReleaseQuery, enabled: admin })
  const [submitting, setSubmitting] = useState(false)
  const [error, setError] = useState<unknown>(null)
  useDisconnectOnAuthError(error)
  const target = release.data?.version
  const current = worker.version?.split('+')[0]
  const progress = workerUpdateProgress(worker, activeJobs)
  const available = target && (!current || current.localeCompare(target.slice(1), undefined, { numeric: true }) < 0)
  const reason = !available ? '' : worker.capabilities.containerized ? 'This worker runs from a container image, so the dashboard cannot replace its binary. After jobs finish, pull the new release image and recreate the container with its existing data volume.'
    : worker.capabilities.policy?.self_update === false ? 'Self-update is disabled by worker policy.'
    : !worker.online ? 'Connect this worker to update it.'
    : !worker.version ? 'Upgrade this older worker locally first.' : ''
  const update = async () => {
    if (!target || submitting) return
    setSubmitting(true)
    setError(null)
    try {
      await post(`/v1/workers/${encodeURIComponent(worker.id)}/update`, { version: target })
      await queryClient.invalidateQueries({ queryKey: ['snapshot'] })
    } catch (failure) { setError(failure) }
    finally { setSubmitting(false) }
  }
  return (
    <div className="worker-update">
      <code>{worker.version || 'Unknown'}</code>
      {progress ? (
        <span className="worker-update-note" role="status">
          {progress.phase === 'updating' ? <RefreshCw size={12} className="spin" /> : <Clock3 size={12} />}
          <span>{progress.detail}{progress.phase === 'draining' && <> <Link className="text-action" to="/jobs" search={{ worker: worker.id }}>View jobs</Link></>}</span>
        </span>
      ) : admin && (
        <>
          {available && <Button size="sm" variant="outline" disabled={submitting || Boolean(reason)} title={reason || 'Running jobs finish before the worker restarts. You can close this page.'} onClick={() => void update()}>{submitting ? 'Starting…' : `Update to ${target}`}</Button>}
          {reason && <span className="worker-update-note">{reason}</span>}
          {!available && target && <span className="worker-update-note">Up to date</span>}
          {release.isError && <span className="form-error" role="alert">{errorMessage(release.error)} <button className="text-action" onClick={() => void release.refetch()}>Retry release check</button></span>}
          {release.isPending && <span className="worker-update-note">Checking release…</span>}
        </>
      )}
      {(error || worker.update_error) && <span className="form-error" role="alert">{error ? errorMessage(error) : worker.update_error}</span>}
    </div>
  )
}

function PolicyBadge({ policy }: { policy: WorkerPolicy }) {
  return policy.warnings?.length
    ? <Badge variant="warning" title={policy.warnings.join('\n')}>Restricted (bypassable)</Badge>
    : <Badge variant="default">Restricted</Badge>
}

function PolicySection({ policy }: { policy: WorkerPolicy }) {
  const rule = (items: string[] | undefined, render: (items: string[]) => ReactNode) =>
    items === undefined ? <span className="muted-text">Unrestricted</span> : items.length === 0 ? <span className="muted-text">None allowed</span> : render(items)
  return (
    <section className="content-section">
      <SectionHeading title="Command policy" detail="Enforced by this worker before it runs anything" />
      {policy.warnings?.map((warning) => (
        <div key={warning} className="error-notice" role="note"><CircleAlert size={16} /><span>{warning}</span></div>
      ))}
      <dl className="detail-grid">
        <Detail label="Commands">{rule(policy.commands?.map((argv) => JSON.stringify(argv)), (items) => <code>{items.join('  ')}</code>)}</Detail>
        <Detail label="Containers">{rule(policy.containers, (items) => <code>{items.join(', ')}</code>)}</Detail>
        <Detail label="Environments">{rule(policy.environments?.map((pin) => `${pin.id}${pin.sha256 === '*' ? ' (any content)' : ''}`), (items) => items.join(', '))}</Detail>
        <Detail label="Job env keys">{rule(policy.env, (items) => <code>{items.join(', ')}</code>)}</Detail>
        <Detail label="Self-update">{policy.self_update === false ? 'Disabled' : 'Allowed'}</Detail>
      </dl>
    </section>
  )
}

// ---------- Jobs ----------

function JobsPage() {
  const snapshot = useSnapshot()
  const workers = snapshot.workers
  const loadMoreRef = useRef<HTMLDivElement>(null)
  const search = jobsRoute.useSearch()
  const filtering = Boolean(search.q || search.state || search.worker)
  const moreQuery = useInfiniteQuery({
    ...jobsHistoryQuery(search),
    refetchInterval: filtering ? pollMs : false,
    initialData: filtering ? undefined : () => ({ pages: [snapshot.jobList], pageParams: [0] }),
  })
  const queryClient = useQueryClient()
  useEffect(() => {
    if (filtering) return
    queryClient.setQueryData(jobsHistoryQuery().queryKey, (previous) => previous && ({
      ...previous,
      pages: [{ ...snapshot.jobList, jobs: [...new Map([
        ...previous.pages[0].jobs, ...snapshot.jobList.jobs,
      ].map((job) => [job.id, job])).values()] }, ...previous.pages.slice(1)],
    }))
  }, [snapshot.jobList, filtering, queryClient])
  const hasMore = moreQuery.hasNextPage
  const { fetchNextPage, isFetching, isError, dataUpdatedAt } = moreQuery
  const jobs = [...new Map([
    ...(moreQuery.data?.pages.flatMap((page) => page.jobs) ?? []),
    ...(!filtering ? snapshot.jobList.jobs : []),
  ].map((job) => [job.id, job])).values()]
  const total = filtering ? (moreQuery.data?.pages[0]?.total ?? 0) : snapshot.jobList.total
  const navigate = useNavigate({ from: '/jobs' })
  const setSearch = useCallback(
    (patch: JobSearch) => void navigate({ search: (previous) => ({ ...previous, ...patch }), replace: true }),
    [navigate],
  )
  const [query, setQuery] = useState(search.q ?? '')
  // Back/forward changes the URL's q; adopt it unless it is just our own debounced write.
  const [syncedQ, setSyncedQ] = useState(search.q)
  if (search.q !== syncedQ) {
    setSyncedQ(search.q)
    if ((query.trim() || undefined) !== search.q) setQuery(search.q ?? '')
  }
  useEffect(() => {
    const next = query.trim() || undefined
    if (next === search.q) return
    const timer = window.setTimeout(() => setSearch({ q: next }), 200)
    return () => window.clearTimeout(timer)
  }, [query, search.q, setSearch])

  const filtered = jobs.sort((a, b) => b.created_at - a.created_at)
  const workerIds = [...new Set([...workers.map((worker) => worker.id), ...(search.worker ? [search.worker] : [])])].sort()

  useEffect(() => {
    const target = loadMoreRef.current
    if (!target || !hasMore || isFetching || isError) return
    const observer = new IntersectionObserver(([entry]) => {
      if (entry.isIntersecting) void fetchNextPage({ cancelRefetch: false })
    }, { rootMargin: '240px' })
    observer.observe(target)
    return () => observer.disconnect()
  }, [hasMore, isFetching, isError, fetchNextPage, dataUpdatedAt, filtered.length])

  return (
    <>
      <PageHeading title="Jobs" description="Recent execution state across the pool." />
      <section className="content-section">
        <div className="filter-row" role="search">
          <input
            className="field field-search"
            type="search"
            aria-label="Search jobs"
            placeholder="Search job, command, worker, environment, submitter"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
          />
          <select className="field" aria-label="Filter by state" value={search.state ?? ''} onChange={(event) => setSearch({ state: event.target.value || undefined })}>
            <option value="">All states</option>
            <option value="active">Active (assigned + running)</option>
            {jobStates.map((state) => <option key={state} value={state}>{state}</option>)}
          </select>
          <select className="field" aria-label="Filter by worker" value={search.worker ?? ''} onChange={(event) => setSearch({ worker: event.target.value || undefined })}>
            <option value="">All workers</option>
            {workerIds.map((id) => <option key={id} value={id}>{id}</option>)}
          </select>
          {filtering && (
            <Button variant="ghost" size="sm" onClick={() => { setQuery(''); setSearch({ q: undefined, state: undefined, worker: undefined }) }}>
              <X size={14} /> Clear
            </Button>
          )}
        </div>
        <SectionHeading
          title="Job history"
          detail={`${jobs.length} of ${total} jobs`}
        />
        {moreQuery.isPending ? <LoadingState compact /> : filtered.length ? <JobsTable jobs={filtered} /> : filtering ? (
          <EmptyState icon={<Terminal />} title="No matching jobs" detail="Try a different search or clear the filters." />
        ) : (
          <EmptyState icon={<Terminal />} title="No jobs yet" detail="Submitted jobs and their latest states will appear here." />
        )}
        {(hasMore || isError) && <div ref={loadMoreRef} className="jobs-pagination">
          {isFetching ? <LoadingState compact label="Loading more jobs…" /> : <>
            {isError && <span role="alert">Could not load more jobs: {errorMessage(moreQuery.error)}</span>}
            <Button variant="outline" onClick={() => void fetchNextPage({ cancelRefetch: false })}>
              {isError ? 'Try again' : 'Load more jobs'}
            </Button>
          </>}
        </div>}
      </section>
    </>
  )
}

function JobPage() {
  const { jobId } = jobRoute.useParams()
  const details = useQuery(jobQuery(jobId))
  const job = details.data
  const finished = Boolean(job && terminalJobStates.has(job.state))
  const logs = useQuery({ ...jobLogsQuery(jobId, !finished), enabled: Boolean(job) })
  const queryClient = useQueryClient()
  // Polling stops at the terminal state; fetch once more so output written after the last poll appears.
  useEffect(() => {
    if (finished) void queryClient.invalidateQueries({ queryKey: jobLogsQuery(jobId, false).queryKey })
  }, [finished, jobId, queryClient])
  const now = useNow()
  const back = <Link className="back-link" to="/jobs"><ArrowLeft size={14} /> Jobs</Link>
  if (!job && details.isPending) return <><PageHeading title="Job details" back={back} /><LoadingState compact /></>
  if (!job) {
    return (
      <>
        <PageHeading title="Could not load job" back={back} />
        <EmptyState icon={<Terminal />} title={jobId} detail={errorMessage(details.error)}>
          <Button variant="outline" onClick={() => void details.refetch()}>Try again</Button>
        </EmptyState>
      </>
    )
  }
  const spec = job.spec
  const envKeys = Object.keys(spec.env ?? {}).sort()
  const requirements = Object.entries(spec.requirements ?? {}).filter(([, value]) =>
    value !== null && value !== false && !(Array.isArray(value) && !value.length) && !(typeof value === 'object' && !Array.isArray(value) && !Object.keys(value as object).length))
  const inputBytes = (spec.inputs ?? []).reduce((sum, file) => sum + file.size, 0)
  const results = job.results ?? []

  return (
    <>
      <PageHeading title={<code className="heading-code">{job.id}</code>} back={back} description={<StatusBadge state={job.state} />} />
      {job.error && (
        <div className="error-notice" role="alert"><CircleAlert size={16} /><span>{job.error}</span></div>
      )}
      <section className="content-section">
        <dl className="detail-grid">
          <Detail label="Worker">{job.worker_id ? <Link className="table-link" to="/workers/$workerId" params={{ workerId: job.worker_id }}>{job.worker_id}</Link> : <span className="muted-text">Unassigned</span>}</Detail>
          <Detail label="Environment">{spec.environment ?? <span className="muted-text">Native</span>}</Detail>
          <Detail label="Created"><RelativeTime timestamp={job.created_at} /></Detail>
          <Detail label={finished ? 'Duration' : 'Elapsed'}>{formatDuration(job.created_at, finished ? job.updated_at : now)}</Detail>
          <Detail label="Exit code">{job.exit_code ?? <span className="muted-text">—</span>}</Detail>
          <Detail label="Priority">{spec.priority ?? 0}</Detail>
          <Detail label="Timeout">{spec.timeout_secs ? `${spec.timeout_secs}s` : <span className="muted-text">None</span>}</Detail>
          <Detail label="Container">{spec.container ?? <span className="muted-text">None</span>}</Detail>
          <Detail label="Submitted by">{job.submitted_by ?? <span className="muted-text">Not recorded</span>}</Detail>
        </dl>
      </section>

      <section className="content-section">
        <SectionHeading title="Command" detail={`${spec.command.length} arguments`} />
        <pre className="code-block">{spec.command.join(' ')}</pre>
      </section>

      <section className="content-section">
        <SectionHeading
          title="Logs"
          detail={logs.data ? `${logs.data.length} entries${finished ? '' : ' · live'}` : 'Loading'}
          action={logs.isFetching && <RefreshCw size={13} className="spin muted-text" aria-label="Updating logs" />}
        />
        {logs.isError && <div className="error-notice" role="alert"><CircleAlert size={16} /><span>Could not load logs: {errorMessage(logs.error)}</span></div>}
        {logs.data ? (
          logs.data.length ? (
            <pre className="log-view" tabIndex={0} aria-label="Job output">
              {logs.data.map((entry) => <span key={`${entry.attempt_id}-${entry.seq}`} className={entry.stream === 'stderr' ? 'log-stderr' : undefined}>{entry.text}</span>)}
            </pre>
          ) : <EmptyState icon={<FileText />} title={finished ? 'No output recorded' : 'No output yet'} detail={finished ? 'This job finished without writing to stdout or stderr.' : 'Output appears here as the worker streams it.'} />
        ) : !logs.isError && <LoadingState compact />}
      </section>

      <section className="content-section">
        <SectionHeading title="Results" detail={`${results.length} files`} />
        {results.length ? <FilesTable files={results} /> : (
          <EmptyState icon={<FileText />} title="No result files" detail={spec.outputs?.length ? `Expected outputs: ${spec.outputs.join(', ')}` : 'This job did not declare any outputs.'} />
        )}
      </section>

      <section className="content-section">
        <SectionHeading title="Spec" detail="As submitted" />
        <dl className="detail-grid">
          <Detail label="Inputs">{spec.inputs?.length ? `${spec.inputs.length} files · ${formatBytes(inputBytes)}` : 'None'}</Detail>
          <Detail label="Outputs">{spec.outputs?.length ? spec.outputs.join(', ') : 'None'}</Detail>
          <Detail label="Environment variables">{envKeys.length ? <>{envKeys.join(', ')} <span className="muted-text">(values hidden)</span></> : 'None'}</Detail>
          <Detail label="Attempt"><code>{job.attempt_id ?? '—'}</code></Detail>
        </dl>
        {requirements.length > 0 && <pre className="code-block">{JSON.stringify(Object.fromEntries(requirements), null, 2)}</pre>}
      </section>
    </>
  )
}

function JobsTable({ jobs }: { jobs: JobSummary[] }) {
  const navigate = useNavigate()
  const now = useNow()
  return (
    <div className="table-scroll"><table className="data-table jobs-table">
      <thead><tr><th>Job</th><th>State</th><th>Command</th><th>Worker</th><th>Environment</th><th>Submitted by</th><th>Updated</th><th>Elapsed</th></tr></thead>
      <tbody>{jobs.map((job) => (
        <tr key={job.id} className="clickable-row" onClick={() => navigate({ to: '/jobs/$jobId', params: { jobId: job.id } })}>
          <td><Link className="table-link" to="/jobs/$jobId" params={{ jobId: job.id }}><code className="job-id">{job.id.slice(0, 8)}</code></Link></td>
          <td><StatusBadge state={job.state} /></td>
          <td><code className="command-cell" title={job.command}>{job.command || '—'}</code></td>
          <td>{job.worker_id
            ? <Link className="table-link" to="/workers/$workerId" params={{ workerId: job.worker_id }} onClick={(event) => event.stopPropagation()}>{job.worker_id}</Link>
            : <span className="muted-text">Unassigned</span>}</td>
          <td>{job.environment ?? <span className="muted-text">Native</span>}</td>
          <td>{job.submitted_by ?? <span className="muted-text">—</span>}</td>
          <td className="time-cell"><RelativeTime timestamp={job.updated_at} /></td>
          <td className="time-cell">{formatDuration(job.created_at, activeJobStates.has(job.state) ? now : job.updated_at)}</td>
        </tr>
      ))}</tbody>
    </table></div>
  )
}

function FilesTable({ files }: { files: { path: string; hash: string; size: number }[] }) {
  return (
    <div className="table-scroll"><table className="data-table">
      <thead><tr><th>Path</th><th>Size</th><th>Hash</th></tr></thead>
      <tbody>{files.map((file) => (
        <tr key={file.path}><td><code>{file.path}</code></td><td>{formatBytes(file.size)}</td><td><code title={file.hash}>{file.hash.slice(0, 12)}</code></td></tr>
      ))}</tbody>
    </table></div>
  )
}

// ---------- Environments ----------

function EnvironmentsPage() {
  const { environments, environmentStatuses } = useSnapshot()
  return (
    <>
      <PageHeading title="Environments" description="Configured environments and worker readiness." />
      <section className="content-section">
        <SectionHeading title="Environment catalogue" detail={`${environments.length} definitions`} />
        {environments.length ? (
          <div className="table-scroll"><table className="data-table">
            <thead><tr><th>Environment</th><th>Revision</th><th>Image</th><th>Worker installations</th></tr></thead>
            <tbody>{environments.map((environment) => {
              const installed = environmentStatuses.filter((status) => status.environment_id === environment.id && status.revision === environment.revision)
              return <tr key={`${environment.id}-${environment.revision}`}>
                <td><strong>{environment.id}</strong></td>
                <td><code>{environment.revision}</code></td>
                <td className="truncate-cell">{environment.image ?? 'Native'}</td>
                <td><div className="install-list">{installed.length ? installed.map((status) => <InstallItem key={status.worker_id} status={status} />) : <span className="muted-text">No worker status</span>}</div></td>
              </tr>
            })}</tbody>
          </table></div>
        ) : <EmptyState icon={<Boxes />} title="No environments configured" detail="Environment definitions will appear here when added to the Hub." />}
      </section>
    </>
  )
}

function InstallItem({ status }: { status: EnvironmentStatus }) {
  return (
    <span className="install-item" title={status.error ?? undefined}>
      <StatusBadge state={status.state} />
      <Link className="table-link" to="/workers/$workerId" params={{ workerId: status.worker_id }}>{status.worker_id}</Link>
    </span>
  )
}

// ---------- Access ----------

function AdminOnly({ error }: { error: unknown }) {
  return <EmptyState icon={<LockKeyhole />} title="Requires an admin token" detail={error instanceof ForbiddenError ? error.message : errorMessage(error)} />
}

/** A revoked token on an admin page ends the session like any other 401. */
function useDisconnectOnAuthError(error: unknown) {
  const { disconnect } = rootRoute.useRouteContext()
  useEffect(() => {
    if (error instanceof AuthError) disconnect(error.message)
  }, [error, disconnect])
}

function AuditPage() {
  const audit = useQuery(auditQuery)
  useDisconnectOnAuthError(audit.error)
  const events = audit.data ?? []
  return (
    <>
      <PageHeading title="Audit log" description="Who changed what in the pool, newest first. Reads are not recorded." />
      <section className="content-section">
        <SectionHeading title="Recent events" detail={audit.data ? `${events.length} shown` : audit.isError ? 'Unavailable' : 'Loading'} />
        {audit.isError ? <AdminOnly error={audit.error} /> : !audit.data ? <LoadingState compact /> : events.length ? (
          <div className="table-scroll"><table className="data-table">
            <thead><tr><th>When</th><th>Actor</th><th>Action</th><th>Target</th><th>Detail</th></tr></thead>
            <tbody>{events.map((event) => (
              <tr key={event.seq}>
                <td className="time-cell"><RelativeTime timestamp={event.at} /></td>
                <td><strong>{event.actor}</strong> <Badge variant={event.role === 'admin' ? 'warning' : 'muted'}>{roleLabels[event.role]}</Badge></td>
                <td><code>{event.action}</code></td>
                <td><AuditTarget action={event.action} target={event.target} /></td>
                <td className="truncate-cell" title={event.detail}>{event.detail ?? <span className="muted-text">—</span>}</td>
              </tr>
            ))}</tbody>
          </table></div>
        ) : <EmptyState icon={<ScrollText />} title="No events yet" detail="Token, job, worker and environment changes will appear here." />}
      </section>
    </>
  )
}

function AuditTarget({ action, target }: { action: string; target: string }) {
  if (action.startsWith('job.')) return <Link className="table-link" to="/jobs/$jobId" params={{ jobId: target }}><code>{target.slice(0, 8)}</code></Link>
  if (action.startsWith('worker.')) return <Link className="table-link" to="/workers/$workerId" params={{ workerId: target }}>{target}</Link>
  return <code>{target}</code>
}

function DashboardPasswordSection() {
  const queryClient = useQueryClient()
  const statusQuery = useQuery({
    queryKey: ['dashboard-password'],
    queryFn: dashboardPasswordStatus,
    retry: (count, error) => !(error instanceof ForbiddenError || error instanceof AuthError) && count < 2,
  })
  const configured = statusQuery.data?.set === true
  const [password, setPassword] = useState('')
  const [status, setStatus] = useState('')
  const [saving, setSaving] = useState(false)
  const [removing, setRemoving] = useState(false)
  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const value = password.trim()
    if (!value) return
    setSaving(true)
    setStatus('')
    try {
      await post('/v1/dashboard-password', { password: await hashPassword(value) })
      await queryClient.invalidateQueries({ queryKey: ['dashboard-password'] })
      setStatus('Password set. You can now log in with it on this or another browser.')
      setPassword('')
    } catch (error) {
      setStatus(error instanceof Error ? error.message : 'Could not set password.')
    } finally {
      setSaving(false)
    }
  }
  const remove = async () => {
    setRemoving(true)
    setStatus('')
    try {
      await del('/v1/dashboard-password')
      await queryClient.invalidateQueries({ queryKey: ['dashboard-password'] })
      setStatus('Password removed.')
    } catch (error) {
      setStatus(error instanceof Error ? error.message : 'Could not remove password.')
    } finally {
      setRemoving(false)
    }
  }
  return (
    <section className="content-section">
      <SectionHeading title="Dashboard password" detail={configured ? 'A dashboard password is currently configured.' : 'Optional. Lets you log in with a password instead of pasting a token.'} />
      {configured && <Button variant="outline" size="sm" onClick={() => void remove()} disabled={removing}>Remove password</Button>}
      <form onSubmit={submit} className="connect-form">
        <label htmlFor="dashboard-password">{configured ? 'Replace password' : 'New password'}</label>
        <input
          id="dashboard-password"
          type="password"
          autoComplete="new-password"
          value={password}
          onChange={(event) => setPassword(event.target.value)}
          placeholder="At least 12 characters"
          required
          minLength={12}
        />
        {status && <p className={status.startsWith('Password') || status.startsWith('Password removed') ? 'session-note' : 'form-error'} role="status">{status}</p>}
        <Button className="connect-submit" type="submit" disabled={saving || password.trim().length < 12}>
          Set password
        </Button>
      </form>
    </section>
  )
}


function TokensPage() {
  const tokens = useQuery(tokensQuery)
  const whoami = useQuery(whoamiQuery)
  useDisconnectOnAuthError(tokens.error)
  const list = [...(tokens.data ?? [])].sort((a, b) => Number(Boolean(a.revoked_at)) - Number(Boolean(b.revoked_at)) || b.created_at - a.created_at)
  return (
    <>
      <PageHeading title="Tokens" description="Per-machine tokens. Create and revoke them with `ylcc tokens`; this page is read-only." />
      {whoami.data?.role === 'admin' && <DashboardPasswordSection />}
      <section className="content-section">
        <SectionHeading title="Issued tokens" detail={tokens.data ? `${list.filter((token) => !token.revoked_at).length} active` : tokens.isError ? 'Unavailable' : 'Loading'} />
        {tokens.isError ? <AdminOnly error={tokens.error} /> : !tokens.data ? <LoadingState compact /> : list.length ? (
          <div className="table-scroll"><table className="data-table">
            <thead><tr><th>Name</th><th>Role</th><th>Worker</th><th>Created</th><th>Last used</th><th>State</th></tr></thead>
            <tbody>{list.map((token) => (
              <tr key={token.id}>
                <td><strong>{token.name}</strong> <code className="muted-text">{token.id}</code></td>
                <td>{roleLabels[token.role]}</td>
                <td>{token.worker_id ?? <span className="muted-text">—</span>}</td>
                <td className="time-cell"><RelativeTime timestamp={token.created_at} /> <span className="muted-text">by {token.created_by}</span></td>
                <td className="time-cell">{token.last_used_at ? <RelativeTime timestamp={token.last_used_at} /> : <span className="muted-text">Never</span>}</td>
                <td>{token.revoked_at ? <Badge variant="muted">Revoked</Badge> : <Badge variant="success">Active</Badge>}</td>
              </tr>
            ))}</tbody>
          </table></div>
        ) : <EmptyState icon={<KeyRound />} title="No tokens issued" detail="Everyone uses the pool token. Run `ylcc tokens create` to issue per-machine tokens." />}
      </section>
    </>
  )
}

// ---------- Shared pieces ----------

function Metric({ icon, label, value, note, tone }: { icon: ReactNode; label: string; value: string; note: string; tone: string }) {
  return (
    <>
      <div className={`metric-icon metric-icon-${tone}`}>{icon}</div>
      <div className="metric-copy"><span>{label}</span><strong>{value}</strong><small>{note}</small></div>
    </>
  )
}

function SectionHeading({ title, detail, action }: { title: string; detail: string; action?: ReactNode }) {
  return (
    <div className="section-heading">
      <div><h2>{title}</h2><span>{detail}</span></div>
      {action}
    </div>
  )
}

function Detail({ label, children }: { label: string; children: ReactNode }) {
  return <div className="detail-item"><dt>{label}</dt><dd>{children}</dd></div>
}

function RelativeTime({ timestamp }: { timestamp: number }) {
  const now = useNow()
  const milliseconds = timestamp < 1_000_000_000_000 ? timestamp * 1000 : timestamp
  return <time dateTime={new Date(milliseconds).toISOString()} title={formatTime(timestamp)}>{formatRelative(timestamp, now)}</time>
}

function WorkerState({ worker, activeJobs }: { worker: WorkerInfo; activeJobs: number }) {
  const progress = workerUpdateProgress(worker, activeJobs)
  if (progress) return <Badge variant="warning"><span className="badge-dot" />{progress.label}</Badge>
  if (!worker.online) return <Badge variant="muted"><span className="badge-dot" />Offline</Badge>
  if (worker.paused) return <Badge variant="warning"><span className="badge-dot" />Paused</Badge>
  return <Badge variant="success"><span className="badge-dot" />Online</Badge>
}

function StatusBadge({ state }: { state: string }) {
  const variant = ['succeeded', 'ready', 'complete'].includes(state) ? 'success'
    : ['running', 'assigned', 'preparing'].includes(state) ? 'default'
      : ['failed', 'unknown'].includes(state) ? 'danger'
        : ['queued', 'pending', 'cancelling'].includes(state) ? 'warning' : 'muted'
  return <Badge variant={variant}><span className={variant === 'default' ? 'badge-dot badge-dot-live' : 'badge-dot'} />{state}</Badge>
}

function EmptyState({ icon, title, detail, children }: { icon: ReactNode; title: string; detail: string; children?: ReactNode }) {
  return <div className="empty-state"><span className="empty-icon">{icon}</span><div><strong>{title}</strong><p>{detail}</p></div>{children && <div className="empty-action">{children}</div>}</div>
}

function LoadingState({ compact, label = 'Loading…' }: { compact?: boolean; label?: string }) {
  return (
    <div className={compact ? 'loading-state loading-state-compact' : 'loading-state'} role="status">
      <span className="loading-indicator" aria-hidden="true" />
      {compact ? <p>{label}</p> : <><strong>Connecting to Hub</strong><p><Clock3 size={12} /> Reading workers, jobs, and environments</p></>}
    </div>
  )
}

function osGlyph(os: string): string {
  return os === 'windows' ? 'W' : os === 'macos' ? 'M' : 'L'
}

export default App
