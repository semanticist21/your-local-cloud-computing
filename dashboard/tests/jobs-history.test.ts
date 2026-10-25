import assert from 'node:assert/strict'
import { test } from 'node:test'
import { InfiniteQueryObserver, QueryClient } from '@tanstack/react-query'

// The API module's shared clock and token lookup need only these browser interfaces.
Object.defineProperties(globalThis, {
  window: { value: { setInterval: () => 0 }, configurable: true },
  sessionStorage: { value: { getItem: () => 'fixture' }, configurable: true },
})
const { jobsHistoryQuery, jobListQuery, jobQuery, snapshotQuery } = await import('../src/lib/api.ts')

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
