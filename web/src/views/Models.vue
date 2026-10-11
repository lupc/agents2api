<template>
  <div>
    <h2 class="page-title">模型同步</h2>

    <el-card shadow="never">
      <template #header>
        <div class="card-head">
          <span>
            同步官网模型
            <el-select v-model="activeUpstream" size="small" style="width: 140px; margin: 0 8px">
              <el-option v-for="u in catalogUpstreams" :key="u" :value="u" :label="u" />
            </el-select>
            <b>{{ currentModels.length }}</b> 个模型
          </span>
          <div>
            <el-button size="small" @click="loadAll">刷新</el-button>
            <el-button type="primary" size="small" :loading="syncing" @click="doSync">
              同步官网模型
            </el-button>
          </div>
        </div>
      </template>
      <div class="hint">
        从上游模型目录接口拉取并替换本地缓存（倍率 / 思考档位 / 图片模态以服务端为准）。
      </div>

      <!-- 详细目录表格（buddy 等提供完整元数据的上游） -->
      <el-table v-if="isDetailed" :data="currentDetailed" size="small" stripe>
        <el-table-column prop="id" label="模型 ID" min-width="150" />
        <el-table-column prop="display" label="展示名" min-width="160" />
        <el-table-column prop="vendor" label="厂商" width="110" />
        <el-table-column label="积分倍率" width="90">
          <template #default="{ row }">
            <span class="rate">{{ row.rate > 0 ? row.rate.toFixed(2) : '0.00' }}</span>
          </template>
        </el-table-column>
        <el-table-column label="思考档位" min-width="140">
          <template #default="{ row }">{{ row.efforts.join(' / ') || '—' }}</template>
        </el-table-column>
        <el-table-column label="上下文" width="90">
          <template #default="{ row }">{{ fmtContext(row.context_length) }}</template>
        </el-table-column>
        <el-table-column label="图片支持" width="90">
          <template #default="{ row }">
            <el-icon v-if="row.supports_image" color="var(--el-color-success)"><Select /></el-icon>
            <span v-else class="dim">✗</span>
          </template>
        </el-table-column>
      </el-table>

      <!-- 简单列表（仅 id 的上游，如 opencode） -->
      <template v-else>
        <div v-if="currentError" class="dim" style="margin: 8px 0">同步不可用：{{ currentError }}</div>
        <el-tag v-for="m in currentModels" :key="m" size="small" style="margin: 2px">{{ m }}</el-tag>
      </template>
    </el-card>

    <el-card shadow="never" style="margin-top: 16px">
      <template #header>每日定时同步</template>
      <div style="display: flex; align-items: center; gap: 12px; flex-wrap: wrap">
        <el-checkbox v-model="settings.daily_sync_enabled">每日定时同步</el-checkbox>
        <el-time-select
          v-model="settings.daily_sync_time"
          start="00:00"
          step="00:15"
          end="23:45"
          size="small"
          style="width: 110px"
          :disabled="!settings.daily_sync_enabled"
        />
        <el-button size="small" :loading="saving" @click="saveSettings">保存</el-button>
        <span class="dim">应用运行期间每天定时自动同步（无可用账号则跳过）</span>
      </div>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { Select } from '@element-plus/icons-vue'
import { api } from '../api'

interface ModelInfo {
  id: string
  display: string
  vendor: string
  rate: number
  efforts: string[]
  context_length: number
  max_tokens: number
  supports_image: boolean
}

interface CatalogEntry {
  count?: number
  models?: ModelInfo[]
  error?: string | null
}

const syncing = ref(false)
const saving = ref(false)
const activeUpstream = ref('buddy')
const byUpstream = ref<Record<string, CatalogEntry>>({})
const settings = ref({ daily_sync_enabled: false, daily_sync_time: '05:45' })

const catalogUpstreams = computed(() =>
  Object.keys(byUpstream.value).sort((a, b) => {
    // 有详细目录的排前面
    const da = byUpstream.value[a].models ? 0 : 1
    const db = byUpstream.value[b].models ? 0 : 1
    return da - db || a.localeCompare(b)
  }),
)

const currentEntry = computed<CatalogEntry | null>(
  () => byUpstream.value[activeUpstream.value] ?? null,
)
const currentDetailed = computed<ModelInfo[]>(() => currentEntry.value?.models ?? [])
const currentModels = computed<string[]>(() =>
  currentDetailed.value.length
    ? currentDetailed.value.map(m => m.id)
    : // 无详细目录时退化为普通模型列表（来自 live models）
      (liveLists.value[activeUpstream.value] ?? []),
)
const currentError = computed(() => currentEntry.value?.error ?? '')
const isDetailed = computed(() => currentDetailed.value.length > 0)

const liveLists = ref<Record<string, string[]>>({})

function fmtContext(n: number): string {
  if (!n) return '—'
  if (n >= 1000) return Math.round(n / 1000) + 'k'
  return String(n)
}

async function loadAll() {
  const [models, settingsData] = await Promise.all([api.models(), api.getSettings()])
  settings.value = {
    daily_sync_enabled: settingsData.daily_sync_enabled,
    daily_sync_time: settingsData.daily_sync_time,
  }
  // live 模型列表（简单形态）
  const live: Record<string, string[]> = {}
  for (const u of models.by_upstream) live[u.upstream] = u.models
  liveLists.value = live
  // 详细目录快照（sync 后存在）
  const snap = models.snapshot as
    | { by_upstream: Record<string, CatalogEntry> }
    | null
  byUpstream.value = snap?.by_upstream ?? {}
  // 快照不存在时用 live 列表占位，保证页面有内容
  if (!Object.keys(byUpstream.value).length) {
    for (const [name, list] of Object.entries(live)) {
      byUpstream.value[name] = { models: list.map(id => emptyInfo(id)) }
    }
  }
}

function emptyInfo(id: string): ModelInfo {
  return {
    id,
    display: id,
    vendor: '',
    rate: 0,
    efforts: [],
    context_length: 0,
    max_tokens: 0,
    supports_image: false,
  }
}

async function doSync() {
  syncing.value = true
  try {
    const r = await api.sync()
    byUpstream.value = r.by_upstream
    const entry = byUpstream.value[activeUpstream.value]
    if (entry?.error) ElMessage.warning(activeUpstream.value + ' 同步失败: ' + entry.error)
    else ElMessage.success('同步完成')
  } catch (e) {
    ElMessage.error('同步失败: ' + (e as Error).message)
  } finally {
    syncing.value = false
  }
}

async function saveSettings() {
  saving.value = true
  try {
    await api.putSettings(settings.value)
    ElMessage.success('已保存')
  } catch (e) {
    ElMessage.error('保存失败: ' + (e as Error).message)
  } finally {
    saving.value = false
  }
}

onMounted(loadAll)
</script>

<style scoped>
.card-head { display: flex; justify-content: space-between; align-items: center; }
.hint { color: var(--el-text-color-secondary); font-size: 12px; margin-bottom: 12px; }
.rate { color: var(--el-color-warning); font-weight: 600; }
.dim { color: var(--el-text-color-secondary); font-size: 12px; }
</style>
