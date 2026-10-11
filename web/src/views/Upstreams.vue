<template>
  <div>
    <h2 class="page-title">上游配置</h2>
    <el-alert type="info" :closable="false" style="margin-bottom: 16px"
      title="修改保存后立即重建上游注册表；凭据文件路径沿用自动探测（可在下方为 Trae 指定 storage.json）。" />

    <el-card shadow="never">
      <template #header>全局</template>
      <el-form label-width="140px" style="max-width: 560px">
        <el-form-item label="默认上游">
          <el-select v-model="cfg.default_upstream" style="width: 240px">
            <el-option v-for="n in knownNames" :key="n" :value="n" :label="n" />
          </el-select>
        </el-form-item>
      </el-form>
    </el-card>

    <el-card shadow="never" style="margin-top: 16px">
      <template #header>
        <div class="card-head">
          <span>各上游</span>
          <div>
            <el-button size="small" @click="load">刷新</el-button>
            <el-button size="small" type="primary" :loading="saving" @click="save">保存并重载</el-button>
          </div>
        </div>
      </template>
      <el-table :data="rows" size="small">
        <el-table-column prop="name" label="上游" width="140" />
        <el-table-column label="启用" width="90">
          <template #default="{ row }">
            <el-switch v-model="row.enabled" />
          </template>
        </el-table-column>
        <el-table-column label="模型数" width="90">
          <template #default="{ row }">{{ row.model_count ?? '-' }}</template>
        </el-table-column>
        <el-table-column label="后端地址覆盖（codebuddy）">
          <template #default="{ row }">
            <el-input v-if="row.name === 'codebuddy'" v-model="row.backend" placeholder="留空用默认 https://copilot.tencent.com" />
            <span v-else class="dim">—</span>
          </template>
        </el-table-column>
        <el-table-column label="Chat Host 覆盖（trae）">
          <template #default="{ row }">
            <el-input v-if="row.name === 'trae'" v-model="row.chat_host" placeholder="留空用默认 https://trae-api-cn.mchost.guru" />
            <span v-else class="dim">—</span>
          </template>
        </el-table-column>
        <el-table-column label="凭据状态" min-width="200">
          <template #default="{ row }">
            <span class="dim">{{ row.healthSummary }}</span>
          </template>
        </el-table-column>
      </el-table>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { api, type AdminConfig, type UpstreamRow } from '../api'

const cfg = ref<AdminConfig>({ default_upstream: '', cli_default_upstream: '', upstreams: {} })
const live = ref<UpstreamRow[]>([])
const saving = ref(false)

interface Row {
  name: string
  enabled: boolean
  backend: string
  chat_host: string
  model_count?: number
  healthSummary: string
}

const knownNames = ['codebuddy', 'workbuddy', 'trae', 'opencode']

const rows = computed<Row[]>(() => {
  const names = new Set<string>([...Object.keys(cfg.value.upstreams), ...knownNames])
  return [...names].sort().map(name => {
    const o = cfg.value.upstreams[name] ?? { enabled: true }
    const lv = live.value.find(u => u.name === name)
    return {
      name,
      enabled: o.enabled,
      backend: (o as { backend?: string | null }).backend ?? '',
      chat_host: (o as { chat_host?: string | null }).chat_host ?? '',
      model_count: lv?.model_count,
      healthSummary: lv ? summarize(lv.health) : '（未启用或无凭据）',
    }
  })
})

function summarize(health: Record<string, unknown>): string {
  const cred = health.credential as Record<string, unknown> | undefined
  if (cred) {
    const parts: string[] = []
    if (typeof cred.uid === 'string' && cred.uid) parts.push('uid=' + cred.uid.slice(0, 12))
    if (typeof cred.variant === 'string') parts.push(cred.variant)
    if (typeof cred.token_expired === 'boolean') parts.push(cred.token_expired ? 'token已过期' : 'token有效')
    return parts.join(' · ') || '已配置'
  }
  const err = health.credential_error as string | undefined
  return err ? '错误: ' + err.slice(0, 80) : '—'
}

async function load() {
  const [c, u] = await Promise.all([api.getConfig(), api.upstreams()])
  cfg.value = c
  live.value = u.upstreams
}

async function save() {
  saving.value = true
  try {
    const upstreams: Record<string, unknown> = {}
    for (const r of rows.value) {
      upstreams[r.name] = {
        enabled: r.enabled,
        backend: r.name === 'codebuddy' ? r.backend : undefined,
        chat_host: r.name === 'trae' ? r.chat_host : undefined,
      }
    }
    await api.putConfig({ default_upstream: cfg.value.default_upstream, upstreams })
    ElMessage.success('已保存并重载')
    await load()
  } catch (e) {
    ElMessage.error('保存失败: ' + (e as Error).message)
  } finally {
    saving.value = false
  }
}

onMounted(load)
</script>

<style scoped>
.card-head { display: flex; justify-content: space-between; align-items: center; }
.dim { color: var(--el-text-color-secondary); }
</style>
