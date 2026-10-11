<template>
  <div>
    <h2 class="page-title">汇总统计</h2>
    <el-row :gutter="16">
      <el-col :span="6" v-for="c in cards" :key="c.label">
        <el-card shadow="never">
          <div class="stat-label">{{ c.label }}</div>
          <div class="stat-value">{{ c.value }}</div>
          <div class="stat-sub" v-if="c.sub">{{ c.sub }}</div>
        </el-card>
      </el-col>
    </el-row>

    <el-row :gutter="16" style="margin-top: 16px">
      <el-col :span="14">
        <el-card shadow="never">
          <template #header>请求趋势（近 {{ days }} 天）</template>
          <div ref="trendChart" style="height: 320px"></div>
        </el-card>
      </el-col>
      <el-col :span="10">
        <el-card shadow="never">
          <template #header>模型速率榜（P95 耗时，近 {{ days }} 天）</template>
          <div ref="rateChart" style="height: 320px"></div>
        </el-card>
      </el-col>
    </el-row>

    <el-card shadow="never" style="margin-top: 16px">
      <template #header>
        <div class="card-head">
          <span>按天用量</span>
          <el-radio-group v-model="days" size="small">
            <el-radio-button :value="1">今天</el-radio-button>
            <el-radio-button :value="7">7 天</el-radio-button>
            <el-radio-button :value="30">30 天</el-radio-button>
          </el-radio-group>
        </div>
      </template>
      <el-table :data="usage" size="small" stripe>
        <el-table-column prop="date" label="日期" width="110" />
        <el-table-column prop="requests" label="请求数" width="90" />
        <el-table-column label="成功/失败" width="110">
          <template #default="{ row }">{{ row.ok }} / {{ row.errors }}</template>
        </el-table-column>
        <el-table-column label="流式/非流式" width="110">
          <template #default="{ row }">{{ row.stream_requests }} / {{ row.non_stream_requests }}</template>
        </el-table-column>
        <el-table-column label="Tokens" width="120">
          <template #default="{ row }">{{ fmtTokens(row.prompt_tokens) }} / {{ fmtTokens(row.completion_tokens) }}</template>
        </el-table-column>
        <el-table-column label="P50 耗时" width="100">
          <template #default="{ row }">{{ fmtMs(row.p50_duration_ms) }}</template>
        </el-table-column>
        <el-table-column label="P95 耗时" width="100">
          <template #default="{ row }">{{ fmtMs(row.p95_duration_ms) }}</template>
        </el-table-column>
        <el-table-column label="平均 TTFB" width="100">
          <template #default="{ row }">{{ fmtMs(row.avg_ttfb_ms) }}</template>
        </el-table-column>
      </el-table>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref, watch, nextTick, computed } from 'vue'
import * as echarts from 'echarts'
import { api, fmtTokens, fmtMs, type Overview, type UsageDay, type ModelLatency } from '../api'

const days = ref(7)
const overview = ref<Overview | null>(null)
const usage = ref<UsageDay[]>([])
const latency = ref<ModelLatency[]>([])
const trendChart = ref<HTMLElement>()
const rateChart = ref<HTMLElement>()

const cards = computed(() => {
  const ov = overview.value
  return [
    { label: `请求数（${days.value} 天）`, value: ov ? ov.requests : '…', sub: `成功 ${ov?.ok ?? '-'} / 失败 ${ov?.errors ?? '-'}` },
    { label: '成功率', value: ov ? ov.success_rate + '%' : '…', sub: '' },
    { label: 'Tokens', value: ov ? fmtTokens(ov.total_tokens) : '…', sub: `输入 ${fmtTokens(ov?.prompt_tokens ?? 0)} / 输出 ${fmtTokens(ov?.completion_tokens ?? 0)}` },
    { label: 'P95 耗时', value: ov ? fmtMs(ov.p95_duration_ms) : '…', sub: `平均 ${fmtMs(ov?.avg_duration_ms ?? 0)}` },
  ]
})

function renderTrend() {
  if (!trendChart.value) return
  const chart = echarts.init(trendChart.value)
  chart.setOption({
    tooltip: { trigger: 'axis' },
    legend: { data: ['请求数', '成功', '失败', 'Tokens'] },
    grid: { left: 50, right: 50, bottom: 30, top: 40 },
    xAxis: { type: 'category', data: usage.value.map(d => d.date.slice(5)) },
    yAxis: [{ type: 'value', name: '请求' }, { type: 'value', name: 'tokens' }],
    series: [
      { name: '请求数', type: 'bar', data: usage.value.map(d => d.requests) },
      { name: '成功', type: 'line', data: usage.value.map(d => d.ok) },
      { name: '失败', type: 'line', data: usage.value.map(d => d.errors) },
      { name: 'Tokens', type: 'line', yAxisIndex: 1, data: usage.value.map(d => d.total_tokens) },
    ],
  })
}

function renderRate() {
  if (!rateChart.value) return
  const chart = echarts.init(rateChart.value)
  const top = latency.value.slice(0, 8)
  chart.setOption({
    tooltip: { trigger: 'axis' },
    grid: { left: 140, right: 30, bottom: 30, top: 20 },
    xAxis: { type: 'value', name: 'ms' },
    yAxis: { type: 'category', data: top.map(m => m.model), inverse: true },
    series: [
      { name: 'P50', type: 'bar', data: top.map(m => m.p50_duration_ms) },
      { name: 'P95', type: 'bar', data: top.map(m => m.p95_duration_ms) },
    ],
  })
}

async function load() {
  const [ov, u, ml] = await Promise.all([
    api.overview(days.value),
    api.usage(days.value),
    api.modelLatency(days.value),
  ])
  overview.value = ov
  usage.value = u.days
  latency.value = ml.models
  await nextTick()
  renderTrend()
  renderRate()
}

onMounted(load)
watch(days, load)
</script>

<style scoped>
.stat-label { color: var(--el-text-color-secondary); font-size: 13px; }
.stat-value { font-size: 26px; font-weight: 700; margin-top: 6px; }
.stat-sub { color: var(--el-text-color-secondary); font-size: 12px; margin-top: 4px; }
.card-head { display: flex; justify-content: space-between; align-items: center; }
</style>
