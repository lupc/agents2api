<template>
  <div>
    <h2 class="page-title">吞吐速度</h2>

    <el-row :gutter="16">
      <!-- 左：上游 → 模型 树形导航 -->
      <el-col :span="6">
        <el-card shadow="never" class="tree-card">
          <template #header>
            <div class="card-head">
              <span>模型导航</span>
              <el-button size="small" text @click="load">刷新</el-button>
            </div>
          </template>
          <el-input v-model="treeFilter" size="small" placeholder="筛选模型…" clearable style="margin-bottom: 8px" />
          <div v-if="!data.models.length" class="dim" style="padding: 40px 0; text-align: center">
            暂无模型数据，发起一次对话后即可看到。
          </div>
          <el-tree
            v-else
            :data="treeData"
            :props="{ label: 'label', children: 'children' }"
            :filter-node-method="filterNode"
            ref="tree"
            node-key="key"
            highlight-current
            default-expand-all
            @node-click="onSelect"
          >
            <template #default="{ data }">
              <span class="tree-node">
                <span class="tree-label">{{ data.label }}</span>
                <span v-if="data.model" class="dim tree-turns">{{ data.turns }} 轮</span>
              </span>
            </template>
          </el-tree>
        </el-card>
      </el-col>

      <!-- 右：选中模型统计汇总 -->
      <el-col :span="18">
        <el-card shadow="never">
          <template #header>
            <div class="card-head">
              <span>{{ selected ? `${selected.model}（${selected.upstream}）` : '模型统计' }}</span>
              <el-radio-group v-model="hours" size="small">
                <el-radio-button :value="6">近 6 小时</el-radio-button>
                <el-radio-button :value="24">近 24 小时</el-radio-button>
                <el-radio-button :value="72">近 3 天</el-radio-button>
              </el-radio-group>
            </div>
          </template>

          <div v-if="!selected" class="dim" style="padding: 100px 0; text-align: center">
            在左侧选择一个模型查看它的速度统计。
          </div>
          <template v-else>
            <!-- 汇总指标 -->
            <el-row :gutter="12" class="stat-row">
              <el-col :span="6">
                <div class="mini-card">
                  <div class="stat-label">平均预填充速度</div>
                  <div class="mini-value" :class="tpsClass(sel.avg_prefill_fresh_tps)">{{ fmtTps(sel.avg_prefill_fresh_tps) }}</div>
                  <div class="stat-sub">含缓存 {{ fmtTps(sel.avg_prefill_tps) }}</div>
                </div>
              </el-col>
              <el-col :span="6">
                <div class="mini-card">
                  <div class="stat-label">平均解码速度</div>
                  <div class="mini-value" :class="tpsClass(sel.avg_decode_tps)">{{ fmtTps(sel.avg_decode_tps) }}</div>
                  <div class="stat-sub">输出 tokens / 生成耗时</div>
                </div>
              </el-col>
              <el-col :span="6">
                <div class="mini-card">
                  <div class="stat-label">输入 / 输出 Tokens</div>
                  <div class="mini-value">{{ fmtTokens(sel.prompt_tokens) }} / {{ fmtTokens(sel.completion_tokens) }}</div>
                  <div class="stat-sub">{{ sel.turns }} 轮</div>
                </div>
              </el-col>
              <el-col :span="6">
                <div class="mini-card">
                  <div class="stat-label">缓存命中</div>
                  <div class="cache-body">
                    <div ref="cacheChart" class="cache-chart"></div>
                    <div class="cache-legend">
                      <div><span class="dot cached-dot"></span>命中 <b>{{ cachePct }}%</b></div>
                      <div><span class="dot fresh-dot"></span>未命中 <b>{{ 100 - cachePct }}%</b></div>
                      <div class="dim cache-sum">{{ fmtTokens(sel.cached_tokens) }} / {{ fmtTokens(sel.prompt_tokens) }}</div>
                    </div>
                  </div>
                </div>
              </el-col>
            </el-row>

            <!-- 整体速度趋势（该模型，按轮次） -->
            <div class="section-title">整体速度趋势</div>
            <div class="hint">
              按轮次依次排列（最多最近 {{ recent }} 条）。预填充速度 = 未命中缓存的输入 tokens / 首 token 耗时（TTFT）；
              解码速度 = 输出 tokens / 生成阶段耗时，生成阶段过短（&lt;100ms）时不可测故不绘制。
            </div>
            <div v-if="!chartTurns.length" class="dim" style="padding: 30px 0; text-align: center">
              所选窗口内暂无该模型的轮次数据。
            </div>
            <div v-else ref="selChart" style="height: 180px"></div>

            <!-- 轮次记录（仅该模型，分页） -->
            <div class="section-title">
              轮次记录
              <span class="dim" style="font-size: 12px; font-weight: 400">
                （共 {{ modelTurns.length }} 轮）
              </span>
            </div>
            <el-table
              :data="pagedTurns"
              size="small"
              stripe
              empty-text="所选窗口内暂无该模型的逐轮记录"
            >
              <el-table-column label="时间" width="90">
                <template #default="{ row }">{{ row.time }}</template>
              </el-table-column>
              <el-table-column label="模式" width="76">
                <template #default="{ row }">
                  <el-tag size="small" :type="row.stream ? 'success' : 'info'">
                    {{ row.stream ? '流式' : '非流' }}
                  </el-tag>
                </template>
              </el-table-column>
              <el-table-column label="Tokens（入/出）" width="130">
                <template #default="{ row }">
                  {{ row.prompt_tokens }} / {{ row.completion_tokens }}
                </template>
              </el-table-column>
              <el-table-column label="缓存命中 Tokens" width="120">
                <template #default="{ row }">
                  <span v-if="row.cached_tokens > 0" class="cached">{{ row.cached_tokens }}</span>
                  <span v-else class="dim">—</span>
                </template>
              </el-table-column>
              <el-table-column label="缓存命中率" width="110">
                <template #default="{ row }">
                  <span v-if="row.cached_tokens > 0">{{ cacheRateOf(row) }}%</span>
                  <span v-else class="dim">—</span>
                </template>
              </el-table-column>
              <el-table-column label="TTFT" width="90">
                <template #default="{ row }">{{ fmtMs(row.ttft_ms) }}</template>
              </el-table-column>
              <el-table-column label="总耗时" width="90">
                <template #default="{ row }">{{ fmtMs(row.duration_ms) }}</template>
              </el-table-column>
              <el-table-column label="预填充速度" width="110">
                <template #default="{ row }">
                  <span :class="tpsClass(row.prefill_fresh_tps)">{{ fmtTps(row.prefill_fresh_tps) }}</span>
                </template>
              </el-table-column>
              <el-table-column label="解码速度" width="110">
                <template #default="{ row }">
                  <span :class="tpsClass(row.decode_tps)">{{ fmtTps(row.decode_tps) }}</span>
                </template>
              </el-table-column>
            </el-table>
            <el-pagination
              v-if="modelTurns.length > pageSize"
              v-model:current-page="page"
              class="pager"
              layout="prev, pager, next"
              small
              background
              :page-size="pageSize"
              :total="modelTurns.length"
            />
          </template>
        </el-card>
      </el-col>
    </el-row>
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import * as echarts from 'echarts'
import { api, fmtMs, fmtTokens, type SpeedData, type SpeedModelRow } from '../api'

