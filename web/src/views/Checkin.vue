<template>
  <div>
    <h2 class="page-title">一键签到（Trae）</h2>
    <el-card shadow="never">
      <div style="display: flex; align-items: center; gap: 16px">
        <el-button type="primary" size="large" :loading="checking" @click="doCheckin">
          立即签到
        </el-button>
        <span v-if="lastResult" :class="statusClass(lastResult.status)">
          上次结果：{{ statusText(lastResult.status) }} · {{ lastResult.message }}
          <template v-if="lastResult.delta > 0">（+{{ lastResult.delta }} 积分）</template>
        </span>
      </div>
    </el-card>

    <el-row :gutter="16" style="margin-top: 16px">
      <el-col :span="14">
        <el-card shadow="never">
          <template #header>签到趋势（近 {{ days }} 天）</template>
          <el-table :data="trend" size="small" stripe>
            <el-table-column prop="date" label="日期" width="120" />
            <el-table-column prop="ok" label="成功" width="80" />
            <el-table-column prop="already" label="已签" width="80" />
            <el-table-column prop="failed" label="失败" width="80" />
          </el-table>
        </el-card>
      </el-col>
      <el-col :span="10">
        <el-card shadow="never">
          <template #header>最近记录</template>
          <el-table :data="records" size="small" stripe>
            <el-table-column label="时间" width="150">
              <template #default="{ row }">{{ row.date.slice(5) }} {{ row.time }}</template>
            </el-table-column>
            <el-table-column label="状态" width="90">
              <template #default="{ row }">
                <el-tag size="small" :type="statusTagType(row.status)">{{ statusText(row.status) }}</el-tag>
              </template>
            </el-table-column>
            <el-table-column prop="message" label="信息" min-width="120" />
          </el-table>
        </el-card>
      </el-col>
    </el-row>
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { api, type CheckinRecord } from '../api'

const checking = ref(false)
const lastResult = ref<CheckinRecord | null>(null)
const records = ref<CheckinRecord[]>([])
const trend = ref<{ date: string; ok: number; already: number; failed: number }[]>([])
const days = ref(30)

function statusText(s: string) {
  return s === 'success' ? '成功' : s === 'already' ? '已签' : '失败'
}
function statusTagType(s: string): 'success' | 'info' | 'danger' {
  return s === 'success' ? 'success' : s === 'already' ? 'info' : 'danger'
}
function statusClass(s: string) {
  return s === 'failed' ? 'fail' : 'ok'
}

async function load() {
  const d = await api.checkinResults(days.value)
  records.value = d.records
  trend.value = d.trend
  if (d.records.length) lastResult.value = d.records[0]
}

async function doCheckin() {
  checking.value = true
  try {
    const r = await api.checkin()
    lastResult.value = r.result
    if (r.result.status === 'success') ElMessage.success('签到成功')
    else if (r.result.status === 'already') ElMessage.info('今日已签到')
    else ElMessage.error('签到失败: ' + r.result.message)
    await load()
  } catch (e) {
    ElMessage.error('签到失败: ' + (e as Error).message)
  } finally {
    checking.value = false
  }
}

onMounted(load)
</script>

<style scoped>
.ok { color: var(--el-color-success); }
.fail { color: var(--el-color-danger); }
</style>
