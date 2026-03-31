# v0.2.0

## 变化

- 115 离线接口切换为和 `github.com/SheltonZhu/115driver` 一致的加密 API
- 新增 `server` 子命令和 `POST /add` 接口
- `magnet` 和 HTTP `/add` 都支持 `savepath`
- `rss.json` 配置新增 `savepath`
- 新增 `--cookies`、`--no-cache`、`--chunk-delay`、`--chunk-size`、`--clear-task-type`
- 支持 `acgnx` 和 `rsshub`
- 重构 RSS 拉取和离线提交流程，单条坏数据不再导致整批 panic
- 请求层补充超时，数据库磁链判重逻辑修复
- GitHub Actions 增加 CI 校验，并调整 release 流程
