import assert from 'node:assert/strict'
import { test } from 'node:test'
import { InfiniteQueryObserver, QueryClient } from '@tanstack/react-query'

// The API module's shared clock and token lookup need only these browser interfaces.
Object.defineProperties(globalThis, {
  window: { value: { setInterval: () => 0 }, configurable: true },
  sessionStorage: { value: { getItem: () => 'fixture' }, configurable: true },
})
const { jobsHistoryQuery, jobListQuery, jobQuery, snapshotQuery, latestReleaseQuery, post, workerUpdateProgress } = await import('../src/lib/api.ts')

test('worker updates distinguish job draining, installation and reconnection', () => {
  const worker = { updating: 'v0.5.10', online: true }
  assert.deepEqual(workerUpdateProgress(worker, 1), {
    phase: 'draining', label: 'Waiting for jobs',
    detail: 'Waiting for 1 active job to finish. Then v0.5.10 installs automatically.',
  })
  assert.match(workerUpdateProgress(worker, 2)!.detail, /2 active jobs/)
  assert.equal(workerUpdateProgress(worker, 0)!.phase, 'updating')
  assert.equal(workerUpdateProgress({ ...worker, online: false }, 1)!.phase, 'reconnecting')
  assert.equal(workerUpdateProgress({ online: true, updating: null }, 1), null)
})

test('release checks are cached and update refusals show the server reason', async (t) => {
  const paths: string[] = []
  t.mock.method(globalThis, 'fetch', async (path: string, init?: RequestInit) => {
    paths.push(path)
    assert.equal(new Headers(init?.headers).get('Authorization'), 'Bearer fixture')
    if (path === '/v1/releases/latest') return Response.json({ version: 'v0.5.7' })
    assert.equal(init?.method, 'POST')
    assert.deepEqual(JSON.parse(init?.body as string), { version: 'v0.5.7' })
    return Response.json({ error: 'worker is offline' }, { status: 409 })
  })
  const client = new QueryClient({ defaultOptions: { queries: { gcTime: Infinity } } })
  try {
    assert.deepEqual(await client.fetchQuery(latestReleaseQuery), { version: 'v0.5.7' })
    await client.fetchQuery(latestReleaseQuery)
    assert.equal(paths.length, 1)
    await assert.rejects(post('/v1/workers/one/update', { version: 'v0.5.7' }), { message: 'worker is offline' })
  } finally { client.clear() }
})

test('history pages stay sequential through duplicate triggers, errors and retry', async (t) => {
  const offsets: number[] = []
  let fail = false
  t.mock.method(globalThis, 'fetch', async (input: string) => {
    const offset = Number(new URL(input, 'http://localhost').searchParams.get('offset'))
    offsets.push(offset)
    await new Promise((resolve) => setTimeout(resolve, 5))
    if (fail) return new Response('Temporary failure', { status: 503 })
    assert.equal(new URL(input, 'http://localhost').searchParams.get('limit'), '50')
    assert.equal(new URL(input, 'http://localhost').pathname, '/v1/jobs/summary')
    return Response.json({ total: 70, jobs: Array.from({ length: offset === 0 ? 50 : 20 }, (_, i) => ({ id: `job-${offset + i}` })) })
  })
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: Infinity } } })
  const observer = new InfiniteQueryObserver(client, { ...jobsHistoryQuery(), enabled: false })
  const unsubscribe = observer.subscribe(() => {})
  try {
    assert.deepEqual(offsets, [])
    await Promise.all([
      observer.fetchNextPage({ cancelRefetch: false }),
      observer.fetchNextPage({ cancelRefetch: false }),
    ])
    assert.deepEqual(offsets, [0])
    assert.equal(observer.getCurrentResult().hasNextPage, true)
    fail = true
    await observer.fetchNextPage()
    assert.equal(observer.getCurrentResult().isError, true)
    assert.equal(observer.getCurrentResult().data?.pages.flatMap((page) => page.jobs).length, 50)
    fail = false
    await observer.fetchNextPage()
    const result = observer.getCurrentResult()
    assert.deepEqual(offsets, [0, 50, 50])
    assert.deepEqual(result.data?.pageParams, [0, 50])
    assert.equal(result.data?.pages.flatMap((page) => page.jobs).length, 70)
    assert.equal(result.hasNextPage, false)
    await observer.fetchNextPage()
    assert.deepEqual(offsets, [0, 50, 50])
  } finally {
    unsubscribe()
    client.clear()
  }
})

test('snapshot and filters use summaries; full data is requested by the detail query', async (t) => {
  const paths: string[] = []
  t.mock.method(globalThis, 'fetch', async (input: string) => {
    paths.push(input)
    return Response.json(input.includes('/summary') ? { jobs: [], total: 2047, stats: { states: {}, workers: {} } } : [])
  })
  const client = new QueryClient({ defaultOptions: { queries: { gcTime: Infinity } } })
  try {
    const snapshot = await client.fetchQuery(snapshotQuery)
    assert.equal(snapshot.jobList.total, 2047)
    assert.ok(paths.includes('/v1/jobs/summary?limit=50&offset=0'))
    assert.ok(!paths.some((path) => /^\/v1\/jobs(?:\?|$)/.test(path)))
    await client.fetchQuery(jobListQuery({ q: '한글', state: 'failed', worker: 'one' }))
    const filtered = new URL(paths.at(-1)!, 'http://localhost')
    assert.equal(filtered.searchParams.get('q'), '한글')
    assert.equal(filtered.searchParams.get('worker'), 'one')
    assert.equal(filtered.searchParams.get('state'), 'failed')
    await client.fetchQuery(jobQuery('old/job'))
    assert.equal(paths.at(-1), '/v1/jobs/old%2Fjob')
  } finally { client.clear() }
})


test('host snapshots are capability-gated for old Hubs', async (t) => {
  for (const supported of [false, true]) {
    const paths: string[] = []
    t.mock.method(globalThis, 'fetch', async (path: string) => {
      paths.push(path)
      if (path === '/health') return Response.json({ capabilities: supported ? ['host_scheduling'] : [] })
      if (path === '/v1/hosts') return Response.json([{ id: 'shared', available_slots: 1 }])
      return Response.json(path.includes('/summary') ? { jobs: [], total: 0 } : [])
    })
    const client = new QueryClient({ defaultOptions: { queries: { gcTime: Infinity, retry: false } } })
    try {
      const snapshot = await client.fetchQuery(snapshotQuery)
      assert.equal(paths.includes('/v1/hosts'), supported)
      assert.equal(snapshot.hosts?.[0]?.id, supported ? 'shared' : undefined)
    } finally { client.clear() }
    t.mock.restoreAll()
  }
})
