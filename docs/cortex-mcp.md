# cortex-mcp —— cortex-agent 内置 MCP 工具二进制

> `crates/cortex-mcp` —— 跟随 cortex-agent 一起编译的 **stdio MCP 工具集**，按模块组织、可扩展。
> 当前内置 **邮件发送**（`send_email`）、**只读数据库查询**（`db_query` / `db_schema` / `db_sample` / `db_explain`，
> nyetdb v0.3.1 移植版，见 §十二）、**InfluxDB 时序查询**（`influx_query` / `influx_schema`，v2/v3，见 §十三）
> 与 **Prometheus 查询**（`prom_query` / `prom_schema`，见 §十四），
> 以后会陆续加别的工具（都进这一个二进制）。
>
> **定位**：cortex-agent 自带的「官方工具箱」。会话里需要发邮件这类**外部副作用**能力时，
> 由助手（LLM 代理）自主调用工具完成；凭证经环境变量注入、加密存储，**不进 LLM 上下文**。

---

## 一、它在本项目里怎么工作（先读这段）

1. `cortex-mcp` 是 cargo workspace 的一个成员 crate（`crates/cortex-mcp/`），和主 crate 一起编译，
   产出单二进制 `target/release/cortex-mcp`。
2. cortex 的 `McpManager`（`src/domain/mcp/transport.rs::connect_stdio`）把它当**本地 stdio 子进程**拉起，
   通过 `cmd.env(k, v)` 把 `SMTP_*` 凭证注入子进程环境。
3. 在「助手编辑页」勾选启用某条 cortex-mcp 服务后，其工具以 `mcp__<slug>__send_email` 命名空间注入助手。
4. 会话中助手自主调用 `mcp__<slug>__send_email` 发信。

> **与 markitdown 的关键区别**：markitdown 是后端**编程式**调用（slug 硬编码 `markitdown`，不进工具回合）；
> cortex-mcp 是 **LLM 代理驱动**（slug 任意、语义化即可，**必须在助手编辑页勾选启用**）。

