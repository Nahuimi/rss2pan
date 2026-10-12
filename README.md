# Rss2pan

将 RSS 订阅离线下载到 115 网盘。

## 关于

这是 `rss2cloud` 的 Rust 实现，离线接口已切换到和 `github.com/SheltonZhu/115driver` 同一套加密 API。

支持 RSS 源: nyaa, dmhy, mikanani, acgnx, rsshub

RSS 站点抓取已从 `reqwest` 切换到 `wreq`，并针对 Cloudflare Worker 1101 等临时错误增强了重试逻辑。

<details>
<summary><code><strong>「 点击查看 实现功能 」</strong></code></summary>

- [x] 115 离线功能
- [x] sqlite 存储数据
- [x] 黑名单存储与自动过期（和 RSS 数据共用 `db.sqlite`）
- [x] 实现 cli
- [x] `/add` HTTP API
- [x] `savepath` 支持
- [x] `--cookies`
- [x] `-q --qrcode`
- [x] `--qrcode-app`
- [x] `--no-cache`
- [x] `--clear-task-type`
- [x] `--retry-failed` 重试失败的离线任务
- [x] `--chunk-delay`
- [x] `--chunk-size`
- [x] `config.toml` 代理 / 站点 / cookie 配置
- [x] 正则过滤 filter
- [ ] Windows 定时任务
- [x] 不同网站的并发任务
- [x] 指定 magnet 链接或者文件，离线到 115

</details>

## 用法

在同一目录下面准备 `rss.json`。首次运行时，如果当前目录没有 `config.toml`，程序会自动生成一份默认配置。

`.cookies` 已加入 `.gitignore`，不要把真实 cookie 提交到 Git 仓库。

在命令行运行 `rss2pan`

