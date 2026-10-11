<template>
  <div>
    <h2 class="page-title">积分看板（Trae）</h2>
    <el-alert v-if="error" type="error" :closable="false" :title="error" style="margin-bottom: 16px" />
    <template v-if="info">
      <el-row :gutter="16">
        <el-col :span="8">
          <el-card shadow="never">
            <div class="stat-label">剩余积分</div>
            <div class="stat-value primary">{{ info.remaining_credits ?? '…' }}</div>
            <div class="stat-sub" v-if="info.consumption_ratio != null">
              已用 {{ ((info.consumption_ratio ?? 0) * 100).toFixed(1) }}%
            </div>
          </el-card>
        </el-col>
        <el-col :span="8">
          <el-card shadow="never">
            <div class="stat-label">总额度</div>
            <div class="stat-value">{{ info.total_credits ?? '…' }}</div>
            <div class="stat-sub">已消耗 {{ info.consumed_credits ?? '-' }}</div>
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

      <el-card shadow="never" style="margin-top: 16px">
        <template #header>积分包明细</template>
        <el-table :data="packs" size="small" stripe>
          <el-table-column prop="name" label="来源" min-width="160" />
          <el-table-column label="过期时间" width="200">
            <template #default="{ row }">{{ fmtExpire(row.expire_time) }}</template>
          </el-table-column>
        </el-table>
      </el-card>

      <el-card shadow="never" style="margin-top: 16px" v-if="info.ent_usage_error || info.checkin_status_error">
        <template #header>上游错误</template>
        <div v-if="info.ent_usage_error" class="dim">积分接口：{{ info.ent_usage_error }}</div>
        <div v-if="info.checkin_status_error" class="dim">签到状态：{{ info.checkin_status_error }}</div>
      </el-card>
    </template>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, type CreditsInfo } from '../api'

const info = ref<CreditsInfo | null>(null)
const error = ref('')

const packs = computed(() => info.value?.packs ?? [])

function fmtExpire(ts: number | null): string {
  if (!ts) return '—'
  // 秒级或毫秒级时间戳兼容
  const ms = ts > 1e12 ? ts : ts * 1000
  const d = new Date(ms)
  return isNaN(d.getTime()) ? String(ts) : d.toLocaleString('zh-CN')
}

onMounted(async () => {
  try {
    info.value = await api.credits()
  } catch (e) {
    error.value = (e as Error).message
  }
})
</script>

<style scoped>
.stat-label { color: var(--el-text-color-secondary); font-size: 13px; }
.stat-value { font-size: 28px; font-weight: 700; margin-top: 6px; }
.stat-value.primary { color: var(--el-color-primary); }
.stat-value-small { font-size: 16px; font-weight: 600; margin-top: 10px; word-break: break-all; }
.stat-sub { color: var(--el-text-color-secondary); font-size: 12px; margin-top: 4px; }
.dim { color: var(--el-text-color-secondary); font-size: 12px; }
</style>