> **为什么自建而不是用外部 mcp-email**：外部 `zavora-ai/mcp-email` 是 24 工具 + 5 发后端 + 3 读后端的「全家桶」，
> 且其手搓 SMTP 把 rustls「服务器关连接未发 close_notify」误报为发送失败（QQ 企业邮/163 极常见）。
> cortex-mcp 只做发送、基于成熟的 [`lettre`](https://crates.io/crates/lettre)，彻底规避该问题，并可作为后续工具的统一载体。

---

## 二、两个硬约束（决定部署形态）

| 约束 | 影响 |
|---|---|
| **① 只支持 stdio 传输** | `cortex-mcp` 二进制**必须和 cortex-agent 部署在同一台机器**，由 cortex 以本地子进程拉起。不能跨机 HTTP。容器部署需把二进制放进容器且 PATH 可达。 |
| **② 单进程单账号（邮件）** | 「多发件账号」= **每个账号起一个独立 `cortex-mcp` 子进程** = 一条独立 MCP 服务，各自独立 env，凭证互相隔离。 |

> 子进程由 cortex 管理生命周期（随 cortex 启停），**不需要 systemd 常驻**。

---

## 三、构建

`cortex-mcp` 是 workspace 成员，在仓库根目录构建：

```bash
# 方式 A：只编 cortex-mcp（快）
cargo build --release -p cortex-mcp
# 产物：target/release/cortex-mcp

# 方式 B：连同主 crate 一起编（部署 cortex 时自然带上）
cargo build --release
```

> 容器部署：把 `target/release/cortex-mcp` 拷进镜像（与 cortex 主二进制同目录即可），确保 cortex 进程对其有执行权限（`chmod +x`）。

---

## 四、接入 cortex-agent（多账号）

**每个发件邮箱新建一条 stdio MCP 服务**，界面手加（**不写 `[[mcp.seeds]]`**——seed 不含 env 字段，且每次启动按 slug 做 `ON CONFLICT DO UPDATE` 覆盖 name/endpoint/args/transport/超时（env/headers 不受影响），会把界面改的 endpoint 冲回配置值；界面方式才省心）。

### 4.1 MCP 服务页字段

以「销售邮箱」为例：

| 字段 | 填什么 |
|---|---|
| name | `邮件-销售`（可读名，列表展示用） |
| **slug** | **`email-sales`**（语义化即可，**无固定值**；模型靠它区分账号） |
| transport | **stdio（本地子进程）** |
| endpoint | **`cortex-mcp` 二进制绝对路径**，如 `/opt/cortex/target/release/cortex-mcp` |
| args | `[]`（无启动参数） |
| env | 见 §4.2，该账号的 SMTP 凭证键值对（加密存储，只设不显） |
| 超时（tool_timeout_secs） | `60`（发信含网络往返 + 附件上传） |

保存 → 列表点「探活」应显示**绿色 + 工具数 9**。

> 任何 cortex-mcp 服务（无论配了哪组 env）都在 tools/list 里暴露**全部 9 个工具**
> （`#[tool_router]` 静态注册，与 env 无关）。未配置对应 env 的工具**调用时**才返回
> 英文未配置提示（如 `Email tool not configured: set SMTP_* ...`），不会从清单里隐藏——
> 探活显示的工具数恒为 9，属正常。

### 4.2 env（SMTP，只发送）

```
SMTP_HOST=smtp.exmail.qq.com
SMTP_PORT=465
SMTP_USERNAME=sales@你的域名.com
SMTP_PASSWORD=<app-password 或 客户端专用密码>
SMTP_FROM=sales@你的域名.com
```

| 变量 | 必填 | 说明 |
|---|---|---|
| `SMTP_HOST` | ✅ | SMTP 服务器地址 |
| `SMTP_USERNAME` | ✅ | 发件账号 |
| `SMTP_PASSWORD` | ✅ | **授权码 / app-password**（不是账号主密码） |
| `SMTP_PORT` | ❌ | 默认 `465`。`465`=隐式 TLS；`587`=STARTTLS |
| `SMTP_FROM` | ❌ | 默认 = `SMTP_USERNAME` |

> 任一必填项缺失 → 工具不崩溃，调用 `send_email` 时返回「未配置：缺少 SMTP_* 环境变量」提示。

> ⚠️ **不要把真实密码贴进对话**——env 值只在 cortex UI 的 env 输入框里填，后端经 AesCodec 加密落库（`env_enc`），列表只回脱敏值（`****abcd`）。

### 4.3 常见邮箱商 SMTP host / port

| 邮箱商 | SMTP_HOST | SMTP_PORT | 备注 |
|---|---|---|---|
| 腾讯企业邮 | `smtp.exmail.qq.com` | 465 / 587 | 用客户端专用密码 |
| QQ 个人邮箱 | `smtp.qq.com` | 465 / 587 | 必须用授权码 |
| 网易 163 | `smtp.163.com` | 465 / 994 | 用授权码 |
| Gmail | `smtp.gmail.com` | 465 / 587 | **必须用 [app-password](https://myaccount.google.com/apppasswords)** + 两步验证 |
| Outlook / 365 | `smtp.office365.com` | 587 | 用账号密码或 app-password |
| Zoho | `smtp.zoho.com` | 465 / 587 | 用应用专用密码 |
| 阿里企业邮 | `smtp.qiye.aliyun.com` | 465 | 用客户端密码 |

---

## 五、`send_email` 工具

本节描述 `send_email`。注意二进制同时暴露其余 8 个工具（数据库 / InfluxDB / Prometheus，
见 §十二～§十四）——未配置相应 env 时调用返回未配置提示，不影响邮件功能。参数（JSON Schema 自动暴露给模型）：

| 参数 | 必填 | 说明 |
|---|---|---|
| `to` | ✅ | 收件人，多个用英文逗号分隔（`a@x.com, b@y.com`） |
| `subject` | ✅ | 主题 |
| `body` | ✅ | 纯文本正文 |
| `html` | ❌ | HTML 正文（同时给时客户端按偏好渲染 HTML，纯文本作回退） |
| `cc` | ❌ | 抄送，逗号分隔 |
| `bcc` | ❌ | 密送，逗号分隔 |
| `attachments` | ❌ | 附件**绝对路径**列表（运行机可达的本地文件，如 workspace 里的产物） |

返回（模型可见串一律英文）：
- 成功 → `sent successfully via <host>:<port>`
- 失败 → `send failed: <原因>`（如认证失败、地址无效、附件读不到）
- 未配置 → `Email tool not configured: missing SMTP_HOST / SMTP_USERNAME / SMTP_PASSWORD environment variables`

> 没有「草稿/收件箱/搜索」等工具——cortex-mcp 只管发送。需要预览就让助手把正文打到对话里给你确认，再发。

---

## 六、助手启用 + 多账号路由

1. 「助手编辑页」→ 勾选要启用的 cortex-mcp 服务（可多选）。
2. **多账号时，务必在助手 system prompt 里写清路由规则**，否则模型可能用错发件身份：

   ```
   发送邮件时按收件场景选择发件账号：
   - 销售/商务相关 → 用 mcp__email-sales__send_email
   - 客户支持/售后 → 用 mcp__email-support__send_email
   发送前先把正文贴出来给我确认，再发。
   ```

3. slug 越语义化（`email-sales` 而非 `email1`），模型选对率越高。

---

## 七、验证

1. **探活**：「MCP 服务」页该服务显示绿色 + 工具数 9（全部工具静态注册，见 §4.1 说明）。
2. **工具可见**：助手编辑页勾选后，工具列表出现 `mcp__<slug>__send_email`。
3. **端到端**：让助手先贴正文确认，再发到测试地址：

   ```
   你：用销售邮箱给 test@example.com 发一封「报价确认」，正文先给我看。
   助手：[贴出正文]
   你：可以，发。
   助手：→ mcp__email-sales__send_email(...) → 「已通过 smtp.exmail.qq.com:465 发送成功」
   ```

收到邮件即全链路通。

---

## 八、安全

发邮件是 **External write（不可撤回）**。落地注意：

1. **确认后发**：让助手先把正文/收件人贴出来确认，再实际调用 `send_email`，避免误发。
2. **凭证隔离**：每账号独立 env + AesCodec 加密；多账号天然隔离。凭证只进子进程环境，不进 LLM 上下文。
3. **app-password**：Gmail/QQ/网易等一律用授权码或 app-password，**绝不用账号主密码**。
4. **slug 防误发**：多账号 slug 语义化 + system prompt 写清路由。
5. **出站限制**：运行机防火墙仅放行到目标 SMTP 服务器的 465/587。

---

## 九、常见问题

| 现象 | 原因 / 解决 |
|---|---|
| 探活失败 / 握手超时 | ① endpoint 不是绝对路径或二进制不存在；② cortex 进程对二进制无执行权限（`chmod +x`）；③ 容器部署时二进制没进容器 |
| 探活绿但发信报认证失败 | `SMTP_PASSWORD` 用了主密码而非授权码/app-password；或部分邮箱要求 `SMTP_USERNAME` 与 `SMTP_FROM` 一致 |
| 发信报连接超时 | 运行机到 SMTP 服务器 465/587 不通：防火墙/安全组未放行出站，或容器网络隔离 |
| Gmail 报认证失败 | 未开启两步验证，或 app-password 生成错误——必须用 [app-password](https://myaccount.google.com/apppasswords) |
| 多账号时模型发错邮箱 | slug 不够语义化，或 system prompt 没写路由规则 |
| 助手不调用邮件工具 | 「助手编辑页」没勾选启用该 MCP 服务——工具不勾选不注入 |
| 重启 cortex 后 endpoint/超时被改回 | 误把服务写进了 `[[mcp.seeds]]`——seed 每次启动覆盖 name/endpoint/args/transport/超时（env/headers 不受影响）。删掉 seed，改用界面维护 |
| 调用返回「未配置：缺少 SMTP_*」 | 该 MCP 服务的 env 没填齐 `SMTP_HOST/USERNAME/PASSWORD` |

> 历史「close_notify 报错但邮件其实发了」的问题在 cortex-mcp 不存在（lettre 收到 250 入队确认即返回成功，连接关闭阶段的错误不上冒泡）。

---

## 十、扩展：加一个新工具

cortex-mcp 设计为**可扩展工具集**。新增工具（如未来的日历、短信…）只需 4 步，业务逻辑与协议声明分离：

1. **新建模块** `crates/cortex-mcp/src/<tool>.rs`：
   - 配置结构 + `impl XxxConfig { fn from_env() -> Option<Self> }`（env 缺失返回 `None`）
   - `XxxInput`（`#[derive(Deserialize, schemars::JsonSchema)]`，字段注释会变成给模型的参数说明）
   - `pub async fn xxx(cfg: &XxxConfig, i: XxxInput) -> anyhow::Result<String>`
2. **`src/server.rs`** 的 `ToolServer` 加一个 `pub xxx: Option<XxxConfig>` 字段。
3. **`src/server.rs`** 的 `#[tool_router(server_handler)] impl ToolServer` 里加一个 `#[tool(description="...")]` 方法，
   取出配置后委托给模块函数；未配置时返回提示（仿照 `send_email`）。
   **description 必须以 `Requires env: ...` 结尾**（必填变量在前、optional 括号在后）——
   工具描述是产品界面上唯一展示给用户的说明（MCP 管理页「工具清单」弹窗），
   不带这行用户就不知道该配哪些环境变量。
4. **`src/main.rs`** 里 `from_env()` 读取并填入 `ToolServer`。

`server.rs` 顶部注释有同样说明。这样新工具的「注册」始终一目了然，主 crate 完全不受影响。

---

## 十一、与其它 MCP 的对比

| | markitdown | cortex-mcp（本二进制） |
|---|---|---|
| 调用方 | 后端**编程式**调用 | **LLM 代理驱动**（模型自主调工具） |
| slug | 必须 `markitdown`（代码硬编码路由） | 任意，语义化即可 |
| 传输 | streamable_http（跨机） | **stdio（同机子进程）** |
| 进程管理 | 独立 HTTP 服务，需常驻 | cortex 子进程，随 cortex 起停 |
| 助手勾选 | 不需要 | **需要** |
| 归属 | 外部依赖 | **本项目自带**（workspace 成员） |
| 扩展 | 否（单一用途） | **是**（加模块即加工具） |

---

## 十二、数据库查询（`db_*` 四工具，严格只读）

**一条 cortex-mcp 服务 = 一个数据库连接**。要看几个库就建几条 stdio MCP 服务，各自 env 独立、
凭证隔离；模型靠 slug 命名空间区分（`mcp__<slug>__db_query`）。支持 **MySQL / PostgreSQL / SQLite**。

### 12.1 实现：nyetdb v0.3.1 移植版

实现要点：**sqlparser AST 只读验证**（fail closed：解析不了=拒绝；panic 也变拒绝）、
Unicode 控制/格式字符剥离、MySQL 可执行注释检测、类型化解码（jsonb/uuid/numeric/时间 → JSON 值）、
EXPLAIN 代价护栏、PII 双网（查询前 net A / 结果溯源 net B）、每查一连接 + `BEGIN READ ONLY`、
JSON 信封输出（`{"v":1,"ok":...}` + `warnings[]` + 英文错误码）。

`DB_IMPL` 仅接受 `nyet`（缺省即 nyet）；显式设其他值 → 启动报配置错误（exit 2）。

### 12.2 四个工具

| 工具 | 作用 |
|---|---|
| `db_query {sql, limit?}` | 执行**单条只读 SQL**，返回 JSON 信封 |
| `db_schema {table?}` | 无 `table` → 全部表/视图清单；有 `table` → 该表列/键/索引/外键（可带 schema 前缀） |
| `db_sample {table, limit?}` | 随机抽 N 行（默认 10）；护栏拒了随机排序会回退首 N 行并打 `SAMPLE_FALLBACK` 警告 |
| `db_explain {sql}` | 看执行计划与代价预估，**不执行语句本身**；判决与 db_query 一致（含护栏） |

结果约束（保护 LLM 上下文）：行数上限默认 100（`DB_MAX_ROWS` 可调，硬上限 1000），
信封 `meta.truncated` 标截断。

### 12.3 env（连接参数，二选一）

**方式 A：一条 URL**

```
DB_URL=mysql://user:pass@host:3306/dbname
```

**方式 B：元组（密码含特殊字符时免转义，推荐）**

```
DB_TYPE=mysql
DB_HOST=10.0.0.5
DB_PORT=3306
DB_USER=readonly_user
DB_PASSWORD=<密码>
DB_NAME=dbname
```

| 变量 | 必填 | 说明 |
|---|---|---|
| `DB_URL` | 二选一 | `mysql://` / `postgres://` / `postgresql://` / `sqlite://`（也接受 `sqlite::`，如 `sqlite::memory:`）开头的连接 URL |
| `DB_TYPE` | 二选一 | `mysql` / `postgres`(或 `postgresql`) / `sqlite`；与 `DB_URL` 同时给时协议必须一致 |
| `DB_HOST` / `DB_PORT` / `DB_USER` / `DB_PASSWORD` / `DB_NAME` | 元组方式必填 | PORT 有引擎默认值（3306 / 5432 / —）；PASSWORD 可缺省（无密码场景）。**sqlite 时 `DB_NAME` 填 `.db` 文件路径**（推荐绝对路径，`:memory:` 亦可），且 sqlite 只需 `DB_TYPE` + `DB_NAME` 两项 |
| `DB_SSLMODE` | ❌ | `disable` / `prefer`(默认) / `required`(或 `require`)；仅 mysql/pg 生效（nyet 经 URL query 参数注入） |
| `DB_MAX_ROWS` | ❌ | 行数上限，默认 100，>1000 收敛到 1000 |
| `DB_QUERY_TIMEOUT_SECS` | ❌ | 单条 SQL 墙钟超时，默认 30，>300 收敛到 300 |
| `DB_IMPL` | ❌ | 仅接受 `nyet`（缺省即 nyet）；设其他值启动报错，见 §12.1 |

**护栏 / PII / 函数黑白名单**：

| 变量 | 说明 |
|---|---|
| `DB_GUARDRAIL_MODE` | `cost` / `rows` / `off`；缺省按引擎取默认（pg=cost、mysql/mariadb=rows、sqlite=off） |
| `DB_GUARDRAIL_MAX_COST` | cost 模式阈值（PG 计划总代价） |
| `DB_GUARDRAIL_MAX_ROWS` | rows 模式阈值（预估扫描行数） |
| `DB_PII` | `table.column` 逗号列表，如 `users.email,orders.phone`；命中即管控 |
| `DB_PII_MODE` | `deny`（默认，整条查询拒绝）/ `mask`（结果里该列打码，查询放行） |
| `DB_SQL_ALLOW_FUNCTIONS` / `DB_SQL_DENY_FUNCTIONS` | 函数白/黑名单覆盖（如 pg 的 `pg_sleep`） |
| `DB_MARIADB` | `1` 或 `true`：mysql 服务器实为 MariaDB 的**提示**，仅决定服务端超时变量（MySQL `max_execution_time` vs MariaDB `max_statement_time`）先试哪个，**不改变验证方言**（mariadb 与 mysql 同按 mysql 验证） |

> 未配任何 `DB_*` → 进程照常 serve，调用 db 工具返回未配置提示（与 send_email 同款行为）。
> **配置无效或连不上 → 进程 exit 2**，cortex 探活立刻转红；下次探测/使用时自动重新拉起，自愈。

### 12.4 nyet 的 JSON 信封与错误码

成功：`{"v":1,"ok":true,"rows":[...],"meta":{"row_count":3,"truncated":false,"duration_ms":2,"connection":"db"},"warnings":[...]}`；
`db_schema` 回 `schema`，`db_explain` 回 `estimate`。拒绝：`{"v":1,"ok":false,"error":{"code","reason","message","hint"}}`，
`code` 取 `NYET`（验证/护栏/PII 拒绝）/ `CONNECTION_FAILED` / `DB_ERROR` / `TIMEOUT` / `CONFIG_INVALID`，
`hint` 给模型可照抄的修正动作。拒绝是**工具结果**（信封自描述），不是 MCP 协议错误，模型读完 hint 自行改写。

### 12.5 接入 cortex（每库一条服务）

界面手加（同 §四：**不写 `[[mcp.seeds]]`**）。以「订单库」为例：

| 字段 | 填什么 |
|---|---|
| name | `数据库-订单库` |
| **slug** | **`db-orders`**（语义化，模型靠它选库） |
| transport | **stdio** |
| endpoint | `cortex-mcp` 二进制绝对路径（可与邮件服务复用同一二进制） |
| args | `[]` |
| env | §12.3 的连接键值对（加密存储） |
| 超时 | `45` 左右（默认 SQL 超时 30s + 余量） |

多库时在助手 system prompt 写清路由：「查订单 → `mcp__db-orders__db_*`；查用户 → `mcp__db-users__db_*`」。

### 12.6 只读防线（三层，缺一不可）

**三层：**

1. **AST 只读验证**（sqlparser，fail closed）：完整解析后逐节点判定，多语句/PRAGMA/写操作/可执行注释/
   超深嵌套（防 DoS）一律拒绝；Unicode 控制字符先剥离；PII net A 在这里拒绝整条查询（deny 模式）。
   解析失败本身就是拒绝——「看不懂的语句不放行」。PII net B 在结果出口再拦一道（防 `SELECT *`）。
2. **连接层只读会话**：MySQL `START TRANSACTION READ ONLY`（与超时 SET 同一往返先行下发，查询结束关连接时 `ROLLBACK` 收场）；
   PostgreSQL `BEGIN READ ONLY` + 启动参数 `default_transaction_read_only=on`；SQLite 文件级 `read_only(true)`。
3. **护栏**（EXPLAIN 代价）：预估超阈值拒绝执行并回 `EXPENSIVE_QUERY`（附支撑判决的计划），防止误跑全表扫描。

### 12.7 常见问题

| 现象 | 原因 / 解决 |
|---|---|
| 探活红，stderr 提示 `DB_* 配置无效` | env 缺必填项或值非法（如 `DB_TYPE` 拼错、URL 协议不支持、nyet 专属 env 值非法）。按 §12.3 补齐；修好后探活自动恢复 |
| 探活红，stderr 提示 `数据库启动自检失败` | 自检（`SELECT 1`，nyet 走完整流水线，受 `DB_QUERY_TIMEOUT_SECS`（默认 30s）与 ≥10s 的连接握手 deadline 约束）不通：账号密码错 / 网络不通 / 库不存在。自检在 30s stdio 握手窗口内完成 |
| SQLite 库在 WAL 模式下打开报错 | WAL 需要写 `-shm`/`-wal`，只读打开可能失败。把库切回 `journal_mode=DELETE`，或给文件目录加写权限（数据仍不可写） |
| `db_query` 信封 `"ok":false, code:"NYET"` | 正常防线：写操作 / 多语句 / PRAGMA / 可执行注释 / PII deny / 护栏拒绝（`reason` 区分，`hint` 给改法）。按 hint 改写成单条合法 SELECT |
| `db_sample` 带 `SAMPLE_FALLBACK` 警告 | 随机排序被护栏判为太贵（等于全表排序），回退为首 N 行且**不代表整表分布**；要真随机按警告里的语句自己 `db_query` |
| 自签证书连 PG/MySQL 失败 | `DB_SSLMODE=disable`（明文，仅内网）或给运行机装 CA 后 `required` |
| 信封带 `INSECURE_TRANSPORT` 警告 | nyet：连接是明文的且 `DB_SSLMODE` 未要求加密；内网可接受，公网必须收紧 |
| 结果被截断 | 行数到 limit/`DB_MAX_ROWS` 上限（nyet：`TRUNCATED` 警告 + `meta.truncated:true`）。加 `WHERE`/`LIMIT` 收窄，或调大 env（≤1000） |

**安全建议**：生产上给 cortex-mcp 用**只读账号**（MySQL `GRANT SELECT`、PG `CREATE ROLE ... READ ONLY`），
防线之上再加权限层；凭证经 env 注入 + AesCodec 加密落库，不进 LLM 上下文。

**第三方归属**：`db/nyet/` 移植自 [nyetdb](https://github.com/stasmarkin/nyetdb) v0.3.1
（© Stas Markin，MIT OR Apache-2.0）。许可证副本在 `crates/cortex-mcp/third-party/nyetdb/`，
各移植文件头有 `Adapted from nyetdb` 标注；移植只做了「CLI → MCP 工具面」的编排适配与
mongo/clickhouse/redis 引擎裁剪，验证器与防线逻辑保持原样（含其测试与 corpus）。

## 十三、InfluxDB 时序查询（`influx_*` 两工具，只读）

**一条 cortex-mcp 服务 = 一个 InfluxDB 服务**（v2 的多 bucket / v3 的多 database 都在
同一个服务内用参数区分；多个 InfluxDB 实例才需要多条服务）。支持 **v2（Flux）与
v3（SQL / InfluxQL）**，查询语言由 `INFLUX_VERSION` 决定，工具面统一。

### 13.1 选型：v2 直连 REST，v3 官方客户端

| 版本 | 实现 | 为什么 |
|---|---|---|
| v2 | reqwest 0.13 直连 REST（`POST /api/v2/query`，annotated CSV 自解析） | InfluxData **官方没有 v2 Rust 客户端**（官方仅 Go/Java/JS/Python 等）；社区 `influxdb2` 停更于 2024-07 且锁 reqwest 0.11。v2 查询本质是一个 POST 返回 annotated CSV，自实现比引停更依赖划算 |
| v3 | [`influxdb3-client`](https://github.com/InfluxCommunity/influxdb3-rust)（InfluxData 官方，InfluxCommunity 维护） | 官方维护、2026-06 仍在发版；查询走 Arrow Flight gRPC，行类型化现成 |

> **验证状态**：v2 已对真实 InfluxDB 2.7 服务器做过全链路 e2e（查询/类型化/截断/只读护栏/
> schema 探查/错误码，14 项）；v3 已对真实 **influxdb3-core 3.11.1** 服务器做过全链路 e2e
> （SQL/InfluxQL 查询、类型化、limit 截断、只读护栏、坏 token 退出码、databases/tables/
> columns 三级 schema，16 项全过）。两个 v3 实测要点：
> - `SHOW DATABASES` 的 Flight 路径在 3-core **未实现**（报 "This feature is not
>   implemented"），改走服务端原生 HTTP 端点 `POST /api/v3/query_influxql`
>   （`{"q":"SHOW DATABASES"}`，format=jsonl）——纯网络调用，目标机器**无需安装任何
>   influxdb3 命令行工具**；
> - `information_schema.tables / .columns`（`table_schema='iox'`）实测可用。

> crates.io 上另有个 `influxdb3`（个人移植）与 `zenoh-backend-influxdb-v2`
> （Eclipse zenoh 的存储后端插件，非查询客户端）——都不要用。

### 13.2 两个工具

| 工具 | 作用 |
|---|---|
| `influx_query {query, dialect?, limit?}` | 执行**单条只读查询**。v2 用 Flux（如 `from(bucket:"mnet") |> range(start:-1h) \|> filter(fn:(r) => r._measurement=="cpu")`）；v3 用 SQL（默认）或 InfluxQL（`dialect:"influxql"`）。返回 JSON 信封 |
| `influx_schema {bucket?, measurement?}` | 无参 → bucket（v2）/ database（v3）清单；只给 `bucket` → measurement（v2）/ table（v3）清单；两者都给 → 字段与 tag（v2）/ 列与类型（v3）。**写查询前先探结构** |

结果约束：行数上限默认 100（`INFLUX_MAX_ROWS`，硬上限 1000）；v2 响应体另有 8 MiB 上限；
信封 `meta.truncated` + `TRUNCATED` 警告标截断。

### 13.3 env

| 变量 | 必填 | 说明 |
|---|---|---|
| `INFLUX_URL` | 是 | 如 `http://127.0.0.1:8086`（v3 默认端口 8181） |
| `INFLUX_TOKEN` | 是 | API token（**建议只读权限**，见 §13.6） |
| `INFLUX_VERSION` | ❌ | `2`（默认）或 `3`（大小写与 `v` 前缀均可：`2`/`v2`/`V2`/`3`/`v3`/`V3`） |
| `INFLUX_ORG` | v2 必填 | org 名（如 `resolink`） |
| `INFLUX_DATABASE` | v3 必填 | 数据库名；**v3 一条进程绑定一个库**（官方客户端查询不带 per-query db），查别的库另配一条服务 |
| `INFLUX_BUCKET` | ❌ | v2 默认 bucket：`influx_schema` 只给 `measurement` 时兜底 |
| `INFLUX_MAX_ROWS` | ❌ | 行数上限，默认 100，>1000 收敛到 1000 |
| `INFLUX_TIMEOUT_SECS` | ❌ | 单查超时，默认 30，>300 收敛到 300 |

> 未配任何 `INFLUX_*` → 进程照常 serve，调用返回未配置提示。配置无效或自检失败
> （/health、token 验证、最小查询）→ **exit 2**，探活转红自愈（与 DB_* 同款）。

### 13.4 JSON 信封与错误码

成功：`{"v":1,"ok":true,"rows":[...],"meta":{"row_count":5,"truncated":false,"duration_ms":12,"connection":"influxdb2"},"warnings":[...]}`；
`influx_schema` 按模式回 `buckets` / `measurements` / `fields`+`tags`（v2）或
`databases` / `tables` / `columns`（v3）。行值按 annotated CSV 的 `#datatype` 类型化
（数值/布尔/时间字符串）。拒绝：`{"v":1,"ok":false,"error":{"code","message","hint"}}`，
`code` 取 `CONNECTION_FAILED` / `AUTH_FAILED` / `QUERY_REJECTED`（本地护栏）/
`QUERY_ERROR`（服务器判定）/ `SERVER_ERROR` / `TIMEOUT` / `INTERNAL`，`hint` 必填。

### 13.5 只读防线

1. **v2 Flux 函数黑名单**：`to` / `http.*` / `sql.*` / `socket.*` / `kafka.to` 等副作用
   函数命中即拒（Flux 是管道语言，做不了语句白名单；黑名单 + 只读 token 纵深防御）。
2. **v3 语句白名单**：首关键字必须是 SELECT / SHOW / WITH / DESCRIBE / EXPLAIN，
   且单语句（分号后还有内容即拒）。
3. **资源上限**：行数 ≤1000、v2 响应体 ≤8 MiB、单查 ≤300s（都可经 env 收紧）。

### 13.6 常见问题

| 现象 | 原因 / 解决 |
|---|---|
| 探活红，stderr 提示 `INFLUX_* 配置无效` | 缺必填项（v2 缺 `INFLUX_ORG`、v3 缺 `INFLUX_DATABASE` 等）或 URL 非 http(s)。按 §13.3 补齐 |
| 探活红，提示 `InfluxDB 启动自检失败` | /health 不通（服务没起/端口错）或 token 验证失败（401）或 v3 最小查询失败（database 不存在） |
| `code:"QUERY_REJECTED"` 且提到 `to()` | 正常防线：Flux 写函数被黑名单拒了；按 hint 移除写调用 |
| v3 查别的 database 被拒 | 一条进程绑一个库；给目标库另配一条 MCP 服务 |
| 结果被截断（`TRUNCATED`） | 行数到上限。Flux 加 `|> limit(n:)` / 收窄 `range`；或调大 `INFLUX_MAX_ROWS`（≤1000） |
| 查不到老数据 | Flux 的 `range` 没覆盖（`start:0` 全时段）；注意 schema 包的 field/tag keys 查询工具已默认 `start:0` |

**安全建议**：给工具配**只读 token**（v2 建-token 时只授读权限；v3 建只读 token），
黑名单/白名单之上再加权限层。凭证经 env 注入 + AesCodec 加密落库，不进 LLM 上下文；
信封与日志里不回显 token。

---

## 十四、Prometheus 查询（`prom_*` 两工具，只读）

**一条 cortex-mcp 服务 = 一个 Prometheus 服务**（可带路径前缀，如网关后面的
`https://gw.example.com/prometheus`；多个实例才需要多条服务）。查询语言只有一种
——PromQL，即时（instant）与区间（range）两种查询形态。

### 14.1 选型：`prometheus-http-query` 0.9.0 + 表达式自解析

Prometheus 官方只发 Go 客户端，**没有官方 Rust 查询客户端**。社区事实标准是
[`prometheus-http-query`](https://crates.io/crates/prometheus-http-query)
（5.5M+ 下载、持续维护、基于 reqwest，与本项目 HTTP 栈对齐），选它。

> **验证状态**：已对真实 **Prometheus 3.14.0** 服务器做过全链路 e2e（即时/区间/标量/
> rate 聚合、RFC3339 时间参数、limit 截断、四类本地护栏、坏 PromQL 错误映射、
> schema 两级探查、未配置提示、坏 URL 退出码，24 项全过）。两个实测要点：
> - **crate 0.9.0 的类型化反序列化对 scalar/string 结果有 bug**（`invalid type: map,
>   expected f64`，live 探测复现）——表达式查询（`/api/v1/query`、`/api/v1/query_range`）
>   改走 crate 的 `get_raw()` 拿原始响应后**自行解析 JSON**（该格式极简且稳定）；
>   crate 仍用于 URL 构建 / Bearer 鉴权 / `label_values` / `series` / `metadata` 端点。
> - Prometheus 的错误在 **JSON 体里**（`status:"error"` + `errorType`），即使 HTTP
>   也是 400/503——错误映射先看体再看 HTTP 状态，否则坏 PromQL 会被误报成
>   SERVER_ERROR 而不是 QUERY_ERROR。

### 14.2 两个工具

| 工具 | 作用 |
|---|---|
| `prom_query {query, time?, start?, end?, step?, limit?}` | 执行**单条只读 PromQL**。只给表达式 → 即时查询（`time` 可选：unix 秒或 RFC3339，缺省当前时刻）；`start`+`end`+`step` **三者齐备** → 区间查询（`step` 为秒数，可小数），每个数据点展开成一行。返回 JSON 信封 |
| `prom_schema {metric?}` | 无参 → 全部指标名清单（按 `label __name__ values`）；给 `metric` → 该指标的 type / help / unit / label 键（`metadata` + `series` 端点合并）。**写查询前先探结构** |

行结构：labels **平铺**（`__name__`、`job`、`instance`…）+ `value`（数值；`NaN`/`±Inf`
以字符串保真）+ `time`（RFC3339 UTC）。行数上限默认 100（`PROM_MAX_ROWS`，硬上限
1000）；信封 `meta.truncated` + `TRUNCATED` 警告标截断。

### 14.3 env

| 变量 | 必填 | 说明 |
|---|---|---|
| `PROM_URL` | 是 | 如 `http://127.0.0.1:9090`；可带路径前缀 `/prometheus`（经反向代理时） |
| `PROM_TOKEN` | ❌ | Bearer token。**仅在服务前有网关鉴权时给**；无鉴权的内网服务省略 |
| `PROM_MAX_ROWS` | ❌ | 行数上限，默认 100，>1000 收敛到 1000 |
| `PROM_TIMEOUT_SECS` | ❌ | 单查超时，默认 30，>300 收敛到 300 |

> 未配任何 `PROM_*` → 进程照常 serve，调用返回未配置提示。配置无效（URL 非
> http(s) 等）或自检失败（最小查询 `1` 不通，可能是网络/鉴权）→ **exit 2**，探活转红
> 自愈（与 DB_* / INFLUX_* 同款）。

### 14.4 JSON 信封与错误码

成功：`{"v":1,"ok":true,"rows":[...],"meta":{"row_count":5,"truncated":false,"duration_ms":12,"connection":"prometheus"},"warnings":[...]}`；
`prom_schema` 无参回 `metrics:[...]`，带 metric 回 `metric_type` / `help` / `unit` /
`labels`。失败：`{"v":1,"ok":false,"error":{"code","message","hint"}}`，`code` 与
`influx_*` 共用同一套闭集（见 §13.4）：本地护栏（区间参数不齐 / 时间解析失败 /
空表达式 / `step<=0`）→ `QUERY_REJECTED`；服务器判定的错误（坏 PromQL、超时、5xx）
按 `errorType` 映射为 `QUERY_ERROR` / `TIMEOUT` / `SERVER_ERROR`；连接层 →
`CONNECTION_FAILED`。`hint` 必填（英文，模型可见）。

### 14.5 只读防线

1. **语言只读（构造性）**：PromQL 是纯表达式语言，**没有写语法**；工具面也只碰
   `/api/v1/query`、`/api/v1/query_range`、`/api/v1/label/*/values`、
   `/api/v1/series`、`/api/v1/metadata` 五个只读端点，绝不触 `/api/v1/admin/*`
   （快照/删数据等管理端点）。
2. **本地参数护栏**：区间三参数齐备性、时间格式（unix 秒/RFC3339）、`step` 有限
   且 >0、`end >= start`、空表达式——不合法直接 `QUERY_REJECTED`，不出网。
3. **资源上限**：行数 ≤1000、单查 ≤300s（都可经 env 收紧）。

### 14.6 常见问题

| 现象 | 原因 / 解决 |
|---|---|
| 探活红，stderr 提示 `PROM_* 配置无效` | `PROM_URL` 缺失或非 http(s)。按 §14.3 补齐 |
| 探活红，提示 `Prometheus 启动自检失败` | URL 不通（服务没起/端口错）或网关 401（`PROM_TOKEN` 缺失或错） |
| `code:"QUERY_REJECTED"` 且提到 range | 区间查询 `start`/`end`/`step` 只给了部分——三者要么全给要么全不给 |
| `prom_schema` 里 `up` 的 `metric_type:"unknown"` | 正常：`up` 是 Prometheus 合成指标，服务器无其 metadata；labels 仍从 series 端点探得 |
| 结果被截断（`TRUNCATED`） | 数据点到行数上限。收窄 `range` / 加大 `step` / 按 label 过滤；或调大 `PROM_MAX_ROWS`（≤1000） |
| `value` 是字符串 `"NaN"` / `"+Inf"` | JSON 无法表达非有限数值，以字符串保真；模型侧按字面理解即可 |

**安全建议**：工具本身只读，但若前面有网关，建议给该 token 只授读路径；凭证经 env
注入 + AesCodec 加密落库，不进 LLM 上下文；信封与日志里不回显 token。
