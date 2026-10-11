<template>
  <div>
    <h2 class="page-title">一键签到</h2>
    <el-card shadow="never">
      <div style="display: flex; align-items: center; gap: 16px; flex-wrap: wrap">
        <el-radio-group v-model="upstream">
          <el-radio-button value="trae">Trae</el-radio-button>
          <el-radio-button value="buddy">Buddy</el-radio-button>
        </el-radio-group>
        <el-button type="primary" :loading="checking" @click="doCheckin">
          立即签到
        </el-button>
        <el-button type="success" :loading="checkingAll" @click="doCheckinAll">
          一键签到全部上游
        </el-button>
        <span v-if="lastResult" :class="statusClass(lastResult.status)">
          {{ statusText(lastResult.status) }} · {{ lastResult.message }}
          <template v-if="lastResult.delta > 0">（+{{ lastResult.delta }} 积分）</template>
        </span>
      </div>
      <div v-if="allSummary" class="all-summary">
        <el-tag
          v-for="r in allSummary.results"
          :key="r.result.upstream || r.result.uid"
          size="small"
          :type="statusTagType(r.result.status)"
        >
          {{ r.result.upstream || r.result.uid }}：{{ statusText(r.result.status) }}
        </el-tag>
      </div>
    </el-card>

    <el-card shadow="never" style="margin-top: 16px">
      <template #header>自动签到任务（可勾选时间点）</template>
      <div style="display: flex; align-items: center; gap: 12px; flex-wrap: wrap">
        <el-checkbox v-model="autoCfg.checkin_auto_enabled">启用自动签到</el-checkbox>
        <el-time-select
          v-model="newTime"
          start="00:00"
          step="00:15"
          end="23:45"
          size="small"
          style="width: 110px"
          :disabled="!autoCfg.checkin_auto_enabled"
          placeholder="选择时间点"
        />
        <el-button size="small" :disabled="!autoCfg.checkin_auto_enabled || !newTime" @click="addTime">
          添加时间点
        </el-button>
        <el-button size="small" type="primary" :loading="savingAuto" @click="saveAuto">保存</el-button>
        <span class="dim">应用运行期间，命中勾选时间点即自动一键签到全部上游（每天每个时间点仅触发一次）</span>
      </div>
      <el-checkbox-group
        v-model="autoCfg.checkin_times"
        :disabled="!autoCfg.checkin_auto_enabled"
        style="margin-top: 12px"
      >
        <el-checkbox
          v-for="t in timeOptions"
          :key="t"
          :value="t"
          border
          size="small"
          style="margin: 0 8px 8px 0"
        >
          {{ t }}
        </el-checkbox>
      </el-checkbox-group>
      <div v-if="!timeOptions.length" class="dim">暂无可选时间点，请先添加。</div>
    </el-card>

    <el-row :gutter="16" style="margin-top: 16px">
      <el-col :span="14">
        <el-card shadow="never">
          <template #header>签到趋势（{{ upstream }} · 近 {{ days }} 天）</template>
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
import { onMounted, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { api, type CheckinAllResult, type CheckinRecord } from '../api'

const upstream = ref<'trae' | 'buddy'>('trae')
const checking = ref(false)
const checkingAll = ref(false)
const allSummary = ref<CheckinAllResult | null>(null)
const lastResult = ref<CheckinRecord | null>(null)
const records = ref<CheckinRecord[]>([])
const trend = ref<{ date: string; ok: number; already: number; failed: number }[]>([])
const days = ref(30)

// 自动签到任务配置（勾选时间点列表）
const autoCfg = ref<{ checkin_auto_enabled: boolean; checkin_times: string[] }>({
  checkin_auto_enabled: false,
  checkin_times: [],
})
const timeOptions = ref<string[]>([])
const newTime = ref('')
const savingAuto = ref(false)

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
  const d = await api.checkinResults(days.value, upstream.value)
  records.value = d.records
  trend.value = d.trend
  if (d.records.length) lastResult.value = d.records[0]
  else lastResult.value = null
}

async function loadAuto() {
  const s = await api.getSettings()
  autoCfg.value = {
    checkin_auto_enabled: s.checkin_auto_enabled,
    checkin_times: [...(s.checkin_times ?? [])],
  }
  syncTimeOptions()
}

// 已选时间点 + 新添加的候选时间点合并展示，已选的可直接勾选
function syncTimeOptions() {
  const set = new Set<string>(timeOptions.value)
  for (const t of autoCfg.value.checkin_times) set.add(t)
  timeOptions.value = [...set].sort()
}

async function doCheckin() {
  checking.value = true
  try {
    const r = await api.checkin(upstream.value)
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

async function doCheckinAll() {
  checkingAll.value = true
  try {
    const r = await api.checkinAll()
    allSummary.value = r
    if (r.ok_count === r.total) ElMessage.success(`全部签到完成（${r.ok_count}/${r.total}）`)
    else ElMessage.warning(`签到完成（${r.ok_count}/${r.total} 成功）`)
    await load()
  } catch (e) {
    ElMessage.error('一键签到失败: ' + (e as Error).message)
  } finally {
    checkingAll.value = false
  }
}

function addTime() {
  const t = newTime.value
  if (!t) return
  if (!autoCfg.value.checkin_times.includes(t)) autoCfg.value.checkin_times.push(t)
  syncTimeOptions()
  newTime.value = ''
}

async function saveAuto() {
  savingAuto.value = true
  try {
    await api.putSettings({
      checkin_auto_enabled: autoCfg.value.checkin_auto_enabled,
      checkin_times: [...autoCfg.value.checkin_times].sort(),
    })
    ElMessage.success('已保存自动签到任务')
  } catch (e) {
    ElMessage.error('保存失败: ' + (e as Error).message)
  } finally {
    savingAuto.value = false
  }
}

onMounted(() => {
  load()
  loadAuto()
})
watch(upstream, load)
</script>

<style scoped>
.ok { color: var(--el-color-success); }
.fail { color: var(--el-color-danger); }
.all-summary { margin-top: 12px; display: flex; gap: 8px; flex-wrap: wrap; }
.dim { color: var(--el-text-color-secondary); font-size: 12px; }
</style>
