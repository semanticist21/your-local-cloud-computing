import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { Area, AreaChart, CartesianGrid, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts'
import { errorMessage, formatBytes, workerMetricsQuery, type WorkerMetrics } from '../lib/api'

const ranges = [
  { label: '1h', seconds: 3600 },
  { label: '24h', seconds: 86400 },
  { label: '7d', seconds: 604800 },
]

type Series = {
  title: string
  value: (sample: WorkerMetrics) => number
  format: (value: number) => string
  details?: (sample: WorkerMetrics) => string | undefined
  max?: number
}

/** Host CPU, memory, load and disk history for one worker. */
export default function WorkerMetricsCharts({ workerId, os }: { workerId: string; os: string }) {
  const [range, setRange] = useState(ranges[0].seconds)
  const metrics = useQuery(workerMetricsQuery(workerId, range))
  const samples = metrics.data ?? []
  const latest = samples.at(-1)
  const series: Series[] = [
    { title: 'CPU', value: (s) => s.cpu_percent, format: (v) => `${v.toFixed(0)}%`, max: 100 },
    {
      title: 'Memory',
      details: (s) => s.memory_total ? `${formatBytes(s.memory_used)} / ${formatBytes(s.memory_total)}` : undefined,
      value: (s) => (s.memory_total ? (s.memory_used / s.memory_total) * 100 : 0),
      format: (v) => `${v.toFixed(0)}%`,
      max: 100,
    },
    ...(os === 'windows' ? [] : [{ title: 'Load (1 min)', value: (s: WorkerMetrics) => s.load, format: (v: number) => v.toFixed(2) }]),
    { title: 'Disk', value: (s) => s.disk_percent, format: (v) => `${v.toFixed(0)}%`, details: (s) => s.disk_total ? `${formatBytes(s.disk_used ?? 0)} / ${formatBytes(s.disk_total)}` : undefined, max: 100 },
  ]
  const tick = (at: number) =>
    new Intl.DateTimeFormat('en', range > 86400 ? { month: 'short', day: 'numeric' } : { hour: '2-digit', minute: '2-digit' }).format(new Date(at * 1000))

  return (
    <section className="content-section">
      <div className="section-heading">
        <div>
          <h2>Resource usage</h2>
          <span>{latest ? `Memory ${formatBytes(latest.memory_used)} of ${formatBytes(latest.memory_total)}` : 'Sampled every 15 s, kept 7 days'}</span>
        </div>
        <div className="range-switch" role="group" aria-label="Time range">
          {ranges.map((option) => (
            <button key={option.label} type="button" aria-pressed={range === option.seconds} onClick={() => setRange(option.seconds)}>
              {option.label}
            </button>
          ))}
        </div>
      </div>
      {metrics.error ? (
        <p className="muted-text">{errorMessage(metrics.error)}</p>
      ) : !samples.length ? (
        <p className="muted-text">{metrics.isPending ? 'Loading…' : 'No samples in this range. Workers on ylcc 0.5.2 or later report usage unless started with --no-metrics.'}</p>
      ) : (
        <div className="metrics-grid">
          {series.map((item) => {
            const data = samples.map((sample) => ({ ...sample, value: item.value(sample) }))
            return (
              <figure key={item.title} className="metric-chart">
                <figcaption>
                  {item.title}
                  <strong>{item.format(data.at(-1)?.value ?? 0)}</strong>
                </figcaption>
                <ResponsiveContainer width="100%" height={120}>
                  <AreaChart data={data} margin={{ top: 4, right: 4, bottom: 0, left: 0 }}>
                    <CartesianGrid stroke="#e7ecf1" vertical={false} />
                    <XAxis dataKey="at" type="number" domain={['dataMin', 'dataMax']} tickFormatter={tick} tick={{ fontSize: 10, fill: '#718094' }} tickLine={false} axisLine={false} minTickGap={40} />
                    <YAxis domain={[0, item.max ?? 'auto']} ticks={item.max ? [0, 50, 100] : undefined} width={34} tick={{ fontSize: 10, fill: '#718094' }} tickLine={false} axisLine={false} tickFormatter={(v: number) => (item.max ? `${v}%` : `${v}`)} />
                    <Tooltip
                      labelFormatter={(at) => new Date(Number(at) * 1000).toLocaleString()}
                      formatter={(value, _name, entry) => {
                        const details = item.details?.(entry.payload)
                        return [`${item.format(Number(value))}${details ? ` · ${details}` : ''}`, item.title]
                      }}
                      contentStyle={{ fontSize: 12, borderRadius: 5, borderColor: '#dce2e9' }}
                    />
                    <Area type="monotone" dataKey="value" stroke="#1759a8" strokeWidth={1.5} fill="#1759a8" fillOpacity={0.12} isAnimationActive={false} />
                  </AreaChart>
                </ResponsiveContainer>
              </figure>
            )
          })}
        </div>
      )}
    </section>
  )
}
