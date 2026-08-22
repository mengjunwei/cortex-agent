# MarkItDown MCP 安装指南（会话文档解析）

> [microsoft/markitdown](https://github.com/microsoft/markitdown) 官方 MCP 服务器，把 **Word / Excel / PPT / PDF / RTF / 纯文本 / HTML / JSON** 等文档转成 Markdown，喂给 AI 助手分析。官方包 `markitdown-mcp` 自带 **STDIO / Streamable HTTP / SSE** 三种传输，无需自建网关。
>
> **选型背景**：本项目会话上传文档后，由**后端**把文档字节编成 `data:` URI，编程式调用 markitdown 的 `convert_to_markdown` 工具，拿到 Markdown 文本注入对话（与图片内联同一路径，模型直接读到内容，不依赖工具调用回合）。markitdown-mcp 跑在**独立部署服务器**上，cortex-agent 通过 Streamable HTTP 连它。

---

## 一、它在本项目里怎么工作（先读这段）

1. 用户在会话里上传文档（≤20MB，前端校验后缀：pdf/doc/docx/xls/xlsx/ppt/pptx/csv/txt/md/rtf；后端支持面更广——`attachment.rs` 的 `DOC_EXTENSIONS` 还收 odt/ods/odp/markdown/html/htm/xml/json，前端 accept 只是子集）。
2. 文件落到对象存储，前端把 `{url, mime_type, filename}` 作为附件随消息发出。
3. 后端 `build_user_content`（`src/server/sse/attachment.rs`）识别到文档附件：
   - 用 reqwest 从对象存储 presigned URL 取回字节（后端与本机存储同网可达）；
   - 编码成 `data:{mime};base64,...` URI，调 markitdown 的 `convert_to_markdown`（按 slug `markitdown` 路由，`src/domain/mcp/manager.rs` 的 `call_tool_by_slug`）；
   - 拿到 Markdown 后，以 `<document filename="...">...</document>` 文本块注入用户消息。
4. markitdown **不必能访问对象存储**——字节是内联传过去的（presigned URL 的 host 是内网 `localhost:9000`，跨机的 markitdown 本来也拉不到）。所以 markitdown-mcp 只要被 cortex-agent 后端**单向可达**即可。

> 解析服务不可用时**降级不阻塞**：注入「已上传文档，但解析服务暂不可用」提示，模型仍知道有这份文档。

---

## 二、环境要求

| 项 | 要求 | 说明 |
|---|---|---|
| 操作系统 | Linux x86_64 / aarch64 | 纯 Python 服务，无图形界面依赖 |
| Python | ≥ 3.10（推荐 3.13） | uvx 可自动下载/管理指定版本，见 §三 |
| uv / uvx | 最新版 | 单文件安装，见 §三 |
| 网络 | 首次需联网拉 Python + 包；国内建议配镜像 | 见 §三镜像配置 |
| 监听端口 | 3001（可改） | 与 cortex 配置的 `endpoint` 一致 |

---

## 三、安装 uv / uvx

`uvx` 是 uv 自带的「一次性运行 Python 包」命令（类似 `npx`），不需要预先 `pip install`。以 root 为例：

```bash
# 官方一键脚本（装到 ~/.local/bin，含 uv 和 uvx）
curl -LsSf https://astral.sh/uv/install.sh | sh

# 让当前 shell 立即可用
source $HOME/.local/bin/env
hash -r
uvx --version    # 能打印版本即成功
```

**国内网络**：装包/拉 Python 慢或超时，配清华镜像（写进 `/etc/environment` 或 systemd unit 持久化）：

```bash
# PyPI 包镜像
export UV_DEFAULT_INDEX=https://pypi.tuna.tsinghua.edu.cn/simple
# uv 自动下载 Python 解释器的镜像（字节跳动的 python-build-standalone 镜像）
export UV_PYTHON_INSTALL_MIRROR=https://ghproxy.net/https://github.com/astral-sh/python-build-standalone/releases/download
```

> ⚠️ `curl` 必须带 `-L`/`-sSf` 中大写 `L`（astral.sh 会跳转）。装完若 `uvx: command not found`，是 PATH 没生效——`source $HOME/.local/bin/env` 或把 `$HOME/.local/bin` 加进 PATH 再 `hash -r`。

---

## 四、前台启动 + 验证

```bash
# --python 3.13：让 uvx 用 3.13 跑（未装则自动拉取）
# --http：启用 Streamable HTTP（与 SSE）传输，默认 STDIO
# --host 0.0.0.0：监听所有网卡（cortex 跨机连必须）
# --port 3001：监听端口
uvx --python 3.13 markitdown-mcp --http --host 0.0.0.0 --port 3001
```

首次启动会下载 `markitdown-mcp` 及依赖；看到 `Uvicorn running on http://0.0.0.0:3001`（或类似 `Application startup complete`）即成功。**Streamable HTTP 端点挂在 `/mcp`**，故完整 endpoint 是 `http://<服务器IP>:3001/mcp`。

探活（GET 非法属正常，返回 4xx/405 说明服务在线）：

```bash
ss -lntp | grep 3001
curl -i http://127.0.0.1:3001/mcp
```

### 冷启动预热（重要）

markitdown **第一次**转换时会加载 Magika（文件类型识别模型），耗时几秒到十几秒；之后常驻内存就快了。MCP 服务的工具超时**默认 60 秒**（数据库默认值），可能不够冷启动——需在「MCP 服务」界面把 markitdown 这条的超时改为 `120`（见 §六表格）。如果想预热掉冷启动，起服务后随便转一次：

```bash
# 用一个小文件触发首次加载（data URI 形式），之后正式请求就快了
curl -sX POST http://127.0.0.1:3001/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"convert_to_markdown",
                 "arguments":{"uri":"data:text/plain;base64,aGVsbG8="}}}'
```

返回里含 markdown 文本即转换链路通。

---

## 五、常驻运行

前台进程断线即失效，需要常驻。

### 5.1 后台（nohup，快速）

```bash
nohup uvx --python 3.13 markitdown-mcp --http --host 0.0.0.0 --port 3001 \
  > /var/log/markitdown-mcp.log 2>&1 &

ss -lntp | grep 3001
tail -f /var/log/markitdown-mcp.log
```

### 5.2 开机自启（systemd，生产推荐）

先确认 uvx 绝对路径：`which uvx`（通常是 `/root/.local/bin/uvx`）。

```bash
UVX=$(which uvx)
cat > /etc/systemd/system/markitdown-mcp.service <<EOF
[Unit]
Description=MarkItDown MCP Server
After=network.target

[Service]
Type=simple
ExecStart=${UVX} --python 3.13 markitdown-mcp --http --host 0.0.0.0 --port 3001
Environment=HOME=/root
Environment=PATH=/root/.local/bin:/usr/local/bin:/usr/bin:/bin
Environment=UV_DEFAULT_INDEX=https://pypi.tuna.tsinghua.edu.cn/simple
Environment=UV_PYTHON_INSTALL_MIRROR=https://ghproxy.net/https://github.com/astral-sh/python-build-standalone/releases/download
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now markitdown-mcp
systemctl status markitdown-mcp
```

> ⚠️ 易错点：
> - `ExecStart` 里 `uvx` **必须用绝对路径**（上面 `${UVX}` 已展开），否则 systemd 找不到命令。
> - `Environment=HOME` 和 `Environment=PATH` 缺一不可：uvx 要在 `$HOME` 下缓存包、靠 PATH 找自身依赖。改 unit 后必须 `systemctl daemon-reload && systemctl restart markitdown-mcp` 才生效。
> - unit 的 `Environment=` 行**不能写行内注释**（systemd 不剥离 `#`，会被当成值），注释只能单独成行。

---

## 六、接入 cortex-agent

服务起来后，在 cortex 的 **「MCP 服务」页界面手动新建** 一条 markitdown 服务（**不写进配置文件**——若配置了 `[[mcp.seeds]]`，每次启动会按 slug 做 `ON CONFLICT DO UPDATE`，覆盖 name/endpoint/args/transport/tool_timeout_secs 等字段（env/headers 不受影响），会把你在界面改的 endpoint 冲回配置值；界面方式更灵活。当前仓库配置未预置任何 seeds）：

| 字段 | 填什么 |
|---|---|
| **slug** | **`markitdown`**（必须完全一致——后端代码按此 slug 编程式调用 `convert_to_markdown`，差一个字都找不到） |
| name | 随意，如 `MarkItDown 文档解析` |
| transport | `streamable_http` |
| endpoint | `http://<markitdown服务器IP>:3001/mcp` |
| 超时（tool_timeout_secs） | 建议 `120`（首次冷启动加载 Magika 较慢） |

> **endpoint 三要素**：
> 1. 必须是**完整 URL**（带 `http://`），`transport=streamable_http` 校验要求如此；
> 2. 必须带 **`/mcp` 路径**——后端 `StreamableHttpClientTransportConfig::with_uri(endpoint)` 把它原样当请求地址，少了 `/mcp` 会握手失败；
> 3. host 用 **cortex-agent 后端能访问到的地址**（跨机用服务器 IP，不要写 `127.0.0.1`——那是 markitdown 本机）。

保存后「MCP 服务」页应显示 `markitdown` 且健康（绿色 + 工具数 1：`convert_to_markdown`）。注意：文档解析走的是后端编程式调用（`call_tool_by_slug`），**不需要**在助手编辑页勾选 markitdown——勾选与否不影响会话文档解析。

---

## 七、端到端验证

在任一会话上传一份 `.docx` / `.pdf` 并提问「总结这份文档」。模型能引用文档内容作答即全链路通。后端日志可见：

```
[attachment] markitdown 解析成功: N 字符
```

若见 `markitdown 解析失败（降级为仅提示）` 或 `MCP server 'markitdown' 未找到`，查 §八。

---

## 八、常见问题

| 现象 | 原因 / 解决 |
|---|---|
| 启动报 `No module named markitdown_mcp` / 装包失败 | uvx 首次需联网拉包；国内加 `UV_DEFAULT_INDEX=...清华镜像`（§三）后重试 |
| uvx 拉 Python 解释器卡住/超时 | 配 `UV_PYTHON_INSTALL_MIRROR`（§三）；或服务器预装 Python ≥3.10 后去掉 `--python 3.13` 让 uvx 用系统解释器 |
| `uvx: command not found`（systemd 尤甚） | `ExecStart` 未用绝对路径；或 `Environment=PATH` 未含 uvx 所在目录（`/root/.local/bin`） |
| cortex 日志 `MCP server 'markitdown' 未找到` | 界面没建这条 MCP 服务，或 **slug 不是 `markitdown`**（后端按此 slug 路由，必须完全一致）。去「MCP 服务」页按 §六 新建 |
| MCP 握手失败 / 连接超时 | ① endpoint 少了 `/mcp` 路径；② host 写成 `127.0.0.1` 而 markitdown 在另一台机器——改用可达 IP；③ 防火墙没放 3001 端口；④ 服务没起：`ss -lntp \| grep 3001` |
| `[attachment] markitdown 解析失败`（服务在跑） | 看 `markitdown-mcp` 自身日志 `/var/log/markitdown-mcp.log`；多为首次冷启动超时——已配 120s，仍超时说明首次加载 Magika 太慢，先按 §四预热一次 |
| 文档成功上传但模型说「看不到内容」 | 后端走的是降级路径。确认 markitdown 服务健康（「MCP 服务」页绿色），再看后端日志是 `解析失败` 还是 `字节获取失败`——前者是 markitdown，后者是 cortex 取不到对象存储字节（查对象存储连通） |
| 大文件（接近 20MB）转换慢/超时 | data URI 体积 ≈ 原文件 ×1.37，经 MCP 传输较大；上传上限是前端 20MB 硬编码 + 后端 `MAX_FETCH_BYTES`（改需动代码），可调大 `tool_timeout_secs` |
| 超大文档转换成功但内容被截断 | 后端对 markitdown 输出按 `MAX_MARKDOWN_CHARS`（20 万字符）截断；原始字节同时落盘到会话工作区 `uploads/` 可供工具读取 |

---

## 九、安全提示

markitdown-mcp 的 `--http` 模式**无内置鉴权**，且其 `convert_to_markdown` 对入参 `uri` 无 SSRF 校验（可拉任意 http/file）。本项目通过**只传受控上传字节的 data URI**（从不传用户任意 URL）规避了 SSRF；但服务端口本身仍裸奔：

- `--host 0.0.0.0` 切勿直接暴露公网；
- 远程使用：防火墙限定来源 IP 为 cortex-agent 服务器，或前置 nginx 加 Basic Auth（cortex 界面的 MCP 服务编辑支持 headers 字段携带 `Authorization` 头；注意 `[[mcp.seeds]]` 种子配置**不含** headers 字段，需装好后经界面补配）；
- 不要把 markitdown 服务对终端用户开放（任何人都能拿它当任意 URL 抓取代理）。
