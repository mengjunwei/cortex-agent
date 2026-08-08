# 对象存储(RustFS)改造设计 — 6+ 节点负载均衡高可用

> 状态:草案待评审 · 日期:2026-08-04 · 作者:cortex-agent 重构
>
> 关联规范:[`architecture.md`](../../architecture.md)(§2.4 基础设施层 / §5 AppDeps / §8.6 数据访问 / §9 反模式 #6/#8)

## 1. 背景与目标

cortex-agent 正从单机走向 **6+ 节点负载均衡**。会话/业务数据已在 PostgreSQL + Redis 共享,但有一批文件资产仍落本地磁盘,多实例下不共享、单点故障即丢失:

- 节点 A 截的图,节点 B 的 `/api/screenshots/...` 直接 404;
- 用户上传图片以 base64 内联进 PG 的 `events` JSONB,既不共享又撑大数据库;
- 代码沙箱工作目录 `workspaces/sessions/{sid}/` 纯本地,节点切换后丢失 agent 产出;
- ADK Artifact 走本地 `FileArtifactService`,跨节点不共享。

**目标**:引入 S3 兼容对象存储(**RustFS**,自建)作为共享文件层,做到:

1. 截图 / 上传图 / artifact:跨节点共享、无状态访问;
2. 代码沙箱工作区:网关会话亲和保证本地 POSIX 性能 + RustFS 快照保证节点故障可恢复(用户已确认接受恢复延迟);
3. 会话/业务数据:维持 PG + Redis,不动。

**边界约定(用户确认)**:
- **不考虑历史数据迁移**——新数据直接走对象存储,存量本地文件 / 历史基 base64 events 不做迁移、不做回退兼容。
- 对象存储用 **RustFS**(S3 兼容),代码层与 MinIO 无差异。

## 2. 现状盘点(调研结论)

| 资产 | 现位置 | 跨机器 | 备注 |
|---|---|---|---|
| 会话/消息/轮次 | PG(`sessions`/`events`/`app_states`/`user_states`,adk `PostgresSessionService`) | ✅ 共享 | 不动 |
| 业务表/记忆 | PG + Redis | ✅ 共享 | 不动 |
| **截图** | 本地 `screenshots/{sid}/` | ❌ | 元信息(`image_url`)嵌 events,共享;字节本地,断 |
| **上传图片** | base64 内联进 PG `events` | ⚠️ | 不缺共享,但**撑大 DB + 每轮重发 MB base64** |
| **沙箱工作区** | 本地 `workspaces/sessions/{sid}/` | ❌ | 仅 `delete_session` 时清理,无 TTL |
| **ADK Artifact** | 本地 `FileArtifactService`(`artifacts/`) | ❌ | **休眠:全仓无任何生产者/消费者,目录空** |
| Skill | 本地 `skills/`(builtin 编译期嵌入;user 手放) | 部分 | 基本不动(脚本需本地执行) |

**三个关键发现(影响设计)**:

1. **沙箱当前只服务 `shell_command`**。`read_file`/`edit_file`/`grep`/`create_file`/`list_directory` 工厂函数已写好(`src/tools/code/*.rs`)但**无任何调用点**——`build_custom_agent`(`src/agent/custom.rs:85-222`)的 `push_tool_for_key` 只识别 `search_kb`/`query_device_catalog`/`shell_command`;`AgentRequest.workspace_mode` 在 `build_agent_for_session`(`custom.rs:284-293`)被 `..` 解构丢弃。故快照当前保护面=shell 产出,需一并接线 code 工具。
2. **上传图给模型有现成 URL 通路**。`build_user_content`(`src/server/sse/mod.rs:696-698`)已有 `https://` → `Part::FileData` 分支;OpenAI(`src/llm/openai_custom.rs:366-372`)与 Anthropic(`src/llm/anthropic_custom/convert.rs:111-125`)都已支持 `FileData` → URL image block。
3. **ArtifactService 是可替换的 async trait**(`adk-artifact-1.0.0/src/service.rs:107-127`,5 方法 + 默认 `health_check`,`Part` 已 `serde::Serialize`),唯一替换点 `src/bootstrap.rs:167-180`。

## 3. 总体架构

```
                 ┌──────── 负载均衡(一致性哈希会话亲和)────────┐
                 │   hash(session_id) → 固定节点;节点增减迁移最小  │
                 ▼                                                    ▼
         ┌──────────────┐                                      ┌──────────────┐
         │ cortex 节点 1 │            ......                    │ cortex 节点 N │
         │ 本地:沙箱SSD │                                      │ 本地:沙箱SSD │
         └──────┬───────┘                                      └──────┬───────┘
                │                                                     │
   共享层 ─────┼───────────────────────────────────────────────────┼──────
         ┌─────▼─────┐  ┌──────────┐  ┌──────────────────────────▼─────┐
         │ PostgreSQL │  │  Redis   │  │  RustFS(S3 兼容对象存储,自建)  │
         │ 会话/业务表 │  │ 记忆/队列 │  │ screenshots / uploads /        │
         └───────────┘  └──────────┘  │ artifacts / workspaces(快照)  │
                                       └────────────────────────────────┘
```

- **会话亲和**:同一会话始终路由到同一节点 → 沙箱工作区留本地 SSD,保证 POSIX 性能(bwrap 依赖命名空间 + bind mount,**不能上 NFS/对象存储**)。
- **共享对象存储**:截图/上传图/artifact 无状态访问;沙箱工作区做快照容灾。
- **RustFS 自身高可用**:由 RustFS 自身纠删码 / 多盘冗余保证(部署侧),cortex 只把它当 S3 endpoint 用。

## 4. 详细设计

### 4.1 ObjectStore 基础设施层(一期,基建)

**归属**:`src/infra/object_store.rs`(技术栈,归 infra;见 architecture §2.4)。

**依赖**:`opendal`(Apache,Rust,统一多后端抽象)。用其 `services::S3` 接 RustFS(标准 S3 协议,path-style)。opendal 同时提供 `services::Fs`,本地开发环境可直接切本地目录后端,**无需起 RustFS 即可调试**。

**抽象**:
```rust
// src/infra/object_store.rs
pub struct ObjectStore { op: opendal::Operator }

impl ObjectStore {
    pub async fn put(&self, key: &str, data: Bytes) -> Result<(), AppError>;
    pub async fn get(&self, key: &str) -> Result<Bytes, AppError>;
    pub async fn delete(&self, key: &str) -> Result<(), AppError>;
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>, AppError>;
    pub async fn delete_prefix(&self, prefix: &str) -> Result<(), AppError>;
    // 可选:presigned GET(给模型/前端直链场景)
    pub async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, AppError>;
}
```

**规范对齐**:
- 错误用 `AppError`(给 `AppError` 加 `ObjectStore` 变体,`opendal::Error` 映射进去)——禁 anyhow(反模式 #8)。
- 注入 `AppDeps.object_store: Arc<ObjectStore>`(§5)。
- 业务层不直接 `use crate::infra::*` 操纵裸 S3;截图/上传图/沙箱各自的存储逻辑通过 `AppDeps.object_store` 调用(反模式 #6,与现有 db pool 经 `<Entity>Store` 间接用同一模式)。
- 无 `unwrap`/`expect`(反模式 #9),`tracing` 结构化日志(§7)。

**配置**(见 §6 `[object_storage]`):`enabled` / `backend`(rustfs|fs) / `endpoint` / `region` / `bucket` / `access_key` / `secret_key` / `secure` / `path_style`。

### 4.2 截图上 RustFS(一期,核心痛点)

**key 规则**:`screenshots/{session_id}/{filename}`(沿用现目录结构,平移为 object key)。

| 操作 | 现在 | 改后 |
|---|---|---|
| 写 | `src/tools/screenshot.rs:127-140` `save_base64_screenshot` 本地 `tokio::fs::write`;SSE 兜底 `src/server/sse/screenshot.rs:59-121` | 解码 base64 后 `object_store.put(key, bytes)`;两条路径都改 |
| 读 | `src/server/mod.rs:265-315` `serve_screenshot` 本地 `tokio::fs::read` | **代理读**:`object_store.get(key)` → 流式回吐 `image/png` |
| 删(会话) | `src/infra/screenshot_cleanup.rs:149-243` `delete_session_screenshots` `remove_dir_all` | `object_store.delete_prefix("screenshots/{sid}/")` |
| 删(孤儿) | `screenshot_cleanup.rs:252-355` 后台 1h 定时,遍历本地 + 查 events 引用集 | **简化为主路径 + RustFS 生命周期兜底**:会话删除同步删 prefix(上行)是主清理;孤儿交给 RustFS 对象生命周期规则(N 天后自动过期)。**移除**后台遍历引用集的孤儿任务——它只因本地文件无法自动过期才需要,对象存储原生支持过期 |

**保留的安全边界(commit bce1048 不动)**:`serve_screenshot` 的**登录鉴权 + 会话归属校验**(`src/server/mod.rs:285-304` `session_belongs_to_user`)、路径段校验 `is_safe_screenshot_segment` 全部保留——代理读只是把"读本地"换成"读 RustFS",鉴权链不变,前端 URL `/api/screenshots/{sid}/{file}` 不变,前端零改动。

**events 字段调整**:工具返回 JSON 里 `saved_path`(本地绝对路径)改为存 **object key**;`image_url` 相对 URL 不变(仍 `/api/screenshots/{sid}/{file}`,后端代理读)。`image_url` 一直是可移植的相对 URL,无需改。

> 注:用户声明不考虑历史数据,故 events 中历史的 `saved_path` 绝对路径不处理;孤儿清理的引用集提取逻辑(`extract_filenames_from_value`)兼容 `image_url` 即可。

### 4.3 上传图片上 RustFS(二期,顺带给 PG 瘦身)

**key 规则**:`uploads/{user_id}/{filename}`。

**核心改动**:`src/server/mod.rs:370-445` `handle_upload_image`
- 现在:base64 编码 → 返回 `data:{mime};base64,...` → 前端塞 `attachment.url` → `build_user_content` 走 `InlineData` → **base64 字节入库 events**。
- 改后:`object_store.put(key, bytes)` → 返回 **presigned HTTPS URL**(长 TTL)→ `build_user_content` 走已有 `https://` → FileData 分支 → events 里只存**几十字节的 URL**,不再 inline base64。

**给多模态模型看图(零改动)**:用户已确认**模型 API 与 RustFS 网络互通**,故走 presigned URL 最简方案,LLM 层与会话层零改动:

- `handle_upload_image` 上传后返回 **presigned HTTPS URL**(长 TTL,默认 7 天);
- events 存 `Part::FileData { file_uri: <presigned url> }`(https,几十字节);
- `build_user_content`(`src/server/sse/mod.rs:696-698`)的 `https://` → FileData 分支**已就绪**;
- OpenAI(`src/llm/openai_custom.rs:366-372`)与 Anthropic(`src/llm/anthropic_custom/convert.rs:111-125`)的 FileData → URL image block **已就绪**;
- 模型直接拉 RustFS 的 presigned URL,无需 cortex 中转。

**presigned TTL**:默认 7 天覆盖会话生命周期;用户已声明不考虑历史数据,超 TTL 的历史图片失效可接受。TTL 进配置项(`object_storage.presign_ttl_secs`)。

**前端展示**:同一 presigned URL 直接给 `<img>`,前端零改动,不暴露 RustFS 凭据(presigned 仅授权该 object 的 GET)。

**收益**:events 表显著瘦身(几 MB → 几十字节);每轮推理由"重发 MB base64"降为"重发小 URL 字符串"(模型侧自行拉图)。

### 4.4 代码沙箱工作区快照容灾(二期)

**机制**:会话亲和 + 本地 SSD 工作 + RustFS 快照恢复。

**快照 key**:`workspaces/{session_id}/snapshot.tar.zst`(全量打包,覆盖式;起步用全量,代码工作区通常百 MB 级可接受)。

**上传触发点**(挂在现有生命周期钩子,调研已确认):
| 触发 | 位置 | 说明 |
|---|---|---|
| 每轮结束 | `src/server/sse/mod.rs:~1306` RUN_FINISHED 尾部 spawn | 最自然的"单轮稳定结束"点 |
| 空闲兜底 | 仿 `src/infra/screenshot_cleanup.rs` 起后台定时 task | 空闲 N 分钟打包上传,防异常中断漏快照 |
| 会话删除前 | `src/server/session.rs:409-457` `delete_session` 入口 | 上传最终快照后删本地 + 删 RustFS 快照 |

**恢复触发点**:`src/server/sse/mod.rs:463-494` 创建沙箱目录处——`create_dir_all` 之后、构建 agent 之前,插入"若本地目录为空且 RustFS 存在快照 → 拉取解包到本地"。这是唯一且最自然的恢复注入点(会话亲和下,本地非空说明是原节点续跑,跳过恢复)。

**RPO**:取决于"每轮结束快照"的粒度(最坏丢失末轮产出)。用户已确认接受。

**配套:接线 code 工具**(二期确定做)。当前 Sandbox 只服务 `shell_command`,快照保护面太窄。在 `push_tool_for_key`(`src/agent/custom.rs:34-71`)增加 `read_file`/`edit_file`/`grep`/`create_file`/`list_directory` 分支,用 `workspace_mode.root_path()` 构造工具(工厂已就绪,见调研一 Q5);`build_agent_for_session` 需把 `workspace_mode` 真正传给 builder(现被 `..` 丢弃)。接线后沙箱才有实质工作内容值得快照。

**清理**:会话删除同步删 RustFS `workspaces/{sid}/` 前缀;可加 RustFS 侧生命周期规则做最终兜底。

### 4.5 ADK Artifact 换 RustFS(三期,按需)

`impl ArtifactService` for `S3ArtifactService`(`src/infra/artifact_s3.rs`):
- `save` → `put` key `{app}/{user}/{session}/{file}/v{version}`;复用 `Part` 的 `serde_json::to_vec` 序列化(与 `FileArtifactService` 一致,未来可互读)。
- `load` → `get` + 反序列化回 `Part`。
- `delete` → 删单版本 / 前缀。
- `list` / `versions` → `list` 解析 key。
- `health_check` → `HeadBucket` 或用默认实现。

**替换点唯一**:`src/bootstrap.rs:167-180` 把 `FileArtifactService::new(&artifact_dir)` 换成 `S3ArtifactService::new(obj_cfg)`,返回类型 `Option<Arc<dyn ArtifactService>>` 不变,下游(sse/runner/工具)面向 trait,**零改动、零风险**(当前休眠,无人调用)。

**触发条件**:等 code 工具接线 / 引入"产出文件"类 agent 后再做;在此之前价值为零。

### 4.6 网关会话亲和(一期配套,部署侧)

- **一致性哈希**按 `session_id` 路由,节点增减时迁移面最小。
- **健康检查**自动摘除故障节点(摘除后其会话被路由到新节点 → 触发 §4.4 快照恢复)。
- **滚动更新前 drain**:重启节点前等待其上活跃会话结束或迁走,避免粗暴打断。

> 这是负载均衡器/Nginx/Ingress 配置,非 cortex 代码;spec 给出要求,部署文档(`DEPLOY.md`)补充具体配置示例。

## 5. 依赖注入与数据流

- `AppDeps` 新增字段 `pub object_store: Arc<ObjectStore>`(`src/bootstrap.rs` 装配,§5.2)。AppDeps 已 25 字段、处于 Level 2→3 过渡,加一个尚可;若后续触发 Level 3 切分,对象存储归入"基础设施类"子 struct。
- 截图/上传图/沙箱的存储调用通过 `AppDeps.object_store`(经 Axum `State`/`FromRef` 提取,§5.5),不进全局(反模式 #2)。

## 6. 配置项(`config/config.toml` + `src/config/mod.rs`)

```toml
[object_storage]
enabled = true
backend = "rustfs"          # rustfs(生产)| fs(本地开发,opendal Fs 后端)
endpoint = "http://rustfs.internal:9000"
region = "us-east-1"
bucket = "cortex-agent"
access_key = "..."
secret_key = "..."          # 敏感:走现有配置加载,不入库不入日志(§8.7)
secure = false              # 内网 http;公网 true
path_style = true           # RustFS/MinIO 类用 path-style
```

`ObjectStorageConfig` 结构体 + `Default`;`enabled = false` 时装配返回 `None`(本地极简调试用,非历史数据兼容)。

## 7. Key 命名规范(统一)

```
screenshots/{session_id}/{filename}
uploads/{user_id}/{filename}
artifacts/{app}/{user}/{session}/{file}/v{version}
workspaces/{session_id}/snapshot.tar.zst
```

前缀即"资产种类",便于按种类做 RustFS 生命周期规则 / 权限隔离。

## 8. 分期计划

| 期 | 范围 | 价值 |
|---|---|---|
| **一期** | ObjectStore 基建(§4.1)+ 截图上 RustFS(§4.2)+ 网关会话亲和配置(§4.6) | 解决用户原始核心痛点:跨节点看图不再 404 |
| **二期** | 上传图上 RustFS(presigned,§4.3)+ 沙箱快照容灾 + code 工具接线(§4.4) | PG 瘦身;节点故障沙箱可恢复;补齐 code 工具 |
| **三期** | ArtifactService 换 RustFS(§4.5) | 等 artifact 有真实消费者再做;当前休眠 |

每期独立可交付、可验证;一期落地后系统已可在 6+ 节点正常跑。

## 9. 风险与权衡

| 项 | 决策 | 理由 / 兜底 |
|---|---|---|
| 依赖 opendal vs aws-sdk-s3 | **opendal** | 统一抽象(未来换后端零成本)、纯 Rust、tokio 契合;aws-sdk-s3 官方但拖重依赖 |
| 截图读法:代理读 vs presigned | **代理读** | 保留 commit bce1048 鉴权 + 会话归属校验,前端零改,不暴露 RustFS;代价是后端一跳流量,内网可接受 |
| 上传图给模型 | **presigned URL**(模型与 RustFS 内网互通) | LLM 层零改动;长 TTL(默认 7 天)覆盖会话;超期历史图失效(用户已确认不考虑历史数据) |
| 沙箱 RPO | 每轮结束快照 | 用户已确认接受延迟与末轮丢失风险 |
| RustFS 单点 | 部署侧纠删码/多盘 | 由 RustFS 自身冗余保证,cortex 只当 S3 用 |
| code 工具未接线 | 二期一并接 | 否则沙箱快照保护面过窄,无意义 |
| artifact 休眠 | 三期 / 暂不做 | 当前零消费者,换实现零风险但也零价值 |

## 10. 不在本设计内(YAGNI)

- 历史数据迁移 / 本地回退兼容(用户明确不做)。
- Skill 上对象存储(builtin 嵌二进制;user skill 需本地跑脚本,最多做同步分发,优先级最低,暂不做)。
- 沙箱工作区增量快照 / NFS 共享卷(全量快照 + 会话亲和已满足需求;增量与共享卷属过度设计)。
