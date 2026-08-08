-- =============================================================================
-- cortex-agent 数据库 Schema（权威建表脚本）
-- =============================================================================
-- 用途：
--   本文件是 cortex-agent 的【权威】建表脚本。新部署【必须先执行本文件】再启动程序。
--   cortex-agent 启动时不再自动建表（已移除各 store 的 ensure_schema()）。
--
-- 幂等：所有语句均使用 IF NOT EXISTS / IF EXISTS，可重复执行，对已有库无破坏。
--   • 新库：CREATE TABLE 直接建出最终状态（列全）。
--   • 老库：CREATE TABLE IF NOT EXISTS 跳过已存在的表，由第 9 节「幂等升级」补列/清理。
--
-- 执行：psql -d <db> -f migrations/schema.sql，或部署脚本调用。
--
-- 注释：表/字段/索引的说明统一用第 10 节的 COMMENT ON 写入数据库元数据（pg_description），
--       psql 的 \d+ 表名、obj_description()/col_description() 可查；本文件正文保持干净。
-- =============================================================================


-- ===========================================================================
-- 1. 用户与第三方身份认证（SSO）
-- ===========================================================================

CREATE TABLE IF NOT EXISTS users (
    id            VARCHAR(36) PRIMARY KEY,
    name          VARCHAR(128) NOT NULL DEFAULT '',
    avatar        VARCHAR(512) NOT NULL DEFAULT '',
    email         VARCHAR(256) NOT NULL DEFAULT '',
    status        SMALLINT     NOT NULL DEFAULT 1,
    is_admin      SMALLINT     NOT NULL DEFAULT 0,
    username      VARCHAR(64),
    password_hash TEXT,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
-- username 唯一索引：PG/MySQL 唯一索引允许多个 NULL，SSO-only 用户(username=NULL)互不冲突
CREATE UNIQUE INDEX IF NOT EXISTS uq_users_username ON users(username);

CREATE TABLE IF NOT EXISTS user_identities (
    id           VARCHAR(36)  PRIMARY KEY,
    provider     VARCHAR(64)  NOT NULL,
    external_id  VARCHAR(128) NOT NULL,
    user_id      VARCHAR(36)  NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         VARCHAR(128) NOT NULL DEFAULT '',
    avatar       VARCHAR(512) NOT NULL DEFAULT '',
    email        VARCHAR(256) NOT NULL DEFAULT '',
    raw_payload  TEXT         NOT NULL DEFAULT '{}',
    linked_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_provider_external UNIQUE (provider, external_id)
);
CREATE INDEX IF NOT EXISTS idx_user_identities_user     ON user_identities(user_id);
CREATE INDEX IF NOT EXISTS idx_user_identities_provider ON user_identities(provider);

-- API Token（账户访问令牌：外部系统以 Authorization: Bearer 调接口，等价登录身份）
-- 明文仅创建时返回一次，库内只存 SHA-256 哈希（不可逆），列表只展示脱敏前缀
CREATE TABLE IF NOT EXISTS api_tokens (
    id           VARCHAR(36)  PRIMARY KEY,
    user_id      VARCHAR(36)  NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         VARCHAR(128) NOT NULL DEFAULT '',
    remark       TEXT         NOT NULL DEFAULT '',
    token_hash   VARCHAR(64)  NOT NULL,
    prefix       VARCHAR(20)  NOT NULL DEFAULT '',
    enabled      SMALLINT     NOT NULL DEFAULT 1,
    valid_from   TIMESTAMPTZ,
    expires_at   TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_api_tokens_hash ON api_tokens(token_hash);
CREATE INDEX IF NOT EXISTS idx_api_tokens_user ON api_tokens(user_id);


-- ===========================================================================
-- 2. 知识库
-- ===========================================================================

CREATE TABLE IF NOT EXISTS kb_doc_meta (
    id          SERIAL PRIMARY KEY,
    doc_id      VARCHAR(128) UNIQUE NOT NULL,
    doc_type    SMALLINT NOT NULL DEFAULT 1,
    brand       VARCHAR(64) NOT NULL DEFAULT '',
    dev_type    VARCHAR(64) NOT NULL DEFAULT '',
    title       VARCHAR(256) NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_kb_doc_meta_type     ON kb_doc_meta(doc_type);
CREATE INDEX IF NOT EXISTS idx_kb_doc_meta_brand    ON kb_doc_meta(brand);
CREATE INDEX IF NOT EXISTS idx_kb_doc_meta_dev_type ON kb_doc_meta(dev_type);

-- 注：kb_doc_meta 为旧表（Dify 时代元数据镜像），新多-provider 架构不再读写，保留供历史数据。

-- 知识库实例（每条 = 一个知识库：provider_kind 区分 Dify 外挂 / 内置 Qdrant）
CREATE TABLE IF NOT EXISTS kb_instances (
    id            VARCHAR(36)  PRIMARY KEY,
    name          VARCHAR(128) NOT NULL,
    provider_kind SMALLINT     NOT NULL,              -- 1=Dify 2=Builtin
    config        TEXT         NOT NULL DEFAULT '{}', -- JSON；secret 字段经 AesCodec 加密
    status        SMALLINT     NOT NULL DEFAULT 1,    -- 1=启用 0=禁用
    creator       VARCHAR(128) NOT NULL DEFAULT 'local',
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    CONSTRAINT chk_kb_instances_kind CHECK (provider_kind IN (1,2))
);
CREATE INDEX IF NOT EXISTS idx_kb_instances_status ON kb_instances(status);

-- 内置 provider 文档元数据（Dify 文档不入此表——实时调 Dify API）
CREATE TABLE IF NOT EXISTS kb_documents (
    id             VARCHAR(36)  PRIMARY KEY,
    kb_instance_id VARCHAR(36)  NOT NULL REFERENCES kb_instances(id) ON DELETE CASCADE,
    doc_type       SMALLINT     NOT NULL DEFAULT 1,   -- 1=手册 2=FAQ
    brand          VARCHAR(64)  NOT NULL DEFAULT '',
    dev_type       VARCHAR(64)  NOT NULL DEFAULT '',
    model          VARCHAR(64)  NOT NULL DEFAULT '',
    firmware_ver   VARCHAR(64)  NOT NULL DEFAULT '',
    title          VARCHAR(256) NOT NULL DEFAULT '',
    source         VARCHAR(32)  NOT NULL DEFAULT 'manual',
    word_count     INTEGER      NOT NULL DEFAULT 0,
    chunk_count    INTEGER      NOT NULL DEFAULT 0,
    status         SMALLINT     NOT NULL DEFAULT 1,
    uploaded_by    VARCHAR(64)  NOT NULL DEFAULT '',
    created_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_kb_documents_instance  ON kb_documents(kb_instance_id);
CREATE INDEX IF NOT EXISTS idx_kb_documents_brand_dev ON kb_documents(kb_instance_id, brand, dev_type);

-- 内置 provider 分段预览（支撑分段预览，避免回查 Qdrant）
CREATE TABLE IF NOT EXISTS kb_chunks (
    id           VARCHAR(36)  PRIMARY KEY,
    document_id  VARCHAR(36)  NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    chunk_index  INTEGER      NOT NULL DEFAULT 0,
    content      TEXT         NOT NULL DEFAULT '',
    word_count   INTEGER      NOT NULL DEFAULT 0,
    header_path  VARCHAR(512) NOT NULL DEFAULT '',
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_kb_chunks_document ON kb_chunks(document_id);


-- ===========================================================================
-- 3. 监控插件（monitor_plugins / monitor_plugin_versions）
-- ===========================================================================

CREATE TABLE IF NOT EXISTS monitor_plugins (
    id              SERIAL PRIMARY KEY,
    plugin_id       VARCHAR(128) UNIQUE NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    active_version  INTEGER,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_monitor_plugins_enabled ON monitor_plugins(enabled);

CREATE TABLE IF NOT EXISTS monitor_plugin_versions (
    id                  SERIAL PRIMARY KEY,
    plugin_id           VARCHAR(128) NOT NULL,
    version             INTEGER NOT NULL,
    source_code         TEXT NOT NULL,
    change_description  TEXT NOT NULL DEFAULT '',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_plugin_version UNIQUE (plugin_id, version)
);
CREATE INDEX IF NOT EXISTS idx_plugin_versions_plugin ON monitor_plugin_versions(plugin_id);


-- ===========================================================================
-- 4. 自定义 / 内置助手（assistants）
-- ===========================================================================

CREATE TABLE IF NOT EXISTS assistants (
    id                VARCHAR(36)   PRIMARY KEY,
    name              VARCHAR(128)  NOT NULL,
    description       TEXT          NOT NULL DEFAULT '',
    avatar            VARCHAR(64)   NOT NULL DEFAULT '🤖',
    kind              SMALLINT      NOT NULL DEFAULT 1,
    agent_type        SMALLINT      NOT NULL DEFAULT 9,
    system_prompt     TEXT          NOT NULL DEFAULT '',
    model_id          VARCHAR(128)  NOT NULL DEFAULT '',
    temperature       DOUBLE PRECISION,
    top_p             DOUBLE PRECISION,
    max_tokens        INTEGER,
    thinking_level    TEXT,
    enabled_tools     TEXT          NOT NULL DEFAULT '[]',
    enabled_mcps      TEXT          NOT NULL DEFAULT '[]',
    knowledge_enabled BOOLEAN       NOT NULL DEFAULT FALSE,
    greeting          TEXT          NOT NULL DEFAULT '',
    share_token       VARCHAR(16)   NOT NULL DEFAULT '',
    fork_count        INTEGER       NOT NULL DEFAULT 0,
    creator           VARCHAR(128)  NOT NULL DEFAULT 'local',
    visibility        SMALLINT      NOT NULL DEFAULT 0,
    sort_order        INTEGER       NOT NULL DEFAULT 0,
    created_at        TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    CONSTRAINT chk_assistants_kind       CHECK (kind IN (0,1)),
    CONSTRAINT chk_assistants_visibility CHECK (visibility IN (0,1,2))
);
CREATE INDEX IF NOT EXISTS idx_assistants_list
    ON assistants (kind, sort_order, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_assistants_explore
    ON assistants (visibility, fork_count DESC, updated_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS uq_assistants_share_token
    ON assistants (share_token) WHERE share_token <> '';


-- ===========================================================================
-- 5. MCP Server（mcp_servers）
-- ===========================================================================

CREATE TABLE IF NOT EXISTS mcp_servers (
    id              VARCHAR(36)   PRIMARY KEY,
    name            VARCHAR(128)  NOT NULL,
    slug            VARCHAR(64)   NOT NULL,
    transport       SMALLINT      NOT NULL,        -- 1=stdio, 2=streamable_http
    endpoint        VARCHAR(1024) NOT NULL,        -- stdio: 命令；http: URL
    args            TEXT          NOT NULL DEFAULT '[]',
    env_enc         TEXT          NOT NULL DEFAULT '',
    env_mask        TEXT          NOT NULL DEFAULT '{}',
    headers_enc     TEXT          NOT NULL DEFAULT '',
    headers_mask    TEXT          NOT NULL DEFAULT '{}',
    status          SMALLINT      NOT NULL DEFAULT 1,
    tool_timeout_secs INT         NOT NULL DEFAULT 60,   -- 单次工具调用超时（秒），界面可配
    created_at      TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_mcp_servers_slug UNIQUE (slug),
    CONSTRAINT chk_mcp_transport CHECK (transport IN (1, 2)),
    CONSTRAINT chk_mcp_status   CHECK (status   IN (0, 1))
);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_status ON mcp_servers(status);


-- ===========================================================================
-- 6. 模型供应商（llm_providers / llm_models）
-- ===========================================================================

CREATE TABLE IF NOT EXISTS llm_providers (
    id            VARCHAR(36) PRIMARY KEY,
    vendor_name   VARCHAR(128) NOT NULL,
    name          VARCHAR(128) NOT NULL,
    base_url      VARCHAR(512) NOT NULL,
    protocol      VARCHAR(16) NOT NULL DEFAULT 'openai_compat',
    encrypted_key TEXT NOT NULL DEFAULT '',
    key_suffix    VARCHAR(8)  NOT NULL DEFAULT '',
    status        SMALLINT    NOT NULL DEFAULT 1,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_llm_providers_status ON llm_providers(status);

CREATE TABLE IF NOT EXISTS llm_models (
    id          VARCHAR(36) PRIMARY KEY,
    provider_id VARCHAR(36) NOT NULL REFERENCES llm_providers(id) ON DELETE CASCADE,
    name        VARCHAR(128) NOT NULL,
    model       VARCHAR(128) NOT NULL,
    is_default  BOOLEAN     NOT NULL DEFAULT FALSE,
    status      SMALLINT    NOT NULL DEFAULT 1,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_provider_model UNIQUE (provider_id, model)
);
-- 全局至多一个默认模型（部分唯一索引）
CREATE UNIQUE INDEX IF NOT EXISTS uq_llm_models_default
    ON llm_models (is_default) WHERE is_default = TRUE;
CREATE INDEX IF NOT EXISTS idx_llm_models_provider ON llm_models(provider_id);


-- ===========================================================================
-- 7. 会话级配置（合并大表 session_settings）
--    一行一个会话，收纳标题 + 模型绑定 + 思考级别 + 沙箱/审批 + 助手绑定。
--    取代旧的 4 张小表（session_models / session_assistants /
--    session_thinking_levels / session_permission_policies）。
--    title / agent_type 物化落列，供会话列表 SQL 排序/筛选/分页直接连表。
-- ===========================================================================

CREATE TABLE IF NOT EXISTS session_settings (
    session_id      VARCHAR(64)  PRIMARY KEY,
    user_id         VARCHAR(128) NOT NULL DEFAULT '',   -- 会话归属用户（列表按用户隔离）
    title           TEXT         NOT NULL DEFAULT '',
    agent_type      VARCHAR(32)  NOT NULL DEFAULT 'custom',
    model_id        VARCHAR(64),                 -- NULL=未绑定具体模型（default/auto/空 → 运行时解析全局默认）
    thinking_level  TEXT         NOT NULL DEFAULT 'high',
    sandbox_mode    TEXT         NOT NULL DEFAULT 'workspace-write',
    approval_policy TEXT         NOT NULL DEFAULT 'unless-trusted',
    assistant_id    VARCHAR(64),                 -- NULL=未绑定助手
    updated_at      TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    CONSTRAINT chk_settings_thinking CHECK (thinking_level IN ('low','medium','high','xhigh','max')),
    CONSTRAINT chk_settings_sandbox  CHECK (sandbox_mode IN ('read-only','workspace-write','danger-full-access')),
    CONSTRAINT chk_settings_approval CHECK (approval_policy IN ('never','on-request','on-request-rule-request-permission','unless-trusted'))
);
-- 会话列表按用户过滤 + 创建时间倒序（session_id 为 UUID v7，前 48 位即创建毫秒，字符串倒序=创建倒序）
CREATE INDEX IF NOT EXISTS idx_session_settings_user_id_desc ON session_settings (user_id, session_id DESC);
-- 列表按 kind 筛选时连 assistants；按 assistant_id 过滤走此列
CREATE INDEX IF NOT EXISTS idx_session_settings_assistant ON session_settings (assistant_id);


-- ===========================================================================
-- 8. Shell 规则（shell_rules）
-- ===========================================================================

CREATE TABLE IF NOT EXISTS shell_rules (
    id         VARCHAR(36) PRIMARY KEY,
    pattern    VARCHAR(512) NOT NULL,
    decision   SMALLINT     NOT NULL CHECK (decision IN (0,1,2)),
    priority   INT          NOT NULL DEFAULT 0,
    enabled    SMALLINT     NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
    created_at TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);


-- ===========================================================================
-- 8b. 跨会话记忆（memories / memory_proposals）
-- ===========================================================================
-- 用户级（scope=0，默认，跨所有助手共享）+ 助手级（scope=1，仅该助手可见）两级记忆。
-- 会话不作为隔离维度（会话内信息走 conversation_history），仅作来源标记（source_session_id）。
-- 写入流程：agent 调 propose_memory 工具 → memory_proposals(status=0 待确认) →
--           前端「建议记忆」卡片确认 → accept 转正写入 memories / reject 标记忽略。
-- 注入流程：每会话构建 stable prefix 时，按 user_id + 当前 assistant_id 拉取记忆拼入
--           （scope=0 全部 + scope=1 且 assistant_id 命中）。

CREATE TABLE IF NOT EXISTS memories (
    id                VARCHAR(36)  PRIMARY KEY,
    user_id           VARCHAR(36)  NOT NULL,
    scope             SMALLINT     NOT NULL DEFAULT 0,   -- 0=用户级(跨助手共享) 1=助手级(仅该助手)
    assistant_id      VARCHAR(36),                       -- scope=1 时填; scope=0 时空
    type              SMALLINT     NOT NULL DEFAULT 0,   -- 0=习惯/偏好 1=坑/避坑
    content           TEXT         NOT NULL,
    source_session_id VARCHAR(64),                       -- 来源会话(溯源用,非隔离维度)
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    CONSTRAINT chk_memories_scope CHECK (scope IN (0,1)),
    CONSTRAINT chk_memories_type  CHECK (type  IN (0,1))
);
CREATE INDEX IF NOT EXISTS idx_memories_user
    ON memories(user_id);
CREATE INDEX IF NOT EXISTS idx_memories_assistant
    ON memories(user_id, assistant_id) WHERE scope = 1;

CREATE TABLE IF NOT EXISTS memory_proposals (
    id            VARCHAR(36)  PRIMARY KEY,
    user_id       VARCHAR(36)  NOT NULL,
    session_id    VARCHAR(64)  NOT NULL,
    assistant_id  VARCHAR(36),                       -- 提议时的助手(scope=1 时填)
    scope         SMALLINT     NOT NULL DEFAULT 0,   -- 0=用户级 1=助手级
    type          SMALLINT     NOT NULL DEFAULT 0,   -- 0=习惯 1=坑
    content       TEXT         NOT NULL,
    reason        TEXT         NOT NULL DEFAULT '',  -- agent 给出的「为什么值得记」
    status        SMALLINT     NOT NULL DEFAULT 0,   -- 0=待确认 1=已加入 2=已忽略
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    CONSTRAINT chk_proposals_scope  CHECK (scope  IN (0,1)),
    CONSTRAINT chk_proposals_type   CHECK (type   IN (0,1)),
    CONSTRAINT chk_proposals_status CHECK (status IN (0,1,2))
);
CREATE INDEX IF NOT EXISTS idx_proposals_user
    ON memory_proposals(user_id, status);
CREATE INDEX IF NOT EXISTS idx_proposals_session
    ON memory_proposals(session_id);


-- 8c. 审计日志
CREATE TABLE IF NOT EXISTS audit_logs (
    id         VARCHAR(36)  PRIMARY KEY,
    user_id    VARCHAR(36)  NOT NULL DEFAULT '',     -- 操作者 user_id（失败登录为空）
    actor      VARCHAR(128) NOT NULL DEFAULT '',     -- 显示名/username（失败登录记 username）
    source     VARCHAR(32)  NOT NULL DEFAULT 'web',  -- 来源：web / api_token
    operation  VARCHAR(128) NOT NULL,                -- mutation 名(deleteSession…)或 REST 动作(login/upload_image…)
    target_id  VARCHAR(128) NOT NULL DEFAULT '',     -- 被操作对象 id（从参数提取）
    success    SMALLINT     NOT NULL DEFAULT 1,      -- 1=成功 0=失败（GraphQL 执行层）
    detail     TEXT         NOT NULL DEFAULT '{}',   -- 脱敏后参数 JSON
    ip         VARCHAR(64)  NOT NULL DEFAULT '',     -- 请求 IP（x-forwarded-for / x-real-ip）
    created_at TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_audit_logs_user    ON audit_logs(user_id);
CREATE INDEX IF NOT EXISTS idx_audit_logs_created ON audit_logs(created_at);

-- ===========================================================================
-- 9. 幂等升级（老库补列 / 清理废弃列；新库执行时均安全跳过）
--    新库由上方 CREATE TABLE 直接建出最终状态，本段仅用于老库兼容升级。
-- ===========================================================================

-- users 历史增量列
ALTER TABLE users ADD COLUMN IF NOT EXISTS is_admin      SMALLINT     NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN IF NOT EXISTS username      VARCHAR(64);
ALTER TABLE users ADD COLUMN IF NOT EXISTS password_hash TEXT;

-- llm_providers 历史增量列
ALTER TABLE llm_providers ADD COLUMN IF NOT EXISTS protocol VARCHAR(16) NOT NULL DEFAULT 'openai_compat';

-- assistants 历史增量列
ALTER TABLE assistants ADD COLUMN IF NOT EXISTS thinking_level TEXT;
ALTER TABLE assistants ADD COLUMN IF NOT EXISTS enabled_mcps   TEXT NOT NULL DEFAULT '[]';

-- MCP Server：工具调用超时（秒），界面可配，默认 60
ALTER TABLE mcp_servers ADD COLUMN IF NOT EXISTS tool_timeout_secs INT NOT NULL DEFAULT 60;

-- monitor_plugin_versions 历史增量列
ALTER TABLE monitor_plugin_versions ADD COLUMN IF NOT EXISTS change_description TEXT NOT NULL DEFAULT '';

-- 多 Agent 编排功能已移除：清理遗留列
ALTER TABLE assistants DROP COLUMN IF EXISTS sub_agent_ids;
ALTER TABLE assistants DROP COLUMN IF EXISTS orchestration;

-- 助手绑定知识库实例（多-provider 知识库；空=不绑定，旧 knowledge_enabled 保留兼容）
ALTER TABLE assistants ADD COLUMN IF NOT EXISTS kb_instance_id VARCHAR(36);

-- kb_documents：设备型号（如 S5300），可选，用于检索过滤
ALTER TABLE kb_documents ADD COLUMN IF NOT EXISTS model VARCHAR(64) NOT NULL DEFAULT '';

-- llm_models：能力标签 tags（多选 JSON 数组，可扩展：chat/embedding/rerank/reasoning/vision…）
ALTER TABLE llm_models ADD COLUMN IF NOT EXISTS tags TEXT NOT NULL DEFAULT '["chat"]';
ALTER TABLE llm_models ADD COLUMN IF NOT EXISTS embedding_dimensions INT;  -- tags 含 embedding 时填（如 bge-m3=1024）
ALTER TABLE llm_models ADD COLUMN IF NOT EXISTS embedding_default   BOOLEAN  NOT NULL DEFAULT FALSE;
-- 上下文窗口（token），用于动态压缩阈值；空=回退配置默认（fallback_context_window）
ALTER TABLE llm_models ADD COLUMN IF NOT EXISTS context_window INT;
-- 全局至多一个默认 embedding 模型
CREATE UNIQUE INDEX IF NOT EXISTS uq_llm_models_embedding_default
    ON llm_models (embedding_default) WHERE embedding_default = TRUE;

-- 会话级配置合并：旧的 4 张小表作废清空，统一由 session_settings 取代
DROP TABLE IF EXISTS session_models;
DROP TABLE IF EXISTS session_assistants;
DROP TABLE IF EXISTS session_thinking_levels;
DROP TABLE IF EXISTS session_permission_policies;

-- session_settings 历史增量列：归属用户（老库已有本表时补列）
ALTER TABLE session_settings ADD COLUMN IF NOT EXISTS user_id VARCHAR(128) NOT NULL DEFAULT '';

-- adk 历史会话数据清空：sessions + events 一并删除（用户明确授权，全部从零开始）。
-- 这两张表由 adk PostgresSessionService 首次使用时自动 CREATE 重建为空表，无需手动重建。
DROP TABLE IF EXISTS events;
DROP TABLE IF EXISTS sessions;


-- ===========================================================================
-- 10. 数据库注释（COMMENT ON，写入 PG 元数据）
-- ===========================================================================
-- 用途：
--   将注释写入 PostgreSQL 系统目录（pg_description），psql 的 \d+ 表名、
--   或 obj_description()/col_description() 即可查到——数据库本身即文档。
--   正文里的 -- 注释只存在于源文件，DB 内不可见；本节才是「入库注释」。
--
-- 幂等：COMMENT 天然覆盖式，重复执行只刷新注释，无需 IF NOT EXISTS，可安全反复执行。
--
-- 约定：status 字段跨表复用同一语义——0=禁用 1=启用。
-- ===========================================================================

-- ---------------------------------------------------------------------------
-- 10.1 用户与第三方身份认证
-- ---------------------------------------------------------------------------
COMMENT ON TABLE users IS '系统用户主表（本地账号 + SSO 登录后的用户记录；首个本地用户可 bootstrap 为管理员）';
COMMENT ON COLUMN users.id IS '用户唯一标识（UUID v4）';
COMMENT ON COLUMN users.name IS '显示名（昵称）';
COMMENT ON COLUMN users.avatar IS '头像 URL';
COMMENT ON COLUMN users.email IS '邮箱（SSO 同步或本地填写）';
COMMENT ON COLUMN users.status IS '启用状态：0=禁用 1=启用（默认）；禁用用户登录被拒';
COMMENT ON COLUMN users.is_admin IS '是否管理员：0=普通用户（默认）1=管理员';
COMMENT ON COLUMN users.username IS '登录用户名；SSO-only 用户为 NULL（唯一索引允许多 NULL）';
COMMENT ON COLUMN users.password_hash IS '本地账号密码哈希；SSO 用户为 NULL';
COMMENT ON COLUMN users.created_at IS '创建时间';
COMMENT ON COLUMN users.updated_at IS '更新时间';
COMMENT ON INDEX uq_users_username IS 'username 唯一索引；PG/MySQL 唯一索引允许多个 NULL，SSO-only 用户互不冲突';

COMMENT ON TABLE user_identities IS '第三方身份绑定表（一个用户可绑定多个第三方身份；provider 取值 feishu/wechat/oidc）';
COMMENT ON COLUMN user_identities.id IS '绑定记录唯一标识';
COMMENT ON COLUMN user_identities.provider IS '身份提供商：feishu（飞书）/ wechat（微信）/ oidc（通用 OIDC）';
COMMENT ON COLUMN user_identities.external_id IS '第三方平台返回的用户唯一 ID';
COMMENT ON COLUMN user_identities.user_id IS '关联用户；用户删除时级联删除其所有绑定';
COMMENT ON COLUMN user_identities.name IS '同步自第三方的昵称快照';
COMMENT ON COLUMN user_identities.avatar IS '同步自第三方的头像快照';
COMMENT ON COLUMN user_identities.email IS '同步自第三方的邮箱快照';
COMMENT ON COLUMN user_identities.raw_payload IS '第三方返回的原始 payload（JSON 字符串，便于排查）';
COMMENT ON COLUMN user_identities.linked_at IS '首次绑定时间';
COMMENT ON INDEX idx_user_identities_user IS '按用户查其绑定的所有第三方身份';
COMMENT ON INDEX idx_user_identities_provider IS '按平台筛选（统计/管理）';

-- API Token（账户访问令牌）
COMMENT ON TABLE api_tokens IS '账户 API Token（访问令牌）：外部系统以 Authorization: Bearer 调接口，等价登录身份；明文仅创建时返回，库内仅存 SHA-256 哈希';
COMMENT ON COLUMN api_tokens.id IS '令牌唯一标识（UUID v7，应用层生成）';
COMMENT ON COLUMN api_tokens.user_id IS '所属用户；用户删除时级联删除其全部令牌';
COMMENT ON COLUMN api_tokens.name IS '令牌名称（标识用途，如「数据看板接入」）';
COMMENT ON COLUMN api_tokens.remark IS '备注（可选）';
COMMENT ON COLUMN api_tokens.token_hash IS '明文令牌的 SHA-256 哈希（64 位 hex）；验证时对输入做同样哈希后查本列，明文不入库';
COMMENT ON COLUMN api_tokens.prefix IS '明文令牌前缀（脱敏辨识，如 cxat_aB3dXy）；列表展示用，无法还原令牌';
COMMENT ON COLUMN api_tokens.enabled IS '启用状态：0=禁用 1=启用（默认）；禁用的令牌验证失败';
COMMENT ON COLUMN api_tokens.valid_from IS '生效起始时间；NULL=创建即生效';
COMMENT ON COLUMN api_tokens.expires_at IS '过期时间；NULL=永不过期';
COMMENT ON COLUMN api_tokens.last_used_at IS '最近一次通过本令牌鉴权的时间；NULL=从未使用';
COMMENT ON COLUMN api_tokens.created_at IS '创建时间';
COMMENT ON COLUMN api_tokens.updated_at IS '更新时间';
COMMENT ON INDEX uq_api_tokens_hash IS '令牌哈希唯一索引；同时作为 Bearer 验证的查找键（O(1)）';
COMMENT ON INDEX idx_api_tokens_user IS '按用户列其全部令牌';

-- ---------------------------------------------------------------------------
-- 10.2 知识库
-- ---------------------------------------------------------------------------
COMMENT ON TABLE kb_doc_meta IS '知识库文档元数据表（PG 侧索引，文档正文存于 Dify 平台；doc_id 为 Dify 文档 ID 用于映射）';
COMMENT ON COLUMN kb_doc_meta.id IS '自增主键';
COMMENT ON COLUMN kb_doc_meta.doc_id IS 'Dify 知识库文档 ID（外部唯一标识）';
COMMENT ON COLUMN kb_doc_meta.doc_type IS '文档类型：1=上传手册（默认）2=FAQ（会话自动学习生成）';
COMMENT ON COLUMN kb_doc_meta.brand IS '厂商（如华为/思科），用于检索过滤';
COMMENT ON COLUMN kb_doc_meta.dev_type IS '设备类型（如交换机/路由器），用于检索过滤';
COMMENT ON COLUMN kb_doc_meta.title IS '文档标题（手册文件名 / FAQ 主题）';
COMMENT ON COLUMN kb_doc_meta.created_at IS '入库时间';
COMMENT ON INDEX idx_kb_doc_meta_type IS '按文档类型检索';
COMMENT ON INDEX idx_kb_doc_meta_brand IS '按厂商过滤';
COMMENT ON INDEX idx_kb_doc_meta_dev_type IS '按设备类型过滤';

-- ---------------------------------------------------------------------------
-- 10.3 监控插件
-- ---------------------------------------------------------------------------
COMMENT ON TABLE monitor_plugins IS '监控插件主表（插件元信息 + 当前激活版本；仅 enabled=TRUE 且 active_version 非空的插件会被加载执行）';
COMMENT ON COLUMN monitor_plugins.id IS '自增主键';
COMMENT ON COLUMN monitor_plugins.plugin_id IS '插件业务标识（全局唯一）';
COMMENT ON COLUMN monitor_plugins.description IS '插件整体说明';
COMMENT ON COLUMN monitor_plugins.active_version IS '当前激活版本号（指向 monitor_plugin_versions.version）；NULL=尚无激活版本';
COMMENT ON COLUMN monitor_plugins.enabled IS '是否启用';
COMMENT ON COLUMN monitor_plugins.created_at IS '创建时间';
COMMENT ON COLUMN monitor_plugins.updated_at IS '更新时间';
COMMENT ON INDEX idx_monitor_plugins_enabled IS '列出启用的插件';

COMMENT ON TABLE monitor_plugin_versions IS '插件版本历史表（每个版本保存一份 Rhai 源码；主表 active_version 指向本表 version）';
COMMENT ON COLUMN monitor_plugin_versions.id IS '自增主键';
COMMENT ON COLUMN monitor_plugin_versions.plugin_id IS '所属插件';
COMMENT ON COLUMN monitor_plugin_versions.version IS '版本号（单调递增整数）';
COMMENT ON COLUMN monitor_plugin_versions.source_code IS 'Rhai 脚本源码（运行时由 Rhai 引擎解释执行）';
COMMENT ON COLUMN monitor_plugin_versions.change_description IS '本次发版变更说明（区别于主表的整体 description）';
COMMENT ON COLUMN monitor_plugin_versions.created_at IS '发版时间';
COMMENT ON INDEX idx_plugin_versions_plugin IS '按插件列出版本历史';

-- ---------------------------------------------------------------------------
-- 10.4 助手
-- ---------------------------------------------------------------------------
COMMENT ON TABLE assistants IS '助手定义表（内置助手 kind=0 只读 + 自定义助手 kind=1 可编辑/Fork；广场只展示 visibility>0）';
COMMENT ON COLUMN assistants.id IS '助手唯一标识（UUID）';
COMMENT ON COLUMN assistants.name IS '助手名称';
COMMENT ON COLUMN assistants.description IS '助手描述';
COMMENT ON COLUMN assistants.avatar IS '头像（emoji 或图片名）';
COMMENT ON COLUMN assistants.kind IS '类型：0=内置（只读）1=自定义（默认）';
COMMENT ON COLUMN assistants.agent_type IS '内置 Agent 调度类型：2=设备命令 4=监控插件 9=自定义（默认；0/1 已废弃按自定义处理）';
COMMENT ON COLUMN assistants.system_prompt IS '系统提示词';
COMMENT ON COLUMN assistants.model_id IS '绑定的模型 id；空=走会话/全局默认';
COMMENT ON COLUMN assistants.temperature IS '采样温度，clamp 0.0~2.0；NULL=默认 0.3';
COMMENT ON COLUMN assistants.top_p IS 'nucleus sampling，clamp 0.0~1.0；NULL=模型/API 默认';
COMMENT ON COLUMN assistants.max_tokens IS '最大输出 token，clamp 16384~32768；NULL=默认 16384（高思考级别从中扣 thinking budget）';
COMMENT ON COLUMN assistants.thinking_level IS '思考级别：low/medium/high/xhigh/max；NULL/空=不发送（走模型默认）';
COMMENT ON COLUMN assistants.enabled_tools IS '启用工具 key 列表（JSON 数组：search_kb/query_device_catalog/shell_command）';
COMMENT ON COLUMN assistants.enabled_mcps IS '启用的 MCP Server id 列表（JSON 数组）';
COMMENT ON COLUMN assistants.knowledge_enabled IS '是否启用知识库检索';
COMMENT ON COLUMN assistants.greeting IS '进入助手时展示的欢迎语';
COMMENT ON COLUMN assistants.share_token IS '分享口令（8 位，剔除易混淆字符）；空=未分享，非空靠部分唯一索引全局唯一用于 Fork';
COMMENT ON COLUMN assistants.fork_count IS '被其他用户 Fork 的次数（广场排序用）';
COMMENT ON COLUMN assistants.creator IS '创建者标识（用户名/id；内置与系统 seed 为 local）';
COMMENT ON COLUMN assistants.visibility IS '可见性：0=私有（默认）1=广场公开 2=内置公开';
COMMENT ON COLUMN assistants.sort_order IS '列表排序权重（列表按 kind, sort_order, updated_at 排序）';
COMMENT ON COLUMN assistants.created_at IS '创建时间';
COMMENT ON COLUMN assistants.updated_at IS '更新时间';
COMMENT ON INDEX idx_assistants_list IS '助手列表查询索引（分类 kind + 排序 sort_order, updated_at）';
COMMENT ON INDEX idx_assistants_explore IS '广场探索列表索引（仅公开 + 按 fork_count 热度排）';
COMMENT ON INDEX uq_assistants_share_token IS '分享口令全局唯一（部分唯一索引，跳过空口令避免大量空值冲突）';

-- ---------------------------------------------------------------------------
-- 10.5 MCP Server
-- ---------------------------------------------------------------------------
COMMENT ON TABLE mcp_servers IS 'MCP Server 配置表（接入外部工具集；敏感字段 env/headers 采用 AES-256-GCM 加密存储 + 掩码展示）';
COMMENT ON COLUMN mcp_servers.id IS 'MCP Server 唯一标识（UUID）';
COMMENT ON COLUMN mcp_servers.name IS '显示名';
COMMENT ON COLUMN mcp_servers.slug IS 'URL 友好标识（全局唯一）';
COMMENT ON COLUMN mcp_servers.transport IS '传输方式：1=stdio（子进程）2=streamable_http（远程）';
COMMENT ON COLUMN mcp_servers.endpoint IS 'stdio: 可执行命令名（禁 shell 元字符）；http: 完整 URL';
COMMENT ON COLUMN mcp_servers.args IS 'stdio 启动参数列表（JSON 数组）；http 留空';
COMMENT ON COLUMN mcp_servers.env_enc IS '环境变量密文（AES-256-GCM 加密的 JSON map）；空 map 时为空串';
COMMENT ON COLUMN mcp_servers.env_mask IS '环境变量掩码（脱敏 JSON，回前端用，明文不外泄）';
COMMENT ON COLUMN mcp_servers.headers_enc IS 'HTTP 自定义请求头密文（同 env 加密）';
COMMENT ON COLUMN mcp_servers.headers_mask IS 'HTTP 请求头掩码（同 env 掩码）';
COMMENT ON COLUMN mcp_servers.status IS '启用状态：0=禁用 1=启用（默认）';
COMMENT ON COLUMN mcp_servers.created_at IS '创建时间';
COMMENT ON COLUMN mcp_servers.updated_at IS '更新时间';
COMMENT ON INDEX idx_mcp_servers_status IS '按状态筛选启用的 MCP Server';

-- ---------------------------------------------------------------------------
-- 10.6 模型供应商
-- ---------------------------------------------------------------------------
COMMENT ON TABLE llm_providers IS '模型供应商表（API 接入配置；protocol 决定走哪条客户端链路，API Key 加密存储）';
COMMENT ON COLUMN llm_providers.id IS '供应商实例唯一标识（UUID）';
COMMENT ON COLUMN llm_providers.vendor_name IS '供应商品牌名（如 DeepSeek/OpenAI）';
COMMENT ON COLUMN llm_providers.name IS '实例显示名（用户自定义）';
COMMENT ON COLUMN llm_providers.base_url IS 'API 基础地址';
COMMENT ON COLUMN llm_providers.protocol IS '协议：openai_compat（默认，OpenAI 兼容）/ anthropic（Anthropic Messages）';
COMMENT ON COLUMN llm_providers.encrypted_key IS 'API Key 密文（AES-256-GCM，base64(nonce+ciphertext+tag)）；空=未配置';
COMMENT ON COLUMN llm_providers.key_suffix IS 'API Key 末 4 位明文（前端识别用）；key<=4 位时为 ****';
COMMENT ON COLUMN llm_providers.status IS '启用状态：0=禁用 1=启用（默认）';
COMMENT ON COLUMN llm_providers.created_at IS '创建时间';
COMMENT ON COLUMN llm_providers.updated_at IS '更新时间';
COMMENT ON INDEX idx_llm_providers_status IS '按状态筛选启用的供应商';

COMMENT ON TABLE llm_models IS '模型表（归属于某供应商；全局至多一个默认模型）';
COMMENT ON COLUMN llm_models.id IS '模型唯一标识（UUID）';
COMMENT ON COLUMN llm_models.provider_id IS '所属供应商；供应商删除时级联删除其模型';
COMMENT ON COLUMN llm_models.name IS '模型显示名（人类可读，用户自定义）';
COMMENT ON COLUMN llm_models.model IS 'API 模型 ID（发往供应商的真实标识，如 deepseek-chat）';
COMMENT ON COLUMN llm_models.is_default IS '是否全局默认模型；配合部分唯一索引保证全局至多一个';
COMMENT ON COLUMN llm_models.status IS '启用状态：0=禁用 1=启用（默认）';
COMMENT ON COLUMN llm_models.created_at IS '创建时间';
COMMENT ON COLUMN llm_models.updated_at IS '更新时间';
COMMENT ON INDEX uq_llm_models_default IS '全局至多一个默认模型（部分唯一索引，仅 is_default=TRUE 行参与）';
COMMENT ON INDEX idx_llm_models_provider IS '按供应商列出其模型';

-- ---------------------------------------------------------------------------
-- 10.7 会话级配置（合并大表 session_settings）
-- ---------------------------------------------------------------------------
COMMENT ON TABLE session_settings IS '会话级配置合并大表（标题+模型+思考级别+沙箱审批+助手绑定；title/agent_type 物化供列表 SQL 排序筛选分页）';
COMMENT ON COLUMN session_settings.session_id IS '会话 ID（UUID v7，前 48 位即创建毫秒；列表按其字符串倒序=创建时间倒序）';
COMMENT ON COLUMN session_settings.user_id IS '会话归属用户（列表按用户隔离；与 adk sessions.user_id 对齐）';
COMMENT ON COLUMN session_settings.title IS '会话标题（重命名/首轮默认标题落列；空串=未设置，前端回退显示）';
COMMENT ON COLUMN session_settings.agent_type IS '会话所属 agent 类型（物化自会话 state，供列表筛选）';
COMMENT ON COLUMN session_settings.model_id IS '绑定模型 id；NULL=未绑定具体模型（default/auto/空 → 运行时解析全局默认）';
COMMENT ON COLUMN session_settings.thinking_level IS '思考级别：low/medium/high/xhigh/max（默认 high）';
COMMENT ON COLUMN session_settings.sandbox_mode IS '沙箱模式：read-only 只读 / workspace-write 工作区写（默认）/ danger-full-access 完全访问';
COMMENT ON COLUMN session_settings.approval_policy IS '审批策略：never 从不 / on-request 请求时 / on-request-rule-request-permission / unless-trusted 除可信外都审批（默认）';
COMMENT ON COLUMN session_settings.assistant_id IS '绑定助手 id（SSE 未带 assistant_id 时从本表兜底读）；NULL=未绑定';
COMMENT ON COLUMN session_settings.updated_at IS '更新时间（任意会话级配置变更时刷新）';
COMMENT ON INDEX idx_session_settings_user_id_desc IS '会话列表按用户过滤 + 创建时间倒序（UUID v7 字符串倒序）';
COMMENT ON INDEX idx_session_settings_assistant IS '按绑定助手过滤会话';

-- ---------------------------------------------------------------------------
-- 10.8 Shell 规则
-- ---------------------------------------------------------------------------
COMMENT ON TABLE shell_rules IS 'Shell 命令审批规则表（命令匹配到自动决策；仅 enabled=1 的规则进内存缓存，按 priority 降序遍历返回首个命中）';
COMMENT ON COLUMN shell_rules.id IS '规则唯一标识（UUID）';
COMMENT ON COLUMN shell_rules.pattern IS 'glob 匹配模式（支持 * ?，大小写敏感），对命令字符串匹配';
COMMENT ON COLUMN shell_rules.decision IS '决策：0=放行(Allow) 1=阻断(Deny) 2=需审批(Ask)';
COMMENT ON COLUMN shell_rules.priority IS '优先级（高优先先匹配；同优先按 created_at）';
COMMENT ON COLUMN shell_rules.enabled IS '1=启用（默认）0=禁用';
COMMENT ON COLUMN shell_rules.created_at IS '创建时间';

-- ---------------------------------------------------------------------------
-- 10.9 跨会话记忆
-- ---------------------------------------------------------------------------
COMMENT ON TABLE memories IS '已确认的跨会话记忆（用户级 scope=0 跨助手共享 / 助手级 scope=1 仅该助手；会话不参与隔离，仅 source_session_id 溯源）';
COMMENT ON COLUMN memories.id IS '记忆唯一标识（UUID v7）';
COMMENT ON COLUMN memories.user_id IS '所属用户（记忆按用户隔离）';
COMMENT ON COLUMN memories.scope IS '作用域：0=用户级（默认，跨所有助手共享）1=助手级（仅 assistant_id 命中时注入）';
COMMENT ON COLUMN memories.assistant_id IS 'scope=1 时绑定的助手 id；scope=0 时为 NULL';
COMMENT ON COLUMN memories.type IS '记忆类型：0=习惯/偏好 1=坑/避坑';
COMMENT ON COLUMN memories.content IS '记忆正文（自由文本，注入 system prompt 给 agent）';
COMMENT ON COLUMN memories.source_session_id IS '产生该记忆的来源会话（仅溯源/管理用，不作为隔离维度）';
COMMENT ON COLUMN memories.created_at IS '创建时间';
COMMENT ON COLUMN memories.updated_at IS '更新时间';
COMMENT ON INDEX idx_memories_user IS '按用户拉取其全部用户级记忆（注入 stable prefix）';
COMMENT ON INDEX idx_memories_assistant IS '按用户+助手拉取助手级记忆（scope=1 部分索引）';

COMMENT ON TABLE memory_proposals IS '记忆建议（agent 通过 propose_memory 工具产出，待用户在卡片上确认；accept 后转正入 memories）';
COMMENT ON COLUMN memory_proposals.id IS '建议唯一标识（UUID v7）';
COMMENT ON COLUMN memory_proposals.user_id IS '所属用户';
COMMENT ON COLUMN memory_proposals.session_id IS '产生建议的会话';
COMMENT ON COLUMN memory_proposals.assistant_id IS '提议时的助手（scope=1 时填）';
COMMENT ON COLUMN memory_proposals.scope IS '建议的作用域：0=用户级 1=助手级';
COMMENT ON COLUMN memory_proposals.type IS '建议类型：0=习惯 1=坑';
COMMENT ON COLUMN memory_proposals.content IS '建议记忆正文';
COMMENT ON COLUMN memory_proposals.reason IS 'agent 给出的「为什么值得记」理由（展示在卡片上帮助用户判断）';
COMMENT ON COLUMN memory_proposals.status IS '状态：0=待确认 1=已加入（转正写入 memories）2=已忽略';
COMMENT ON COLUMN memory_proposals.created_at IS '创建时间';
COMMENT ON INDEX idx_proposals_user IS '按用户查待确认建议（卡片列表）';
COMMENT ON INDEX idx_proposals_session IS '按会话查其产生的建议';
