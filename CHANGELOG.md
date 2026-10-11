# Unreleased

## 变化

- 黑名单合并进 `db.sqlite`，不再单独使用 `blacklist.sqlite`；首次打开数据库时会自动导入旧文件并改名为 `blacklist.sqlite.bak`
- `rss_items` 表新增 `cid`、`savepath` 两列，记录每条连接离线时使用的参数
- 新增 `--retry-failed`：删除 115 上失败的离线任务后用 `db.sqlite` 里的 `cid`/`savepath` 重新提交，数据库里没有记录时使用 115 返回的 `wp_path_id`

## 修复

- 连接被对端断开（HTTP/2 的 `stream closed because of a broken pipe`、HTTP/1.1 的 `connection closed before message completed` 等）现在按瞬时错误处理：RSS 抓取会重试，重试仍失败时只跳过该 RSS 源；115 API 请求也会重试，不再因为一次断连中断整个任务

# v0.2.3

## 变化

- RSS 站点抓取从 `reqwest` 切换到 `wreq`
- RSS 重试策略增强，增加退避次数，并对 Cloudflare Worker 1101、HTTP 429、HTTP 5xx 等临时错误自动重试

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
