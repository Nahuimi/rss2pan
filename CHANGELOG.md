# v0.2.3

## 变化

- RSS 拉取保留 `reqwest` 主路径，并内置 `libcurl` 兼容兜底；遇到特定 Cloudflare Worker 1101 错误时会自动回退，不再依赖系统 `curl`
- 恢复 `mikanani` 系列站点在 `reqwest` 路径上的自动 `Referer`，和既有行为保持一致

# v0.2.2

## 变化

- `config.toml` 新增 `[paths]` 和 `[log]` 配置，支持自定义 `db.sqlite`、`rss.json` 路径和默认日志级别
- 自动生成 / 回写 `config.toml` 时，模板数组改为单行输出，和 README 示例保持一致
- `[paths]` 兼容 macOS、Linux、Windows 路径写法，Windows 未转义反斜杠路径也可直接读取

# v0.2.1

## 变化

- 用 `config.toml` 替代 `node-site-config.json`，统一管理代理、站点模板和 cookie
- 首次运行会自动生成默认 `config.toml`，支持按域名配置代理和镜像站共享模板
- 115 cookie 优先级调整为 `--cookies` > `config.toml` > `.cookies`，并兼容更宽松的 cookie 字符串格式
- 二维码登录成功后会把最新 cookie 回写到 `config.toml`
- `rss.json` 改为扁平数组结构，按 URL 直接匹配配置
- `mikanani` 系列站点会自动生成 `Referer`，简化模板配置
- 更新 README 和示例配置，移除旧版配置格式说明

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
