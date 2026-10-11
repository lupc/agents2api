import { createApp } from 'vue'
import { createRouter, createWebHistory } from 'vue-router'
import ElementPlus from 'element-plus'
import zhCn from 'element-plus/es/locale/lang/zh-cn'
import 'element-plus/dist/index.css'
import 'element-plus/theme-chalk/dark/css-vars.css'
import App from './App.vue'
import Dashboard from './views/Dashboard.vue'
import Upstreams from './views/Upstreams.vue'
import Models from './views/Models.vue'
import Speed from './views/Speed.vue'
import Checkin from './views/Checkin.vue'
import Credits from './views/Credits.vue'
import Logs from './views/Logs.vue'

const router = createRouter({
  history: createWebHistory('/admin/'),
  routes: [
    { path: '/', redirect: '/dashboard' },
    { path: '/dashboard', component: Dashboard, meta: { title: '汇总统计' } },
    { path: '/upstreams', component: Upstreams, meta: { title: '上游配置' } },
    { path: '/models', component: Models, meta: { title: '模型同步' } },
    { path: '/speed', component: Speed, meta: { title: '吞吐速度' } },
    { path: '/checkin', component: Checkin, meta: { title: '一键签到' } },
    { path: '/credits', component: Credits, meta: { title: '积分看板' } },
    { path: '/logs', component: Logs, meta: { title: '日志' } },
  ],
})

createApp(App).use(router).use(ElementPlus, { locale: zhCn }).mount('#app')