`.cookies` 文件内容是 115 的 cookie 字符串。手动从浏览器复制或者使用 [gcookie](https://github.com/zhifengle/gcookie)

```bat
REM 使用 gcookie 读取浏览器的 cookie
gcookie.exe 115.com > .cookies
```

```bash
# 查看帮助
rss2pan -h
# 直接运行。读取 rss.json，依次添加离线任务
rss2pan
# 并发请求 rss 网站。然后再添加 115 离线任务
rss2pan -m
# 使用 cookies
rss2pan --cookies "UID=xxx;CID=xxx;SEID=xxx;KID=xxx"
# 使用二维码登录。成功后会写入 config.toml
rss2pan -q
# 指定二维码登录端
rss2pan -q --qrcode-app android
rss2pan -q --qrcode-app ios
rss2pan -q --qrcode-app 115android
# 跳过 db.sqlite 缓存检查
rss2pan --no-cache
# 调整分块大小和间隔
rss2pan --chunk-size 100 --chunk-delay 3

# 指定 rss 配置文件路径；优先级高于 config.toml 里的 [paths].rss
rss2pan --rss custom-rss.json

# 指定 rss URL 离线下载
# 如果 rss.json 存在这条 url 的配置，会读取配置。没有配置，默认离线到 115 的默认目录
rss2pan -u "https://mikanani.me/RSS/Bangumi?bangumiId=2739&subgroupid=12"
# --clear-task-type 清除离线任务。1: 已完成的 2: 所有任务 3: 失败任务 4: 运行的任务 5: 完成并删除的任务 6: 所有的任务
rss2pan --clear-task-type 1
# 重试失败的离线任务：先用 115 官方重试接口，不行再删除后用 db.sqlite 里的 cid/savepath 重新提交
rss2pan --retry-failed

# 查看 magnet 子命令帮助
rss2pan magnet -h
rss2pan magnet --link "magnet:?xt=urn:btih:12345" --cid "12345" --savepath "番剧/测试"
# 离线包含 magnet 的 txt 文件; 按行分割
rss2pan magnet --txt magnet.txt --cid "12345" --savepath "番剧/测试"

# 服务模式
rss2pan server
curl -H "Content-Type: application/json" -d "{\"tasks\":[\"magnet:?xt=urn:btih:xx\"],\"cid\":\"12345\",\"savepath\":\"番剧/测试\"}" -X POST http://localhost:8115/add
```

### 注意

日志报 `115 abnormal operation` 时，说明账号触发了异常验证，需要在浏览器端手动离线，输入验证码后解除。

#### 二维码登录端

`--qrcode-app` 默认是 `tv`

- `web`: 网页版
- `android`: 115生活(Android端)
- `115android`: 115(Android端)
- `ios`: 115生活(iOS端)
- `115ipad`: 115(iPad端)
- `tv`: 115网盘(Android电视端)
- `alipaymini`: 115生活(支付宝小程序)
- `wechatmini`: 115生活(微信小程序)
- `qandroid`: 115管理(Android端)
- `115ios`: 115(iOS端)

## 配置

### rss.json

`rss.json` 现在是一个数组，不再按域名分组。程序会按 `url` 直接匹配对应配置。

<details>
<summary><code><strong>「 点击查看 配置文件 rss.json 」</strong></code></summary>

```json
[
  {
    "name": "test",
    "filter": "/简体|1080p/",
    "url": "https://mikanani.me/RSS/Bangumi?bangumiId=2739&subgroupid=12"
  },
  {
    "name": "VCB-Studio",
    "cid": "2479224057885794455",
    "savepath": "VCB-Studio",
    "url": "https://nyaa.si/?page=rss&u=VCB-Studio"
  },
  {
    "name": "hikiko123",
    "cid": "2479224057885794455",
    "url": "https://sukebei.nyaa.si/?page=rss&u=hikiko123"
  },
  {
    "name": "水星的魔女",
    "filter": "简日双语",
    "cid": "2479224057885794455",
    "savepath": "番剧/水星的魔女",
    "url": "https://share.dmhy.org/topics/rss/rss.xml?keyword=%E6%B0%B4%E6%98%9F%E7%9A%84%E9%AD%94%E5%A5%B3&sort_id=2&team_id=0&order=date-desc"
  }
]
```

</details>

配置了 `filter` 后，标题包含该文字的会被离线。不设置 `filter` 默认离线全部。

`/简体|\\d{3,4}[pP]/` 使用斜线包裹的正则规则。注意转义规则。

`cid` 是离线到指定文件夹的 id。

`savepath` 是可选项，直接作为 `add_task_urls` 请求体里的 `savepath` 字段提交给 115；不设置时保持 115 默认行为。

提交成功后，程序会把这条连接的 `magnet` 以及本次使用的 `cid`、`savepath` 一起写进 `db.sqlite` 的 `rss_items` 表。`--retry-failed` 重试失败任务时会优先使用这里保存的参数。

获取方法: 浏览器打开 115 的文件，地址栏像 `https://115.com/?cid=2479224057885794455&offset=0&tab=&mode=wangpan`

> 其中 `2479224057885794455` 就是 `cid`

### config.toml

`config.toml` 用来统一管理代理、路径、日志、镜像站模板和 cookie。

<details>
<summary><code><strong>「 点击查看 配置文件 config.toml 」</strong></code></summary>

```toml
[proxy]
address = "http://127.0.0.1:10808"

[paths]
database = "db.sqlite"
rss = "rss.json"

[log]
level = "info"

[blacklist]
exclude = ""
retention_months = 6

[cookies]
"115.com" = ""

[template.mikanani]
domains = ["mikanani.me", "mikanime.tv"]
proxy = ["mikanani.me"]

[template.nyaa]
domains = ["nyaa.si", "sukebei.nyaa.si"]
proxy = ["nyaa.si", "sukebei.nyaa.si"]

[template.dmhy]
domains = ["share.dmhy.org"]
proxy = ["share.dmhy.org"]

[template.acgnx]
domains = ["share.acgnx.se", "www.acgnx.se", "share.acgnx.net"]

[template.rsshub]
domains = ["rsshub.app"]
```

</details>

#### 路径配置

- `[paths].database`：sqlite 数据库文件路径，默认是 `db.sqlite`。RSS 记录、站点状态和黑名单都在这个文件里。
- `[paths].rss`：默认 RSS 配置文件路径，默认是 `rss.json`。
- CLI `--rss` 的优先级高于 `[paths].rss`。
- 如果你想把数据库或 RSS 配置放到其他目录，可以直接改成相对路径或绝对路径。
- macOS / Linux 可以直接写成：`"/Users/name/rss2pan/db.sqlite"`、`"/home/name/rss2pan/rss.json"`。
- Windows 推荐写成：`'D:\ruanjian\rss2pan\db.sqlite'`，或者 `"D:/ruanjian/rss2pan/db.sqlite"`。
- 为了兼容常见写法，`[paths]` 里也接受 `"D:\ruanjian\rss2pan\db.sqlite"` 这种未转义反斜杠路径。

#### 从 `blacklist.sqlite` 迁移

- 旧版本用单独的 `blacklist.sqlite` 保存黑名单。现在黑名单表 `rss_blacklist` 和 RSS 数据一起放在 `[paths].database` 里。
- 第一次打开数据库时，如果发现旧的 `blacklist.sqlite`（`[paths].blacklist` 指定的路径，或者当前目录下的 `blacklist.sqlite`），程序会把里面的黑名单记录导入 `db.sqlite`，然后把旧文件改名为 `blacklist.sqlite.bak`。
- 导入完成后 `[paths].blacklist` 这个键就不再需要，程序回写 `config.toml` 时也不会再写入它。
- 如果旧文件不是有效的 sqlite 数据库，程序只会打印一条警告并跳过，不会删除它。

#### RSS 黑名单

- `[blacklist].exclude`：和 `rss.json` 里的 `filter` 规则一致。普通字符串按标题包含匹配；`/regex/` 按正则匹配。
- `[blacklist].retention_months`：黑名单保留月数，默认 `6`。程序启动时会自动删除更早的黑名单记录。
- 当某个 RSS 源里出现匹配 `exclude` 的标题，并且当前 RSS 处理成功后，程序会把这条记录的 `link`、`title`、`createdAt`、`updatedAt` 以及 RSS 链接写入 `db.sqlite` 的 `rss_blacklist` 表。
- 黑名单按 RSS 链接去重；同一条 RSS 下一次运行会直接跳过，不再抓取和提交。

例如：

```toml
[blacklist]
exclude = "/(?i)(合集|fin|end)/"
retention_months = 6
```

上面的配置适合把出现 `合集`、`fin`、`end` 的 RSS 源视为完结源，后续自动跳过。

#### 重试失败的离线任务

`--retry-failed` 用来重试 115 上状态是失败（`status = -1`）的离线任务：

1. 分页拉取 115 离线任务列表，筛出失败任务，按 `info_hash` 去重。
2. 先用 115 官方的重试接口（`https://clouddownload.115.com/?ac=restart`，参数 `info_hash`）逐个重试；能重新开始下载的任务不用删除。
3. 等几秒再拉一次任务列表，把“重试接口报错”和“重试后仍然是失败状态”的任务挑出来，退回删除 + 重新提交：
   - 先用 `task_del` 逐个删除这些任务（`flag=0`，不删除已下载的文件），每次请求只带一个 `hash`，删完等几秒再提交，避免 115 还没处理完删除就报“任务已存在”。
   - 连接在 `db.sqlite` 的 `rss_items` 里有记录时，使用记录里的 `cid` 和 `savepath`（记录里没有 `savepath` 就不传；记录里没有 `cid` 时退回用 115 返回的 `wp_path_id`）。
   - 数据库里没有记录时，使用 115 任务列表返回的 `wp_path_id` 作为 `cid`，不带 `savepath`。
   - 如果 115 仍然返回“任务已存在”，会隔几秒重试两次；还是存在就只记一条警告并计入 `already existed`，不再算提交失败。
4. 同一组 `cid` / `savepath` 的连接会合并成一次 `add_task_urls` 请求，分块大小和间隔仍然由 `--chunk-size`、`--chunk-delay` 控制。

```bash
# 重试失败的离线任务
rss2pan --retry-failed
# 控制分块和间隔
rss2pan --retry-failed --chunk-size 50 --chunk-delay 3
```

- `--retry-failed` 和 `--clear-task-type` 不能同时使用。
- 结束后会打印失败任务数、用重试接口重启的任务数、删除数、重新提交数（分别来自 `db.sqlite` 和 115 API）、115 仍认为存在的任务数以及提交失败数；有提交失败时退出码是 `1`。
- 如果 `already existed` 不为 0，说明 115 还在用它自己的“重复链接”校验挡住这些连接，稍后再跑一次 `--retry-failed`，或者先用 `--clear-task-type 3` 清掉 115 里的失败任务（对应 `task_clear` 的 `flag=2`）。
- 数据库里没有记录的重试**不会**写回 `db.sqlite`。

#### 代理配置

- `proxy.address` 支持 `http://`、`https://`、`socks5://` 等常见代理地址。
- 每个模板里的 `proxy` 用来列出哪些域名走代理；未列出的域名默认直连。
- `proxy` 里的域名必须已经出现在同一个模板的 `domains` 里。

#### 镜像站 / 模板共享

`template.<parser>` 的 `<parser>` 是固定值，直接表示使用哪个 RSS 解析模板。

真正影响行为的是：

- `template.<parser>`：选择解析器
- `domains`：这个模板覆盖哪些域名
- `proxy`：这些域名里哪些走代理

默认配置里：

- `mikanime.tv` 复用 `mikanani` parser
- `sukebei.nyaa.si` 复用 `nyaa` parser

如果你要手动加入新的镜像站，比如 `mikanani.kas.pub`，可以这样写：

```toml
[template.mikanani]
domains = ["mikanani.me", "mikanime.tv", "mikanani.kas.pub"]
proxy = ["mikanani.me"]
```

`mikanani` 系列站点需要 `Referer`，程序会根据当前请求域名自动生成，不需要手动再写 headers。

#### RSS 抓取和重试

- RSS 请求现在使用 `wreq`。
- 默认会对超时、连接失败、HTTP 408、HTTP 429、HTTP 5xx，以及包含 Cloudflare Worker 1101 特征的返回内容自动重试。
- 重试退避会比旧版更长，降低站点临时异常时的失败率。

#### 兼容性说明

旧版 `[sites."host"]` / `proxy.domains` 配置已经不再支持，需要改成新的 `[template.<parser>]` 结构。

旧版 `template.<name>.parser` / `template.<name>.rss_key` 也不再支持。

旧版按 host 分组的 `rss.json` 也不再支持，需要改成数组结构。

如果同一个域名出现在多个模板里，或者 `proxy` 里写了不在 `domains` 里的域名，程序会直接报错。

#### cookie 优先级

115 cookie 的优先级如下：

1. `--cookies`
2. `config.toml`
3. `.cookies`

支持下面两种格式，程序会自动规范化：

```text
UID=115; CID=a1e; SEID=37d; KID=40b
UID=115;CID=a1e;SEID=37d;KID=40b;
```

二维码登录成功后，会把最新的 115 cookie 写回 `config.toml`。

如果 `config.toml` 可能会被提交，务必只保留示例值；实际使用时也可以继续把真实 cookie 放在 `.cookies` 里。

#### 日志级别

可以通过 `[log].level` 或环境变量 `RUST_LOG` 控制日志输出。

支持的级别：

- `off`：关闭日志输出。
- `error`：只显示错误，适合不想看普通日志时使用。
- `warn`：显示错误和警告。
- `info`：默认级别，显示主要运行信息。
- `debug`：显示更详细的调试信息。
- `trace`：显示最详细的跟踪信息。

优先级如下：

1. 环境变量 `RUST_LOG`
2. `config.toml` 里的 `[log].level`
3. 默认值 `info`

### 日志环境变量

不想看日志时：

```bat
set RUST_LOG=error
```

Linux / macOS:

```bash
export RUST_LOG=error
```

如果你更喜欢写在配置文件里，也可以：

```toml
[log]
level = "error"
```
