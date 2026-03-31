# Rss2pan

将 RSS 订阅离线下载到 115 网盘。

## 关于

这是 `rss2cloud` 的 Rust 实现，离线接口已切换到和 `github.com/SheltonZhu/115driver` 同一套加密 API。

支持 RSS 源: nyaa, dmhy, mikanani, acgnx, rsshub

<details>
<summary><code><strong>「 点击查看 实现功能 」</strong></code></summary>

- [x] 115 离线功能
- [x] sqlite 存储数据
- [x] 实现 cli
- [x] `/add` HTTP API
- [x] `savepath` 支持
- [x] `--cookies`
- [x] `--no-cache`
- [x] `--clear-task-type`
- [x] `--chunk-delay`
- [x] `--chunk-size`
- [x] proxy 配置
  - 读取 ALL_PROXY 或者 HTTPS_PROXY 环境变量
- [x] 正则过滤 filter
- [ ] Windows 定时任务
- [x] 不同网站的并发任务
- [x] 指定 magnet 链接或者文件，离线到 115

</details>

## 用法

在同一目录下面，配置好 `rss.json` 、 `node-site-config.json` 和 `.cookies`

`.cookies` 已加入 `.gitignore`，不要把真实 cookie 提交到 Git 仓库。

在命令行运行 `rss2pan`

`.cookies` 文件内容是115的cookie字符串。手动从浏览器复制或者使用 [gcookie](https://github.com/zhifengle/gcookie)

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
# 跳过 db.sqlite 缓存检查
rss2pan --no-cache
# 调整分块大小和间隔
rss2pan --chunk-size 100 --chunk-delay 3

# 指定 rss URL 离线下载
# 如果 rss.json 存在这条url 的配置，会读取配置。没有配置，默认离线到 115 的默认目录
rss2pan -u "https://mikanani.me/RSS/Bangumi?bangumiId=2739&subgroupid=12"
# 清理 115 离线任务。1-6 对齐 rss2cloud
rss2pan --clear-task-type 1

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

## 配置

<details>
<summary><code><strong>「 点击查看 配置文件 rss.json 」</strong></code></summary>

```json
{
  "mikanani.me": [
    {
      "name": "test",
      "filter": "/简体|1080p/",
      "url": "https://mikanani.me/RSS/Bangumi?bangumiId=2739&subgroupid=12"
    }
  ],
  "nyaa.si": [
    {
      "name": "VCB-Studio",
      "cid": "2479224057885794455",
      "savepath": "VCB-Studio",
      "url": "https://nyaa.si/?page=rss&u=VCB-Studio"
    }
  ],
  "sukebei.nyaa.si": [
    {
      "name": "hikiko123",
      "cid": "2479224057885794455",
      "url": "https://sukebei.nyaa.si/?page=rss&u=hikiko123"
    }
  ],
  "share.dmhy.org": [
    {
      "name": "水星的魔女",
      "filter": "简日双语",
      "cid": "2479224057885794455",
      "savepath": "番剧/水星的魔女",
      "url": "https://share.dmhy.org/topics/rss/rss.xml?keyword=%E6%B0%B4%E6%98%9F%E7%9A%84%E9%AD%94%E5%A5%B3&sort_id=2&team_id=0&order=date-desc"
    }
  ]
}
```

</details>

配置了 `filter` 后，标题包含该文字的会被离线。不设置 `filter` 默认离线全部

`/简体|\\d{3,4}[pP]/` 使用斜线包裹的正则规则。注意转义规则

cid 是离线到指定的文件夹的 id 。

savepath 是可选项，直接作为 `add_task_urls` 请求体里的 `savepath` 字段提交给 115；不设置时保持 115 默认行为。

获取方法: 浏览器打开 115 的文件，地址栏像 `https://115.com/?cid=2479224057885794455&offset=0&tab=&mode=wangpan`

> 其中 2479224057885794455 就是 cid

<details>
<summary><code><strong>「 点击查看 node-site-config.json 配置 」</strong></code></summary>

配置示例。 设置 【httpsAgent】 表示使用代理连接对应网站。不想使用代理删除对应的配置。

```json
{
  "share.dmhy.org": {
    "httpsAgent": "httpsAgent"
  },
  "nyaa.si": {
    "httpsAgent": "httpsAgent"
  },
  "sukebei.nyaa.si": {
    "httpsAgent": "httpsAgent"
  },
  "mikanime.tv": {
    "headers": {
      "Referer": "https://mikanime.tv/"
    }
  },
  "mikanani.me": {
    "httpsAgent": "httpsAgent",
    "headers": {
      "Referer": "https://mikanani.me/"
    }
  }
}
```

</details>

#### cookie 配置

在 `node-site-config.json` 文件里面配置 115.com cookie。
如果这个文件会被提交，务必只保留示例值；实际使用更推荐 `.cookies` 或 `--cookies`。

```json
{
  "115.com": {
    "headers": {
      "cookie": "yourcookie"
    }
  }
}
```

### proxy 配置

设置【httpsAgent】会使用代理。默认使用的地址 `http://127.0.0.1:10808`。

> 【httpsAgent】沿用的 node 版的配置。

需要自定义代理时，在命令行设置 Windows: set ALL_PROXY=http://youraddr:port

> Linux: export ALL_PROXY=http://youraddr:port

```batch
@ECHO off
SETLOCAL
CALL :find_dp0
REM set ALL_PROXY=http://youraddr:port
rss2pan.exe  %*
ENDLOCAL
EXIT /b %errorlevel%
:find_dp0
SET dp0=%~dp0
EXIT /b
```

把上面的 batch 例子改成自己的代理地址。另存为 rss2pan.cmd 和 rss2pan.exe 放在一个目录下面。

在命令行运行 rss2pan.cmd 就能够使用自己的代理的了。

### 日志的环境变量

不想看日志时，Windows: set RUST_LOG=error