const data = ref<SpeedData>({
  window_hours: 24,
  summary: { turns: 0, avg_prefill_tps: 0, avg_prefill_fresh_tps: 0, avg_decode_tps: 0, prompt_tokens: 0, cached_tokens: 0, completion_tokens: 0 },
  hourly: [],
  daily: [],
  models: [],
  models_series: [],
  turns: [],
})
const hours = ref(24)
const recent = ref(200)
const selChart = ref<HTMLElement>()
const cacheChart = ref<HTMLElement>()

// ── 左树：上游 → 模型 ────────────────────────────────────────────
const tree = ref()
const treeFilter = ref('')
const selected = ref<{ upstream: string; model: string } | null>(null)

interface TreeLeaf {
  key: string
  label: string
  model?: string
  upstream?: string
  turns?: number
  children?: TreeLeaf[]
}

const treeData = computed<TreeLeaf[]>(() => {
  const byUp = new Map<string, TreeLeaf>()
  for (const m of data.value.models) {
    let up = byUp.get(m.upstream)
    if (!up) {
      up = { key: `up:${m.upstream}`, label: m.upstream, children: [] }
      byUp.set(m.upstream, up)
    }
    up.children!.push({
      key: `um:${m.upstream}\u{1f}${m.model}`,
      label: m.model,
      model: m.model,
      upstream: m.upstream,
      turns: m.turns,
    })
  }
  return [...byUp.values()].sort((a, b) => a.label.localeCompare(b.label))
})

