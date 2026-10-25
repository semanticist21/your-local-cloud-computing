import assert from 'node:assert/strict'
import { test } from 'node:test'
import { InfiniteQueryObserver, QueryClient } from '@tanstack/react-query'

// The API module's shared clock and token lookup need only these browser interfaces.
Object.defineProperties(globalThis, {
  window: { value: { setInterval: () => 0 }, configurable: true },
  sessionStorage: { value: { getItem: () => 'fixture' }, configurable: true },
})
const { jobsHistoryQuery } = await import('../src/lib/api.ts')

test('history pages stay sequential through duplicate triggers, errors and retry', async (t) => {
  const offsets: number[] = []
  let fail = false
  t.mock.method(globalThis, 'fetch', async (input: string) => {
    const offset = Number(new URL(input, 'http://localhost').searchParams.get('offset'))
    offsets.push(offset)
    await new Promise((resolve) => setTimeout(resolve, 5))
    if (fail) return new Response('Temporary failure', { status: 503 })
    return Response.json(Array.from({ length: offset === 200 ? 200 : 50 }, (_, i) => ({ id: `job-${offset + i}` })))
  })
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: Infinity } } })
  const observer = new InfiniteQueryObserver(client, jobsHistoryQuery)
  const unsubscribe = observer.subscribe(() => {})
  try {
    assert.deepEqual(offsets, [])
    await Promise.all([
      observer.fetchNextPage({ cancelRefetch: false }),
      observer.fetchNextPage({ cancelRefetch: false }),
    ])
    assert.deepEqual(offsets, [200])
    assert.equal(observer.getCurrentResult().hasNextPage, true)
    fail = true
    await observer.fetchNextPage()
    assert.equal(observer.getCurrentResult().isError, true)
    assert.equal(observer.getCurrentResult().data?.pages.flat().length, 200)
    fail = false
    await observer.fetchNextPage()
    const result = observer.getCurrentResult()
    assert.deepEqual(offsets, [200, 400, 400])
    assert.deepEqual(result.data?.pageParams, [200, 400])
    assert.equal(result.data?.pages.flat().length, 250)
    assert.equal(result.hasNextPage, false)
    await observer.fetchNextPage()
    assert.deepEqual(offsets, [200, 400, 400])
  } finally {
    unsubscribe()
    client.clear()
  }
})
