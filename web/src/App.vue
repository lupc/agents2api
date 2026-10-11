<template>
  <el-container class="layout">
    <el-aside width="200px" class="aside">
      <div class="logo">agents2api</div>
      <el-menu :default-active="active" router class="menu">
        <el-menu-item index="/dashboard">
          <el-icon><Odometer /></el-icon><span>汇总统计</span>
        </el-menu-item>
        <el-menu-item index="/upstreams">
          <el-icon><Setting /></el-icon><span>上游配置</span>
        </el-menu-item>
        <el-menu-item index="/models">
          <el-icon><Cpu /></el-icon><span>模型同步</span>
        </el-menu-item>
        <el-menu-item index="/checkin">
          <el-icon><Calendar /></el-icon><span>一键签到</span>
        </el-menu-item>
        <el-menu-item index="/credits">
          <el-icon><Coin /></el-icon><span>积分看板</span>
        </el-menu-item>
        <el-menu-item index="/logs">
          <el-icon><Document /></el-icon><span>日志</span>
        </el-menu-item>
      </el-menu>
      <div class="foot">默认上游：{{ defaultUpstream || '…' }}</div>
    </el-aside>
    <el-main class="main">
      <router-view />
    </el-main>
  </el-container>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useRoute } from 'vue-router'
import { Odometer, Setting, Cpu, Calendar, Coin, Document } from '@element-plus/icons-vue'
import { api } from './api'

const route = useRoute()
const active = computed(() => route.path)
const defaultUpstream = ref('')

async function refresh() {
  try {
    const ov = await api.overview(1)
    defaultUpstream.value = ov.default_upstream
  } catch { /* 后端不可达时静默 */ }
}
watch(active, refresh, { immediate: true })
</script>

<style>
html, body, #app { height: 100%; margin: 0; }
body { background: var(--el-bg-color-page); }
.layout { height: 100%; }
.aside { border-right: 1px solid var(--el-border-color-light); display: flex; flex-direction: column; }
.logo { font-weight: 700; font-size: 16px; padding: 18px 20px; color: var(--el-color-primary); }
.menu { border-right: none; flex: 1; }
.foot { padding: 12px 20px; font-size: 12px; color: var(--el-text-color-secondary); }
.main { background: var(--el-bg-color-page); }
.page-title { font-size: 18px; font-weight: 600; margin: 0 0 16px; }
</style>
