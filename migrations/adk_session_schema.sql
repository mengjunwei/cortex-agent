-- ============================================================================
-- adk-rust PostgreSQL Session 后端建表脚本（v1 baseline）
-- 对应 adk-rust (rev fa633c5) 的 PostgresSessionService::migrate()
-- 来源：adk-session/src/postgres.rs PG_SESSION_MIGRATIONS + adk-session/src/migration.rs
--
-- 用途：新环境部署时，若 cortex 启动日志显示
--   "[infra] Postgres session 初始化失败，降级为 InMemory"
-- 且 DB 里缺少这些表，则手动执行本脚本建表。建表后重启 cortex，
-- 日志应显示 "[infra] adk session service: PostgreSQL 持久化"，
-- 会话即可跨进程重启持久化。
--
-- 执行：psql -U <user> -d <db> -f migrations/adk_session_schema.sql
--        或在 DB 客户端里整段执行。
-- ============================================================================

-- 1. sessions：会话主表
--    主键 (app_name, user_id, session_id)；cortex 的 app_name 恒为 "cortex-agent"
CREATE TABLE IF NOT EXISTS sessions (
    app_name   TEXT NOT NULL,
    user_id    TEXT NOT NULL,
    session_id TEXT NOT NULL,
    state      JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_name, user_id, session_id)
);

-- 2. events：会话事件（对话历史 / 工具调用 / 压缩检查点等）
--    外键关联 sessions，ON DELETE CASCADE（会话删除时事件级联清除）
CREATE TABLE IF NOT EXISTS events (
    id                    TEXT NOT NULL,
    app_name              TEXT NOT NULL,
    user_id               TEXT NOT NULL,
    session_id            TEXT NOT NULL,
    invocation_id         TEXT NOT NULL,
    branch                TEXT NOT NULL,
    author                TEXT NOT NULL,
    timestamp             TIMESTAMPTZ NOT NULL,
    llm_response          JSONB NOT NULL,
    actions               JSONB NOT NULL,
    long_running_tool_ids JSONB NOT NULL,
    PRIMARY KEY (id, app_name, user_id, session_id),
    FOREIGN KEY (app_name, user_id, session_id)
        REFERENCES sessions (app_name, user_id, session_id)
        ON DELETE CASCADE
);

-- 3. app_states：应用级共享状态（全 app 单例）
CREATE TABLE IF NOT EXISTS app_states (
    app_name   TEXT PRIMARY KEY,
    state      JSONB NOT NULL DEFAULT '{}',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- 4. user_states：用户级状态（按 app_name + user_id）
CREATE TABLE IF NOT EXISTS user_states (
    app_name   TEXT NOT NULL,
    user_id    TEXT NOT NULL,
    state      JSONB NOT NULL DEFAULT '{}',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_name, user_id)
);

-- 索引（加速 list / 历史加载）
CREATE INDEX IF NOT EXISTS idx_sessions_app_user ON sessions (app_name, user_id);
CREATE INDEX IF NOT EXISTS idx_events_session_ts  ON events (session_id, timestamp);

-- 5. 迁移注册表：标记 v1 已应用
--    避免 cortex 启动时 migrate() 的 baseline detection 误判 / 重复执行
CREATE TABLE IF NOT EXISTS _adk_session_migrations (
    version     BIGINT PRIMARY KEY,
    description TEXT NOT NULL,
    applied_at  TEXT NOT NULL
);

INSERT INTO _adk_session_migrations (version, description, applied_at)
VALUES (1, 'create initial session tables', NOW()::TEXT)
ON CONFLICT (version) DO NOTHING;
