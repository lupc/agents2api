//! Trae 一键签到（Phase 3 实现）。
//!
//! 计划：调 `https://api.trae.cn/trae/api/v2/ug/checkin_credits/{status,claim}`，
//! 头用 `authorization: Cloud-IDE-JWT <jwt>`；结果逐账号返回并落 `<data_dir>/checkin.json`。
