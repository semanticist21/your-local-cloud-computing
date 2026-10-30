import { useId, useState, type FormEvent } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { Button } from './ui'
import { del, errorMessage, formatRelative, put, useNow, whoamiQuery, type HostConfig, type HostInfo, type WorkerInfo } from '../lib/api'

export function HostStatus({ host }: { host: HostInfo }) {
  const now = useNow()
  return <span className="host-status">
    <span>{host.occupied} / {host.max_jobs} slots occupied · {host.available_slots} available</span>
    <span>{host.cpu_percent == null ? 'CPU unavailable' : `CPU ${host.cpu_percent.toFixed(0)}%`}{host.cpu_at != null && ` · ${formatRelative(host.cpu_at, now)}`}</span>
    {host.wait_reason && <span role="status">{host.wait_reason}</span>}
  </span>
}

export function HostEditor({ host, workers, hosts }: { host?: HostInfo; workers: WorkerInfo[]; hosts: HostInfo[] }) {
  const principal = useQuery(whoamiQuery)
  const [open, setOpen] = useState(false)
  if (principal.data?.role !== 'admin') return null
  return <div className="host-editor">
    <Button variant="outline" size="sm" onClick={() => setOpen(!open)} aria-expanded={open}>{open ? 'Close settings' : host ? 'Host settings' : 'Add host'}</Button>
    {open && <HostForm host={host} workers={workers} hosts={hosts} close={() => setOpen(false)} />}
  </div>
}

function HostForm({ host, workers, hosts, close }: { host?: HostInfo; workers: WorkerInfo[]; hosts: HostInfo[]; close: () => void }) {
  const prefix = useId()
  const queryClient = useQueryClient()
  const [name, setName] = useState(host?.id ?? '')
  const [members, setMembers] = useState(host?.workers ?? [])
  const [maxJobs, setMaxJobs] = useState(host?.max_jobs ?? 1)
  const [guard, setGuard] = useState(!!host?.cpu_guard)
  const [source, setSource] = useState(host?.cpu_guard?.metrics_worker ?? '')
  const [pause, setPause] = useState(host?.cpu_guard?.pause_above ?? 90)
  const [resume, setResume] = useState(host?.cpu_guard?.resume_below ?? 75)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const assignedElsewhere = new Set(hosts.filter((h) => h.managed && h.id !== host?.id).flatMap((h) => h.workers))
  const sources = workers.filter((w) => members.includes(w.id) && !w.capabilities.containerized && w.capabilities.cpu_metrics)
  async function save(event: FormEvent) {
    event.preventDefault()
    setError('')
    if (!members.length) { setError('Select at least one worker.'); return }
    if (guard && (!sources.some((w) => w.id === source) || resume >= pause)) {
      setError('Choose a native CPU measurement worker and set the resume threshold below the pause threshold.'); return
    }
    setBusy(true)
    const config: HostConfig = { workers: members, max_jobs: maxJobs, cpu_guard: guard ? { metrics_worker: source, pause_above: pause, resume_below: resume } : null }
    try {
      await put(`/v1/hosts/${encodeURIComponent(name)}`, config)
      await queryClient.invalidateQueries({ queryKey: ['snapshot'] })
      close()
    } catch (cause) { setError(errorMessage(cause)) }
    finally { setBusy(false) }
  }
  async function remove() {
    setBusy(true)
    setError('')
    try {
      await del(`/v1/hosts/${encodeURIComponent(host!.id)}`)
      await queryClient.invalidateQueries({ queryKey: ['snapshot'] })
      close()
    } catch (cause) { setError(errorMessage(cause)) }
    finally { setBusy(false) }
  }
  return <form className="host-form" onSubmit={save} aria-label={host ? `Settings for ${host.id}` : 'New physical host'}>
    <fieldset disabled={busy}>
      <legend>{host ? `${host.id} settings` : 'Physical host'}</legend>
      <p className="muted-text">Group workers only when they share the same physical computer. Existing jobs keep running.</p>
      <label htmlFor={`${prefix}-name`}>Host name</label>
      <input id={`${prefix}-name`} value={name} onChange={(e) => setName(e.target.value)} required maxLength={128} pattern="[A-Za-z0-9][A-Za-z0-9_.-]*" disabled={!!host} />
      <fieldset className="host-members">
        <legend>Member workers</legend>
        {workers.map((w) => <label key={w.id}>
          <input type="checkbox" checked={members.includes(w.id)} disabled={assignedElsewhere.has(w.id)} onChange={(e) => setMembers(e.target.checked ? [...members, w.id] : members.filter((id) => id !== w.id))} />
          <span>{w.id}{assignedElsewhere.has(w.id) ? ' (another host)' : ''}</span>
        </label>)}
      </fieldset>
      <label htmlFor={`${prefix}-jobs`}>Maximum concurrent jobs on this host</label>
      <input id={`${prefix}-jobs`} type="number" min={1} max={256} required value={maxJobs} onChange={(e) => setMaxJobs(e.target.valueAsNumber)} />
      <label className="host-toggle"><input type="checkbox" checked={guard} onChange={(e) => setGuard(e.target.checked)} /> Pause new assignments when CPU stays busy</label>
      {guard && <>
        <label htmlFor={`${prefix}-source`}>CPU measurement worker</label>
        <select id={`${prefix}-source`} required value={source} onChange={(e) => setSource(e.target.value)}>
          <option value="">Select a native worker</option>
          {sources.map((w) => <option key={w.id} value={w.id}>{w.id}</option>)}
        </select>
        {!sources.length && <p className="muted-text">A member native worker must be upgraded to report CPU measurement validity.</p>}
        <div className="host-thresholds">
          <label htmlFor={`${prefix}-pause`}>Pause at CPU %<input id={`${prefix}-pause`} type="number" step="any" min={0} max={100} required value={pause} onChange={(e) => setPause(e.target.valueAsNumber)} /></label>
          <label htmlFor={`${prefix}-resume`}>Resume at CPU %<input id={`${prefix}-resume`} type="number" step="any" min={0} max={100} required value={resume} onChange={(e) => setResume(e.target.valueAsNumber)} /></label>
        </div>
        <p className="muted-text">Each threshold must hold for 30 seconds. Missing CPU measurements pause new assignments after 60 seconds. This does not cap CPU usage.</p>
      </>}
      {error && <p className="form-error" role="alert">{error}</p>}
      <div className="host-actions">
        <Button type="submit" disabled={busy}>{busy ? 'Saving…' : 'Save host'}</Button>
        {host && <Button type="button" variant="outline" onClick={() => void remove()} disabled={busy}>Remove host grouping</Button>}
      </div>
    </fieldset>
  </form>
}
