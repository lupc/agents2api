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
      <el-row v-if="upstream === 'buddy'" :gutter="16">
        <el-col :span="8">
          <el-card shadow="never">
            <div class="stat-label">剩余积分</div>
            <div class="stat-value primary">{{ fmtNum(info.remaining_credits) }}</div>
            <div class="stat-sub" v-if="info.source">数据源：{{ info.source }}</div>
          </el-card>
        </el-col>
        <el-col :span="8" v-if="info.total_credits != null">
          <el-card shadow="never">
            <div class="stat-label">总额度</div>
            <div class="stat-value">{{ fmtNum(info.total_credits) }}</div>
          </el-card>
        </el-col>
        <el-col :span="8">
          <el-card shadow="never">
            <div class="stat-label">账号</div>
            <div class="stat-value-small">Buddy（CodeBuddy / WorkBuddy）</div>
          </el-card>
        </el-col>
      </el-row>

      <!-- Trae：summary 数值 -->
      <el-row v-if="upstream === 'trae'" :gutter="16">
        <el-col :span="8">
          <el-card shadow="never">
            <div class="stat-label">剩余积分</div>
            <div class="stat-value primary">{{ fmtNum(info.remaining_credits) }}</div>
            <div class="stat-sub" v-if="info.consumption_ratio != null">
              已用 {{ ((info.consumption_ratio ?? 0) * 100).toFixed(1) }}%
            </div>
          </el-card>
        </el-col>
        <el-col :span="8" v-if="info.total_credits != null">
          <el-card shadow="never">
            <div class="stat-label">总额度</div>
            <div class="stat-value">{{ fmtNum(info.total_credits) }}</div>
            <div class="stat-sub">已消耗 {{ fmtNum(info.consumed_credits ?? 0) }}</div>
          </el-card>
        </el-col>
        <el-col :span="8">
          <el-card shadow="never">
            <div class="stat-label">账号</div>
            <div class="stat-value-small">{{ info.uid }}</div>
            <div class="stat-sub">
              <el-tag size="small" :type="info.token_expired ? 'danger' : 'success'">
                {{ info.token_expired ? 'token 已过期' : 'token 有效' }}
              </el-tag>
            </div>
          </el-card>
        </el-col>
      </el-row>

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
            <template #default="{ row }">{{ row.end_time || '—' }}</template>
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

const packs = computed(() => info.value?.packs ?? [])

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
.stat-label { color: var(--el-text-color-secondary); font-size: 13px; }
.stat-value { font-size: 28px; font-weight: 700; margin-top: 6px; }
.stat-value.primary { color: var(--el-color-primary); }
.stat-value-small { font-size: 16px; font-weight: 600; margin-top: 10px; word-break: break-all; }
.stat-sub { color: var(--el-text-color-secondary); font-size: 12px; margin-top: 4px; }
</style>
