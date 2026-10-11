<template>
  <div>
    <h2 class="page-title">模型同步</h2>
    <el-card shadow="never">
      <template #header>
        <div class="card-head">
          <span>
            各上游模型目录
            <el-tag v-if="syncedAt" size="small" type="success" style="margin-left: 8px">
              上次同步：{{ syncedAt.replace('T', ' ').slice(0, 19) }}
            </el-tag>
          </span>
          <el-button type="primary" size="small" :loading="syncing" @click="doSync">
            立即同步
          </el-button>
        </div>
      </template>
      <el-collapse v-model="opened">
        <el-collapse-item v-for="u in byUpstream" :key="u.upstream" :name="u.upstream">
          <template #title>
            <b>{{ u.upstream }}</b>
            <el-tag size="small" style="margin-left: 8px">{{ u.models.length }} 个模型</el-tag>
            <span v-if="snapshotCounts[u.upstream]" class="dim" style="margin-left: 8px">
              快照 {{ snapshotCounts[u.upstream] }} 个
            </span>
          </template>
          <el-tag v-for="m in u.models" :key="m" size="small" style="margin: 2px">{{ m }}</el-tag>
        </el-collapse-item>
      </el-collapse>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { api } from '../api'

const byUpstream = ref<{ upstream: string; models: string[] }[]>([])
const snapshot = ref<{ synced_at: string; by_upstream: { upstream: string; count: number; models: string[] }[] } | null>(null)
const syncing = ref(false)
const opened = ref(['codebuddy'])

const syncedAt = computed(() => snapshot.value?.synced_at ?? '')
const snapshotCounts = computed<Record<string, number>>(() => {
  const out: Record<string, number> = {}
  for (const u of snapshot.value?.by_upstream ?? []) out[u.upstream] = u.count
  return out
})

async function load() {
  const d = await api.models()
  byUpstream.value = d.by_upstream
  snapshot.value = d.snapshot ?? null
}

async function doSync() {
  syncing.value = true
  try {
    await api.sync()
    ElMessage.success('同步完成')
    await load()
  } catch (e) {
    ElMessage.error('同步失败: ' + (e as Error).message)
  } finally {
    syncing.value = false
  }
}

onMounted(load)
</script>

<style scoped>
.card-head { display: flex; justify-content: space-between; align-items: center; }
.dim { color: var(--el-text-color-secondary); }
</style>