watch(treeFilter, v => tree.value?.filter(v))

function filterNode(value: string, d: TreeLeaf): boolean {
  if (!value) return true
  if (!d.model) return (d.children ?? []).some(c => c.label.toLowerCase().includes(value.toLowerCase()))
  return d.label.toLowerCase().includes(value.toLowerCase())
}

function onSelect(d: TreeLeaf) {
  selected.value = d.model && d.upstream ? { upstream: d.upstream, model: d.model } : null
}

// 选中模型的聚合行与小时序列
const selectedRow = computed<SpeedModelRow | null>(() => {
  if (!selected.value) return null
  return (
    data.value.models.find(
      m => m.upstream === selected.value!.upstream && m.model === selected.value!.model,
    ) ?? null
  )
})
const sel = computed(() => selectedRow.value ?? {
  upstream: '', model: '', turns: 0, avg_prefill_tps: 0, avg_prefill_fresh_tps: 0,
  avg_decode_tps: 0, prompt_tokens: 0, cached_tokens: 0, completion_tokens: 0,
})

// 选中模型的逐轮记录（从全局 turns 里按 upstream+model 过滤；接口按时间倒序返回）
const modelTurns = computed(() => {
  if (!selected.value) return []
  return data.value.turns.filter(
    t => t.upstream === selected.value!.upstream && t.model === selected.value!.model,
  )
})

// 轮次记录分页
const page = ref(1)
const pageSize = 20
const pagedTurns = computed(() => {
  const start = (page.value - 1) * pageSize
  return modelTurns.value.slice(start, start + pageSize)
})

// 趋势图按轮次排列（接口是倒序，需翻转成时间正序）
const chartTurns = computed(() => [...modelTurns.value].reverse())

function cacheRateOf(r: { prompt_tokens: number; cached_tokens: number }): number {
  return r.prompt_tokens > 0 ? Math.round((r.cached_tokens / r.prompt_tokens) * 100) : 0
}

// 选中模型的缓存命中率（供饼图与图例）
const cachePct = computed(() => cacheRateOf(sel.value))

// ── 工具 ────────────────────────────────────────────────────────
function fmtTps(v: number): string {
  if (!v || v <= 0) return '—'
  return v >= 100 ? Math.round(v).toString() : v.toFixed(1)
}

function tpsClass(v: number): string {
  if (!v || v <= 0) return 'dim'
  return v >= 30 ? 'fast' : v >= 10 ? 'mid' : 'slow'
}

// ── 图表 ────────────────────────────────────────────────────────
let selChartInst: echarts.ECharts | null = null

function renderSelected() {
  if (!selChart.value) return
  if (selChartInst && selChartInst.getDom() !== selChart.value) {
    selChartInst.dispose()
    selChartInst = null
  }
  if (!selChartInst) selChartInst = echarts.init(selChart.value)
  if (!chartTurns.value.length) {
    selChartInst.clear()
    return
  }
  selChartInst.setOption({
    backgroundColor: 'transparent',
    tooltip: {
      trigger: 'axis',
      valueFormatter: (v: number) => (v > 0 ? `${v} tokens/s` : '不可测'),
    },
    legend: { data: ['预填充速度', '解码速度'] },
    grid: { left: 60, right: 20, bottom: 28, top: 36 },
    xAxis: {
      type: 'category',
      boundaryGap: false,
      data: chartTurns.value.map(t => t.time),
      axisLabel: { interval: 'auto', hideOverlap: true },
    },
    yAxis: {
      type: 'value',
      name: 'tokens/s',
      axisLabel: { formatter: (v: number) => `${v}` },
    },
    series: [
      {
        name: '预填充速度',
        type: 'line',
        smooth: true,
        // 透明渐变面积填充（自上而下由蓝到无）
        areaStyle: {
          color: new echarts.graphic.LinearGradient(0, 0, 0, 1, [
            { offset: 0, color: 'rgba(64,158,255,0.28)' },
            { offset: 1, color: 'rgba(64,158,255,0.02)' },
          ]),
        },
        // 值为 0 表示该轮不可测（无输入或超时），留空断开而非画成 0
        data: chartTurns.value.map(t => (t.prefill_fresh_tps > 0 ? t.prefill_fresh_tps : null)),
      },
      {
        name: '解码速度',
        type: 'line',
        smooth: true,
        areaStyle: {
          color: new echarts.graphic.LinearGradient(0, 0, 0, 1, [
            { offset: 0, color: 'rgba(103,194,58,0.24)' },
            { offset: 1, color: 'rgba(103,194,58,0.02)' },
          ]),
        },
        // 生成阶段 <100ms 的短回复不可测，记 null 断开
        data: chartTurns.value.map(t => (t.decode_tps > 0 ? t.decode_tps : null)),
      },
    ],
  })
}

