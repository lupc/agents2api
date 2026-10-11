<template>
  <div>
    <h2 class="page-title">积分看板</h2>
    <el-radio-group v-model="upstream" style="margin-bottom: 16px">
      <el-radio-button value="trae">Trae</el-radio-button>
      <el-radio-button value="buddy">Buddy</el-radio-button>
    </el-radio-group>

    <el-alert v-if="error" type="error" :closable="false" :title="error" style="margin-bottom: 16px" />
    <el-skeleton v-if="loading" :rows="4" animated />
    <template v-if="info">
      <!-- Buddy：summary 数值 + 积分包明细 -->
      <div class="stat-row" v-if="upstream === 'buddy'">
        <el-card class="stat-card" shadow="never">
          <div class="stat-label">剩余积分</div>
          <div class="stat-value primary">{{ fmtNum(info.remaining_credits) }}</div>
          <div class="stat-sub" v-if="info.source">数据源：{{ info.source }}</div>
        </el-card>
        <el-card class="stat-card" shadow="never" v-if="info.total_credits != null">
          <div class="stat-label">总额度</div>
          <div class="stat-value">{{ fmtNum(info.total_credits) }}</div>
        </el-card>
        <el-card class="stat-card" shadow="never">
          <div class="stat-label">今日消耗</div>
          <div class="stat-value">{{ fmtNum(info.today_consumed) }}</div>
          <div class="stat-sub" v-if="info.today_consumed_error" :title="info.today_consumed_error">
            获取失败
          </div>
        </el-card>
        <el-card class="stat-card" shadow="never">
          <div class="stat-label">7天内即将过期</div>
          <div class="stat-value danger">{{ fmtNum(expiringSoon.sum) }}</div>
          <div class="stat-sub">{{ expiringSoon.count }} 项</div>
        </el-card>
        <el-card class="stat-card" shadow="never">
          <div class="stat-label">账号</div>
          <div class="stat-value-small">{{ info.uid || '—' }}</div>
          <div class="stat-sub">
            <el-tag size="small" :type="info.token_expired ? 'danger' : 'success'">
              {{ info.token_expired ? 'token 已过期' : 'token 有效' }}
            </el-tag>
          </div>
        </el-card>
      </div>

      <!-- Trae：summary 数值 -->
      <div class="stat-row" v-if="upstream === 'trae'">
        <el-card class="stat-card" shadow="never">
          <div class="stat-label">剩余积分</div>
          <div class="stat-value primary">{{ fmtNum(info.remaining_credits) }}</div>
          <div class="stat-sub" v-if="info.consumption_ratio != null">
            已用 {{ ((info.consumption_ratio ?? 0) * 100).toFixed(1) }}%
          </div>
        </el-card>
        <el-card class="stat-card" shadow="never" v-if="info.total_credits != null">
          <div class="stat-label">总额度</div>
          <div class="stat-value">{{ fmtNum(info.total_credits) }}</div>
        </el-card>
        <el-card class="stat-card" shadow="never">
          <div class="stat-label">今日消耗</div>
          <div class="stat-value">{{ fmtNum(info.today_consumed) }}</div>
          <div class="stat-sub" v-if="info.today_consumed_error" :title="info.today_consumed_error">
            获取失败
          </div>
        </el-card>
        <el-card class="stat-card" shadow="never">
          <div class="stat-label">7天内即将过期</div>
          <div class="stat-value danger">{{ fmtNum(expiringSoon.sum) }}</div>
          <div class="stat-sub">{{ expiringSoon.count }} 项</div>
        </el-card>
        <el-card class="stat-card" shadow="never">
          <div class="stat-label">账号</div>
          <div class="stat-value-small">{{ info.uid }}</div>
          <div class="stat-sub">
            <el-tag size="small" :type="info.token_expired ? 'danger' : 'success'">
              {{ info.token_expired ? 'token 已过期' : 'token 有效' }}
            </el-tag>
          </div>
        </el-card>
      </div>

      <!-- 积分包明细（两个上游通用） -->
      <el-card shadow="never" style="margin-top: 16px">
        <template #header>积分包明细</template>
        <el-table :data="packs" size="small" stripe>
          <el-table-column prop="name" label="来源" min-width="160" />
          <el-table-column label="获得积分" width="110">
            <template #default="{ row }">{{ fmtNum(row.granted) }}</template>
          </el-table-column>
          <el-table-column label="剩余" width="110">
            <template #default="{ row }">{{ fmtNum(row.remaining) }}</template>
          </el-table-column>
          <el-table-column label="已用" width="110">
            <template #default="{ row }">{{ fmtNum(row.used) }}</template>
          </el-table-column>
          <el-table-column label="过期时间" width="140">
            <template #default="{ row }">
              <el-tag v-if="isExpiringSoon(row)" size="small" type="danger" effect="dark">
                {{ row.end_time }}
              </el-tag>
              <span v-else>{{ row.end_time || '—' }}</span>
            </template>
          </el-table-column>
        </el-table>
      </el-card>
    </template>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { api, type CreditsInfo } from '../api'

const upstream = ref<'trae' | 'buddy'>('trae')
const info = ref<CreditsInfo | null>(null)
const error = ref('')
const loading = ref(false)

const packs = computed(() => {
  const list = info.value?.packs ?? []
  // 按过期时间升序（即将过期的排前面），无日期的排末尾
  return [...list].sort((a, b) => {
    const da = parseDate(a.end_time)
    const db = parseDate(b.end_time)
    if (!da && !db) return 0
    if (!da) return 1
    if (!db) return -1
    return da.getTime() - db.getTime()
  })
})

// 7 天内即将过期的积分项（end_time 为 "YYYY-MM-DD" 本地日期）
const EXPIRING_DAYS = 7

function parseDate(s?: string | null): Date | null {
  if (!s) return null
  const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(s)
  if (!m) return null
  return new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]))
}

function isExpiringSoon(row: { end_time?: string | null }): boolean {
  const d = parseDate(row.end_time)
  if (!d) return false
  const now = new Date()
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate())
  const deadline = new Date(today)
  deadline.setDate(deadline.getDate() + EXPIRING_DAYS)
  return d >= today && d <= deadline
}

const expiringSoon = computed(() => {
  const items = packs.value.filter(isExpiringSoon)
  return {
    count: items.length,
    sum: items.reduce((acc, p) => acc + (Number(p.remaining) || 0), 0),
  }
})

function fmtNum(n: unknown): string {
  const v = typeof n === 'number' ? n : parseFloat(String(n ?? ''))
  if (isNaN(v)) return '—'
  return v.toLocaleString('zh-CN', { maximumFractionDigits: 2 })
}

async function load() {
  loading.value = true
  error.value = ''
  info.value = null
  try {
    info.value = await api.credits(upstream.value)
  } catch (e) {
    error.value = (e as Error).message
  } finally {
    loading.value = false
  }
}

onMounted(load)
watch(upstream, load)
</script>

<style scoped>
.stat-row { display: flex; gap: 12px; }
.stat-card { flex: 1; min-width: 0; }
.stat-label { color: var(--el-text-color-secondary); font-size: 12px; }
.stat-value { font-size: 22px; font-weight: 700; margin-top: 4px; }
.stat-value.primary { color: var(--el-color-primary); }
.stat-value.danger { color: var(--el-color-danger); }
.stat-value-small { font-size: 14px; font-weight: 600; margin-top: 6px; word-break: break-all; }
.stat-sub { color: var(--el-text-color-secondary); font-size: 12px; margin-top: 4px; }
</style>
