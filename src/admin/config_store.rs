//! 上游可改配置的持久化（Phase 2 实现）。
//!
//! 计划：`<data_dir>/upstreams.json` 存每上游覆盖项（enabled / backend / chat_host），
//! 启动时与 CLI/env 合并（文件优先）；`POST /admin/api/upstreams/reload` 重建上游实例。