// 缓存命中饼图（选中模型的输入 tokens 构成）
let cacheChartInst: echarts.ECharts | null = null

function renderCachePie() {
  if (!cacheChart.value) return
  if (cacheChartInst && cacheChartInst.getDom() !== cacheChart.value) {
    cacheChartInst.dispose()
    cacheChartInst = null
  }
  if (!cacheChartInst) cacheChartInst = echarts.init(cacheChart.value)
  const cached = Math.min(sel.value.cached_tokens, sel.value.prompt_tokens)
  const fresh = Math.max(0, sel.value.prompt_tokens - cached)
  if (sel.value.prompt_tokens <= 0) {
    cacheChartInst.clear()
    return
  }
  cacheChartInst.setOption({
    tooltip: {
      trigger: 'item',
      formatter: (p: { name: string; value: number; percent: number }) =>
        `${p.name}：${fmtTokens(p.value)} tokens（${p.percent}%）`,
    },
    series: [
      {
        type: 'pie',
        radius: ['58%', '88%'],
        label: { show: false },
        emphasis: { scale: false },
        silent: false,
        data: [
          { name: '命中', value: cached, itemStyle: { color: '#409eff' } },
          { name: '未命中', value: fresh, itemStyle: { color: '#dcdfe6' } },
        ],
      },
    ],
  })
}

async function load() {
  data.value = await api.speed(hours.value, recent.value)
  if (selected.value && !selectedRow.value) selected.value = null
  // 窗口变化后轮次集合变了，回到第一页避免停在越界页
  page.value = 1
  await nextTick()
  renderSelected()
}

onBeforeUnmount(() => {
  selChartInst?.dispose()
  selChartInst = null
  cacheChartInst?.dispose()
  cacheChartInst = null
})

onMounted(load)
watch(hours, load)
watch(selected, () => {
  page.value = 1
  nextTick(() => {
    renderSelected()
    renderCachePie()
  })
})
// 数据刷新（窗口切换）后同步重绘两张图
watch(data, () => nextTick(() => {
  renderSelected()
  renderCachePie()
}))
</script>

<style scoped>
.stat-label { color: var(--el-text-color-secondary); font-size: 13px; }
.stat-sub { color: var(--el-text-color-secondary); font-size: 12px; margin-top: 4px; }
.card-head { display: flex; justify-content: space-between; align-items: center; }
.dim { color: var(--el-text-color-secondary); }
.hint { color: var(--el-text-color-secondary); font-size: 12px; margin-bottom: 8px; }
.cached { color: var(--el-color-primary); }
/* 四张指标卡等高：列拉伸 + 卡片撑满，饼图卡用剩余高度 */
.stat-row :deep(.el-col) { display: flex; }
.stat-row .mini-card {
  flex: 1;
  display: flex;
  flex-direction: column;
}
.cache-body {
  flex: 1;
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 4px;
  min-height: 0;
}
.cache-chart { flex: none; width: 96px; height: 96px; }
.cache-legend { display: flex; flex-direction: column; gap: 4px; font-size: 12px; }
.cache-sum { font-size: 12px; }
.cache-legend b { color: var(--el-text-color-primary); }
.cache-legend .dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  display: inline-block;
  margin-right: 4px;
}
.pager { margin-top: 10px; justify-content: flex-end; }
.cached-dot { background: #409eff; }
.fresh-dot { background: #dcdfe6; }
.mini-card {
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  padding: 10px 14px;
}
.mini-value { font-size: 22px; font-weight: 700; margin-top: 4px; }
.section-title {
  font-size: 14px;
  font-weight: 600;
  margin: 16px 0 8px;
}
.tree-card :deep(.el-card__body) { padding-top: 10px; }
.tree-node { display: flex; align-items: center; width: 100%; font-size: 13px; }
.tree-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tree-turns { margin-left: auto; padding-left: 8px; }
.fast { color: var(--el-color-success); font-weight: 600; }
.mid { color: var(--el-color-warning); }
.slow { color: var(--el-color-danger); }
</style>
