<template>
  <div>
    <h2 class="page-title">日志</h2>
    <el-alert v-if="!file" type="info" :closable="false"
      title="未发现日志文件。用 --log-dir <目录> 启动后，此处会展示最新日志文件的尾部。" style="margin-bottom: 12px" />
    <el-card shadow="never">
      <template #header>
        <div class="card-head">
          <span class="dim">{{ file || '（无日志文件）' }}</span>
          <div>
            <el-radio-group v-model="lines" size="small">
              <el-radio-button :value="100">100 行</el-radio-button>
              <el-radio-button :value="500">500 行</el-radio-button>
              <el-radio-button :value="2000">2000 行</el-radio-button>
            </el-radio-group>
            <el-button size="small" style="margin-left: 8px" @click="load">刷新</el-button>
          </div>
        </div>
      </template>
      <pre class="logpre">{{ text }}</pre>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { api } from '../api'

const lines = ref(100)
const file = ref<string | null>(null)
const rawLines = ref<string[]>([])

const text = computed(() => rawLines.value.join('\n') || '（空）')

async function load() {
  const d = await api.logs(lines.value)
  file.value = d.file
  rawLines.value = d.lines
}

onMounted(load)
watch(lines, load)
</script>

<style scoped>
.card-head { display: flex; justify-content: space-between; align-items: center; }
.dim { color: var(--el-text-color-secondary); font-size: 12px; }
.logpre {
  background: var(--el-fill-color-light);
  padding: 12px;
  border-radius: 6px;
  font-size: 12px;
  line-height: 1.6;
  max-height: 60vh;
  overflow: auto;
  margin: 0;
  white-space: pre-wrap;
  word-break: break-all;
}
</style>
