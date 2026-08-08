# Playwright MCP 安装指南（麒麟 / 国产化 Linux，无图形界面）

> 浏览器自动化 MCP 服务器。在**无 GUI 的 Linux 服务器**上用 Playwright 驱动 headless Chromium，给 AI 助手提供「打开网页 / 点击 / 填表 / 截图 / 抓取内容 / 生成 PDF」等能力。官方仓库：[microsoft/playwright-mcp](https://github.com/microsoft/playwright-mcp)。
>
> **选型背景**：同类 [mcp-chrome](https://github.com/hangwin/mcp-chrome) 基于 Chrome 扩展，必须有图形界面 + 手动加载扩展，**不能用于 headless 服务器**；Playwright MCP 自带无头浏览器进程，正是服务器场景的方案。本项目接入后，工具以 `mcp__<slug>__<tool>` 命名空间注入助手。

---

## 一、环境要求

| 项 | 要求 | 说明 |
|---|---|---|
| 操作系统 | Linux x86_64 / aarch64 | 本文以麒麟 Kylin Server / RHEL 系为例 |
| 图形界面 | **不需要** | headless 运行 |
| Node.js | ≥ 20 | 麒麟自带通常过旧（v12/v16），见 §2 |
| Chromium | 由 Playwright 自动下载 | 需系统提供运行库，见 §3 |
| 网络 | 国内环境建议用 npmmirror 镜像 | 见各步命令 |

---

## 二、安装 Node.js 20（覆盖系统旧版）

麒麟自带的 Node 常是 v12 / v16，而 Playwright 要求 ≥ 20。最省事的方式：**下载预编译包覆盖系统 node**，不依赖 nvm、无需编译。

```bash
# 自动识别架构（x86_64 → x64，aarch64 → arm64）
ARCH=$(uname -m | sed 's/x86_64/x64/; s/aarch64/arm64/')

cd /usr/local
curl -L -o node20.tar.xz https://npmmirror.com/mirrors/node/v20.18.1/node-v20.18.1-linux-${ARCH}.tar.xz
tar -xf node20.tar.xz
ln -sf /usr/local/node-v20.18.1-linux-${ARCH}/bin/node /usr/local/bin/node
ln -sf /usr/local/node-v20.18.1-linux-${ARCH}/bin/npm  /usr/local/bin/npm
ln -sf /usr/local/node-v20.18.1-linux-${ARCH}/bin/npx  /usr/local/bin/npx

# 关键：清掉 bash 对旧 node 路径的缓存，并把新路径提到 PATH 最前
hash -r
export PATH=/usr/local/bin:$PATH
node -v   # 必须显示 v20.18.1
```

> ⚠️ 三个易错点：
> - `curl` **必须带 `-L`**——镜像会 302 跳转，不加会下到空页面。
> - `node -v` 若仍报旧路径（`/usr/bin/node: No such file or directory`），是 bash 哈希缓存没清，执行 `hash -r` 即可。
> - `npx` 随 Node 自带，**不要单独 `npm install npx`**。
>
> 想用更新的 Node 22 LTS：把 URL 里的 `v20.18.1` 换成镜像上对应的 v22 版本即可（Playwright 要求 ≥20）。

换国内 npm 源：

```bash
npm config set registry https://registry.npmmirror.com
```

---

## 三、安装 Chromium 系统依赖（yum 系）

Playwright 的 `--with-deps` 内部只调用 `apt-get`，**在麒麟（yum/dnf 系）上必然失败**（`sh: apt-get: command not found`）。因此：手动用 yum 装 Chromium 运行所需的共享库，下载浏览器时再去掉 `--with-deps`。

```bash
# 麒麟 / CentOS / RHEL（yum 或 dnf 均可）
yum install -y nss nspr atk at-spi2-atk cups-libs libdrm \
  libXcomposite libXdamage libXrandr mesa-libgbm pango alsa-lib libxshmfence
```

---

## 四、下载浏览器 + 启动 MCP

```bash
# 用国内镜像下载 chromium。先全局装 playwright，再用 node 调 cli（绕过 npx）
# ⚠️ 不要用 `npx -y playwright@latest install chromium`——在麒麟上会被系统 coreutils 的
#    `install` 命令截获，报 "missing destination file operand after 'chromium'"（见 §八）
npm i -g playwright@latest
PLAYWRIGHT_DOWNLOAD_HOST=https://cdn.npmmirror.com/binaries/playwright \
  node "$(npm root -g)/playwright/cli.js" install chromium

# @playwright/mcp 运行时要的是它自己 revision 的 chrome-for-testing（CFT），与上面的
# stable chromium 不通用——必须再装一次，否则 MCP 起来报 Browser "chrome-for-testing" is not installed
PLAYWRIGHT_DOWNLOAD_HOST=https://cdn.npmmirror.com/binaries/playwright \
  npx -y @playwright/mcp install-browser chrome-for-testing

# 前台启动验证
# --allowed-hosts '*' 允许用 IP / 跨机访问（默认只放行 localhost，见 §8）；仅本机访问可去掉
npx -y @playwright/mcp@latest --headless --browser chromium --no-sandbox --port 8931 --host 0.0.0.0 --allowed-hosts '*'
```

看到 `Listening on http://localhost:8931` 即成功。浏览器二进制落在 `~/.cache/ms-playwright/chromium-*`。

> ⚠️ 启动参数：
> - `--headless`：无界面服务器**必加**。
> - `--no-sandbox`：以 root / 容器身份运行 Chromium **必加**，否则崩溃。
> - `--host 0.0.0.0`：暴露到所有网卡（远程访问用）；仅本机访问去掉此参数（默认 localhost）。
> - `BEWARE: your OS is not officially supported` 仅为警告——麒麟会回退到 ubuntu 构建，**不影响使用**。

---

## 五、常驻运行

前台进程断线即失效，需要常驻。

### 5.1 后台（nohup，快速）

```bash
nohup npx -y @playwright/mcp@latest --headless --browser chromium --no-sandbox \
  --port 8931 --host 0.0.0.0 --allowed-hosts '*' > /var/log/playwright-mcp.log 2>&1 &

ss -lntp | grep 8931     # 确认监听
tail -f /var/log/playwright-mcp.log
```

### 5.2 开机自启（systemd，生产推荐）

```bash
cat > /etc/systemd/system/playwright-mcp.service <<'EOF'
[Unit]
Description=Playwright MCP Server
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/npx -y @playwright/mcp@latest --headless --browser chromium --no-sandbox --port 8931 --host 0.0.0.0 --image-responses allow
Environment=PATH=/usr/local/bin:/usr/bin:/bin
Environment=HOME=/root
Environment=PLAYWRIGHT_BROWSERS_PATH=/root/.cache/ms-playwright
Environment=PLAYWRIGHT_MCP_ALLOWED_HOSTS=*
Environment=PLAYWRIGHT_MCP_EXECUTABLE_PATH=/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome
Environment=PLAYWRIGHT_MCP_IMAGE_RESPONSES=allow
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now playwright-mcp
systemctl status playwright-mcp
```

> `ExecStart` 必须单行（终端粘贴折行会丢掉后半参数）；三行 `PLAYWRIGHT_MCP_*`（`ALLOWED_HOSTS` / `EXECUTABLE_PATH` / `IMAGE_RESPONSES`）缺一不可，改 unit 整段替换。`EXECUTABLE_PATH` 路径以实际为准：`find /root/.cache/ms-playwright -name chrome -type f`。
>
> systemd 里 `npx` **必须用绝对路径** `/usr/local/bin/npx`，并设 `Environment=PATH=...`（npx 内部要找 `node`）。
>
> ⚠️ **改完 unit 文件必须 `systemctl restart`**：`daemon-reload` 只是让 systemd 重读 unit 定义，**不会重启**已在运行的进程；`enable --now` 对已运行服务的 `start` 也是 no-op。所以改 ExecStart / Environment 后要用 `systemctl daemon-reload && systemctl restart playwright-mcp` 才会生效（用 `systemctl status` 看 CGroup 命令行是否带新参数可验证）。

---

## 六、接入 cortex-agent

服务起来后，把它注册成 cortex 的一个**外部 MCP**（HTTP 传输，工具以 `mcp__<slug>__<tool>` 注入助手）。cortex 的 `transport=2` 走 rmcp **streamable HTTP**，`endpoint` 即为完整 URL，与 Playwright MCP 的 `/mcp` 端点直接对接。

**方式 A：配置驱动**（`config/config.local.toml`，启动自动 upsert）：

```toml
[[mcp.seeds]]
slug = "browser"
name = "浏览器自动化(Playwright)"
transport = 2                       # 2 = streamable_http
endpoint = "http://localhost:8931/mcp"   # 必须是完整 URL
args = "[]"
# headers = '{"Authorization":"Basic xxx"}'   # 若前置了 nginx Basic Auth，在此填
```

**方式 B：界面驱动**：「MCP 服务」页 → 新建 MCP 服务 → 传输选 `streamable_http` → endpoint 填 `http://localhost:8931/mcp`。

注册后：重启 cortex → 「MCP 服务」页验证健康（绿色 + 工具数）→ 助手编辑页勾选 `browser` → 会话中即可调用浏览器工具。

> **通用 MCP 客户端**（CherryStudio / Claude Desktop / Cursor 等）配置相同：
> ```json
> { "mcpServers": { "playwright": { "url": "http://localhost:8931/mcp" } } }
> ```

---

## 七、验证

在任一已接入的客户端执行：`打开 https://example.com 并截图`，能返回截图即全链路通。或直接探活：

```bash
ss -lntp | grep 8931            # 端口在听
curl -i http://localhost:8931/mcp   # 返回 4xx/405 表示服务在线（GET 非法，属正常）
```

---

## 八、常见问题

| 现象 | 原因 / 解决 |
|---|---|
| `node -v` 仍是旧版 / `/usr/bin/node: No such file` | bash 哈希缓存未清，`hash -r` 后重试；确认 PATH 含 `/usr/local/bin` |
| `npm WARN EBADENGINE ... required node >=20` | Node 版本不足，回到 §2 装 Node 20 |
| `sh: apt-get: command not found` | 麒麟是 yum 系，`--with-deps` 不可用；按 §3 手动装依赖，§4 下载时去掉 `--with-deps` |
| `install: missing destination file operand after 'chromium'` | `npx -y playwright@latest install chromium` 在麒麟上被系统 coreutils 的 `install` 截获。改用 §4：`npm i -g playwright@latest` 后 `node "$(npm root -g)/playwright/cli.js" install chromium` |
| Chromium 下载卡住 / 超时 | 未设镜像，加 `PLAYWRIGHT_DOWNLOAD_HOST=https://cdn.npmmirror.com/binaries/playwright` |
| `error while loading shared libraries: libXxx.so` | 缺系统库，对照报错补 yum 包（常见 `mesa-libgbm` / `nss` / `pango` / `atk`） |
| Chromium 启动即崩（root 环境） | 漏 `--no-sandbox` |
| systemd 起不来 / `npx: command not found` | `ExecStart` 未用绝对路径，或 `Environment=PATH` 未含 `/usr/local/bin` |
| 接入 cortex 后 MCP 握手失败 | endpoint 不是完整 URL（必须 `http://...`）；或服务未监听对应端口 |
| 握手报 `403 ... Access is only allowed at localhost` | Playwright MCP 的 host 白名单（防 DNS rebinding），默认只放行 localhost。跨机 / 用 IP 访问需启动时加 `--allowed-hosts '*'`（或 env `PLAYWRIGHT_MCP_ALLOWED_HOSTS=*`）；同机访问则把 endpoint 改成 `http://127.0.0.1:8931/mcp` |
| 改了 unit 文件后服务仍用旧配置（`status` 命令行没新参数）| `daemon-reload` 只重载 unit 定义、**不重启**运行中的进程；`enable --now` 的 `start` 对已运行服务也是 no-op。必须 `systemctl restart playwright-mcp` 才会用新的 ExecStart / Environment |
| 工具报 `Browser "chrome-for-testing" is not installed` | 两类原因：① systemd 服务默认 `HOME=/`，找不到 `/root/.cache` 下的浏览器 → unit 加 `Environment=HOME=/root` / `PLAYWRIGHT_BROWSERS_PATH=/root/.cache/ms-playwright` 后 `daemon-reload && restart`；② 用 `npx playwright install` 装的浏览器与 `@playwright/mcp` 期望的 revision 不符 → 改用 MCP 自带命令装：`PLAYWRIGHT_BROWSERS_PATH=/root/.cache/ms-playwright PLAYWRIGHT_DOWNLOAD_HOST=https://cdn.npmmirror.com/binaries/playwright npx -y @playwright/mcp install-browser chrome-for-testing`。⚠️ unit 的 `Environment=` 行**不能写行内注释**（systemd 不剥离 `#`，会被当成值），注释必须单独成行 |
| `install-browser` 报 404 `NoSuchKey`（下载 `151.0.7922.x` 的 chrome 失败）| `@playwright/mcp@latest`（含全部 `0.0.x`）绑的是 **playwright alpha**，其浏览器 revision **旧镜像（npmmirror.com/mirrors/playwright）未收录**（新镜像 `cdn.npmmirror.com/binaries/playwright` 已收录大部分 CFT build，§4 已改用新镜像，通常可直接下）；仍 404 时只有官方 CDN（国内极慢）。最省事解法：复用 stable playwright 已下好的 chromium，用 `--executable-path` 指过去绕过版本检查——`CHROME=$(find /root/.cache/ms-playwright/chromium-1234 -name chrome -type f \| head -1)`，启动参数加 `--executable-path "$CHROME"`（等价 env `PLAYWRIGHT_MCP_EXECUTABLE_PATH`） |

| 截图成功但不在聊天界面内联显示（cortex 与 MCP 跨机器）| 跨机器不共享文件系统，截图必须以 **base64 走 MCP 协议**回来。两步：① MCP 端 ExecStart 加 `--image-responses allow`（**CLI flag，比 env 可靠**——此 alpha 可能没认 env）让它回传 base64 图片块；② cortex 已自动剥掉截图工具的 `filename` 参数（逼其内联 base64 而非只存盘）。cortex 收到 base64 后自动解码落盘 + 注入 `image_url` 显示（需 cortex 带 `manager.rs` 的 image 块保留 + filename 剥离代码）|

---

## 九、安全提示

`--host 0.0.0.0` 且**无内置鉴权**，切勿直接暴露公网。远程使用建议：防火墙限定来源 IP，或前置 nginx 加 Basic Auth（cortex 的 `[[mcp.seeds]]` 支持 `headers` 字段携带鉴权头）。
