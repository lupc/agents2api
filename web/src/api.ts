import axios from 'axios'

const client = axios.create({ baseURL: '/admin/api', timeout: 60000 })

export interface Overview {
  window_days: number
  requests: number
  ok: number
  errors: number
  success_rate: number
  prompt_tokens: number
  completion_tokens: number
  total_tokens: number
  avg_duration_ms: number
  p95_duration_ms: number
  today?: { requests: number; ok: number; errors: number; prompt_tokens: number; completion_tokens: number }
  upstreams: number
  default_upstream: string
}

export interface UsageDay {
  date: string
  requests: number
  ok: number
  errors: number
  success_rate: number
  stream_requests: number
  non_stream_requests: number
  prompt_tokens: number
  completion_tokens: number
  total_tokens: number
  avg_duration_ms: number
  p50_duration_ms: number
  p95_duration_ms: number
  max_duration_ms: number
  avg_ttfb_ms: number
  upstreams: { name: string; requests: number; ok: number; errors: number }[]
  models: { name: string; requests: number; ok: number; errors: number }[]
}

export interface ModelLatency {
  model: string
  samples: number
  requests: number
  ok: number
  errors: number
  avg_duration_ms: number
  p50_duration_ms: number
  p95_duration_ms: number
  max_duration_ms: number
  avg_ttfb_ms: number
  p95_ttfb_ms: number
}

export interface UpstreamRow {
  name: string
  model_count: number
  health: Record<string, unknown>
}

export interface AdminConfig {
  default_upstream: string
  cli_default_upstream: string
  upstreams: Record<string, { enabled: boolean; backend?: string | null; chat_host?: string | null }>
}

export interface CheckinRecord {
  date: string
  time: string
  uid: string
  status: 'success' | 'already' | 'failed'
  message: string
  credits: number
  delta: number
}

export interface CreditsInfo {
  uid: string
  token_expired: boolean
  total_credits?: number
  consumed_credits?: number
  remaining_credits?: number
  consumption_ratio?: number
  packs?: { name: string | null; expire_time: number | null }[]
  ent_usage_error?: string
  checkin_status_error?: string
}

export const api = {
  overview: (days = 7) => client.get<Overview>('/overview', { params: { days } }).then(r => r.data),
  usage: (days = 7) => client.get<{ window_days: number; days: UsageDay[] }>('/stats/usage', { params: { days } }).then(r => r.data),
  modelLatency: (days = 7) => client.get<{ models: ModelLatency[] }>('/stats/models', { params: { days } }).then(r => r.data),
  upstreams: () => client.get<{ upstreams: UpstreamRow[] }>('/upstreams').then(r => r.data),
  getConfig: () => client.get<AdminConfig>('/config').then(r => r.data),
  putConfig: (cfg: unknown) => client.put('/config', cfg).then(r => r.data),
  models: () => client.get<{ by_upstream: { upstream: string; models: string[] }[]; snapshot?: { synced_at: string; by_upstream: { upstream: string; count: number; models: string[] }[] } }>('/models').then(r => r.data),
  sync: () => client.post('/sync').then(r => r.data),
  checkin: () => client.post<{ ok: boolean; result: CheckinRecord }>('/checkin/trae').then(r => r.data),
  checkinResults: (days = 30) => client.get<{ records: CheckinRecord[]; trend: { date: string; ok: number; already: number; failed: number }[] }>('/checkin/results', { params: { days } }).then(r => r.data),
  credits: () => client.get<CreditsInfo>('/credits/trae').then(r => r.data),
  logs: (lines = 200) => client.get<{ lines: string[]; file: string | null }>('/logs', { params: { lines } }).then(r => r.data),
}

export function fmtTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M'
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'K'
  return String(n)
}

export function fmtMs(n: number): string {
  if (n >= 1000) return (n / 1000).toFixed(1) + 's'
  return n + 'ms'
}
