--
-- PostgreSQL database dump
--


-- Dumped from database version 16.14
-- Dumped by pg_dump version 16.14

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: api_tokens; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.api_tokens (
    id character varying(36) NOT NULL,
    user_id character varying(36) NOT NULL,
    name character varying(128) DEFAULT ''::character varying NOT NULL,
    remark text DEFAULT ''::text NOT NULL,
    token_hash character varying(64) NOT NULL,
    prefix character varying(20) DEFAULT ''::character varying NOT NULL,
    enabled smallint DEFAULT 1 NOT NULL,
    valid_from timestamp with time zone,
    expires_at timestamp with time zone,
    last_used_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: TABLE api_tokens; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.api_tokens IS '账户 API Token（访问令牌）：外部系统以 Authorization: Bearer 调接口，等价登录身份；明文仅创建时返回，库内仅存 SHA-256 哈希';


--
-- Name: COLUMN api_tokens.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.id IS '令牌唯一标识（UUID v7，应用层生成）';


--
-- Name: COLUMN api_tokens.user_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.user_id IS '所属用户；用户删除时级联删除其全部令牌';


--
-- Name: COLUMN api_tokens.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.name IS '令牌名称（标识用途，如「数据看板接入」）';


--
-- Name: COLUMN api_tokens.remark; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.remark IS '备注（可选）';


--
-- Name: COLUMN api_tokens.token_hash; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.token_hash IS '明文令牌的 SHA-256 哈希（64 位 hex）；验证时对输入做同样哈希后查本列，明文不入库';


--
-- Name: COLUMN api_tokens.prefix; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.prefix IS '明文令牌前缀（脱敏辨识，如 cxat_aB3dXy）；列表展示用，无法还原令牌';


--
-- Name: COLUMN api_tokens.enabled; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.enabled IS '启用状态：0=禁用 1=启用（默认）；禁用的令牌验证失败';


--
-- Name: COLUMN api_tokens.valid_from; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.valid_from IS '生效起始时间；NULL=创建即生效';


--
-- Name: COLUMN api_tokens.expires_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.expires_at IS '过期时间；NULL=永不过期';


--
-- Name: COLUMN api_tokens.last_used_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.last_used_at IS '最近一次通过本令牌鉴权的时间；NULL=从未使用';


--
-- Name: COLUMN api_tokens.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.created_at IS '创建时间';


--
-- Name: COLUMN api_tokens.updated_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.api_tokens.updated_at IS '更新时间';


--
-- Name: assistants; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.assistants (
    id character varying(36) NOT NULL,
    name character varying(128) NOT NULL,
    description text DEFAULT ''::text NOT NULL,
    avatar character varying(64) DEFAULT '🤖'::character varying NOT NULL,
    kind smallint DEFAULT 1 NOT NULL,
    agent_type smallint DEFAULT 9 NOT NULL,
    system_prompt text DEFAULT ''::text NOT NULL,
    model_id character varying(128) DEFAULT ''::character varying NOT NULL,
    temperature double precision,
    top_p double precision,
    max_tokens integer,
    thinking_level text,
    enabled_tools text DEFAULT '[]'::text NOT NULL,
    enabled_mcps text DEFAULT '[]'::text NOT NULL,
    enabled_skills text DEFAULT '[]'::text NOT NULL,
    env_vars text DEFAULT '{}'::text NOT NULL,
    knowledge_enabled boolean DEFAULT false NOT NULL,
    greeting text DEFAULT ''::text NOT NULL,
    share_token character varying(16) DEFAULT ''::character varying NOT NULL,
    fork_count integer DEFAULT 0 NOT NULL,
    creator character varying(128) DEFAULT '019feab3-20d2-7993-8886-d05f225e4e54'::character varying NOT NULL,
    visibility smallint DEFAULT 0 NOT NULL,
    sort_order integer DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    kb_instance_id character varying(36),
    CONSTRAINT chk_assistants_kind CHECK ((kind = ANY (ARRAY[0, 1]))),
    CONSTRAINT chk_assistants_visibility CHECK ((visibility = ANY (ARRAY[0, 1, 2])))
);


--
-- Name: TABLE assistants; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.assistants IS '助手定义表（内置助手 kind=0 只读 + 自定义助手 kind=1 可编辑/Fork；广场只展示 visibility>0）';


--
-- Name: COLUMN assistants.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.id IS '助手唯一标识（UUID）';


--
-- Name: COLUMN assistants.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.name IS '助手名称';


--
-- Name: COLUMN assistants.description; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.description IS '助手描述';


--
-- Name: COLUMN assistants.avatar; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.avatar IS '头像（emoji 或图片名）';


--
-- Name: COLUMN assistants.kind; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.kind IS '类型：0=内置（只读）1=自定义（默认）';


--
-- Name: COLUMN assistants.agent_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.agent_type IS '内置 Agent 调度类型：2=设备命令 4=监控插件 9=自定义（默认；0/1 已废弃按自定义处理）';


--
-- Name: COLUMN assistants.system_prompt; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.system_prompt IS '系统提示词';


--
-- Name: COLUMN assistants.model_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.model_id IS '绑定的模型 id；空=走会话/全局默认';


--
-- Name: COLUMN assistants.temperature; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.temperature IS '采样温度，clamp 0.0~2.0；NULL=默认 0.3';


--
-- Name: COLUMN assistants.top_p; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.top_p IS 'nucleus sampling，clamp 0.0~1.0；NULL=模型/API 默认';


--
-- Name: COLUMN assistants.max_tokens; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.max_tokens IS '最大输出 token，clamp 16384~32768；NULL=默认 16384（高思考级别从中扣 thinking budget）';


--
-- Name: COLUMN assistants.thinking_level; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.thinking_level IS '思考级别：low/medium/high/xhigh/max；NULL/空=不发送（走模型默认）';


--
-- Name: COLUMN assistants.enabled_tools; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.enabled_tools IS '启用工具 key 列表（JSON 数组：search_kb/query_device_catalog/shell_command）';


--
-- Name: COLUMN assistants.enabled_mcps; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.enabled_mcps IS '启用的 MCP Server id 列表（JSON 数组）';


--
-- Name: COLUMN assistants.enabled_skills; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.enabled_skills IS '可用 Skill 白名单（JSON 数组，存 skill name）；空数组=不限制=全部可见';


--
-- Name: COLUMN assistants.knowledge_enabled; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.knowledge_enabled IS '是否启用知识库检索';


--
-- Name: COLUMN assistants.greeting; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.greeting IS '进入助手时展示的欢迎语';


--
-- Name: COLUMN assistants.share_token; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.share_token IS '分享口令（8 位，剔除易混淆字符）；空=未分享，非空靠部分唯一索引全局唯一用于 Fork';


--
-- Name: COLUMN assistants.fork_count; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.fork_count IS '被其他用户 Fork 的次数（广场排序用）';


--
-- Name: COLUMN assistants.creator; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.creator IS '归属用户 id（按用户隔离；内置助手/系统资源 seed 默认归属管理员 marvelnet，普通用户经 visibility 只读可见）';


--
-- Name: COLUMN assistants.visibility; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.visibility IS '可见性：0=私有（默认）1=广场公开 2=内置公开';


--
-- Name: COLUMN assistants.sort_order; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.sort_order IS '列表排序权重（列表按 kind, sort_order, updated_at 排序）';


--
-- Name: COLUMN assistants.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.created_at IS '创建时间';


--
-- Name: COLUMN assistants.updated_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.assistants.updated_at IS '更新时间';


--
-- Name: audit_logs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.audit_logs (
    id character varying(36) NOT NULL,
    user_id character varying(36) DEFAULT ''::character varying NOT NULL,
    actor character varying(128) DEFAULT ''::character varying NOT NULL,
    source character varying(32) DEFAULT 'web'::character varying NOT NULL,
    operation character varying(128) NOT NULL,
    target_id character varying(128) DEFAULT ''::character varying NOT NULL,
    success smallint DEFAULT 1 NOT NULL,
    detail text DEFAULT '{}'::text NOT NULL,
    ip character varying(64) DEFAULT ''::character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: kb_chunks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.kb_chunks (
    id character varying(36) NOT NULL,
    document_id character varying(36) NOT NULL,
    chunk_index integer DEFAULT 0 NOT NULL,
    content text DEFAULT ''::text NOT NULL,
    word_count integer DEFAULT 0 NOT NULL,
    header_path character varying(512) DEFAULT ''::character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: kb_doc_meta; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.kb_doc_meta (
    id integer NOT NULL,
    doc_id character varying(128) NOT NULL,
    doc_type smallint DEFAULT 1 NOT NULL,
    brand character varying(64) DEFAULT ''::character varying NOT NULL,
    dev_type character varying(64) DEFAULT ''::character varying NOT NULL,
    title character varying(256) DEFAULT ''::character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: TABLE kb_doc_meta; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.kb_doc_meta IS '知识库文档元数据表（PG 侧索引，文档正文存于 Dify 平台；doc_id 为 Dify 文档 ID 用于映射）';


--
-- Name: COLUMN kb_doc_meta.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.kb_doc_meta.id IS '自增主键';


--
-- Name: COLUMN kb_doc_meta.doc_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.kb_doc_meta.doc_id IS 'Dify 知识库文档 ID（外部唯一标识）';


--
-- Name: COLUMN kb_doc_meta.doc_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.kb_doc_meta.doc_type IS '文档类型：1=上传手册（默认）2=FAQ（会话自动学习生成）';


--
-- Name: COLUMN kb_doc_meta.brand; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.kb_doc_meta.brand IS '厂商（如华为/思科），用于检索过滤';


--
-- Name: COLUMN kb_doc_meta.dev_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.kb_doc_meta.dev_type IS '设备类型（如交换机/路由器），用于检索过滤';


--
-- Name: COLUMN kb_doc_meta.title; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.kb_doc_meta.title IS '文档标题（手册文件名 / FAQ 主题）';


--
-- Name: COLUMN kb_doc_meta.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.kb_doc_meta.created_at IS '入库时间';


--
-- Name: kb_doc_meta_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.kb_doc_meta_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: kb_doc_meta_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.kb_doc_meta_id_seq OWNED BY public.kb_doc_meta.id;


--
-- Name: kb_documents; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.kb_documents (
    id character varying(36) NOT NULL,
    kb_instance_id character varying(36) NOT NULL,
    doc_type smallint DEFAULT 1 NOT NULL,
    brand character varying(64) DEFAULT ''::character varying NOT NULL,
    dev_type character varying(64) DEFAULT ''::character varying NOT NULL,
    model character varying(64) DEFAULT ''::character varying NOT NULL,
    firmware_ver character varying(64) DEFAULT ''::character varying NOT NULL,
    title character varying(256) DEFAULT ''::character varying NOT NULL,
    source character varying(32) DEFAULT 'manual'::character varying NOT NULL,
    word_count integer DEFAULT 0 NOT NULL,
    chunk_count integer DEFAULT 0 NOT NULL,
    status smallint DEFAULT 1 NOT NULL,
    uploaded_by character varying(64) DEFAULT ''::character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: kb_instances; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.kb_instances (
    id character varying(36) NOT NULL,
    name character varying(128) NOT NULL,
    provider_kind smallint NOT NULL,
    config text DEFAULT '{}'::text NOT NULL,
    status smallint DEFAULT 1 NOT NULL,
    creator character varying(128) DEFAULT '019feab3-20d2-7993-8886-d05f225e4e54'::character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    visibility smallint DEFAULT 0 NOT NULL,
    CONSTRAINT chk_kb_instances_kind CHECK ((provider_kind = ANY (ARRAY[1, 2])))
);


--
-- Name: COLUMN kb_instances.creator; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.kb_instances.creator IS '归属用户 id（与 assistants.creator 同义；默认归属管理员 marvelnet，普通用户经 visibility 只读可见）';


--
-- Name: COLUMN kb_instances.visibility; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.kb_instances.visibility IS '可见性：0=私有（仅归属人/管理员可见）1=公开（所有用户只读可见，保留跨用户共享）';


--
-- Name: llm_models; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.llm_models (
    id character varying(36) NOT NULL,
    provider_id character varying(36) NOT NULL,
    name character varying(128) NOT NULL,
    model character varying(128) NOT NULL,
    is_default boolean DEFAULT false NOT NULL,
    status smallint DEFAULT 1 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    tags text DEFAULT '["chat"]'::text NOT NULL,
    embedding_dimensions integer,
    embedding_default boolean DEFAULT false NOT NULL,
    context_window integer,
    user_id character varying(128) DEFAULT ''::character varying NOT NULL
);


--
-- Name: TABLE llm_models; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.llm_models IS '模型表（归属于某供应商；每用户至多一个默认 chat / embedding 模型，按 user_id 隔离）';


--
-- Name: COLUMN llm_models.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_models.id IS '模型唯一标识（UUID）';


--
-- Name: COLUMN llm_models.provider_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_models.provider_id IS '所属供应商；供应商删除时级联删除其模型';


--
-- Name: COLUMN llm_models.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_models.name IS '模型显示名（人类可读，用户自定义）';


--
-- Name: COLUMN llm_models.model; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_models.model IS 'API 模型 ID（发往供应商的真实标识，如 deepseek-chat）';


--
-- Name: COLUMN llm_models.is_default; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_models.is_default IS '是否该用户的默认 chat 模型；配合部分唯一索引 (user_id) 保证每用户至多一个';


--
-- Name: COLUMN llm_models.status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_models.status IS '启用状态：0=禁用 1=启用（默认）';


--
-- Name: COLUMN llm_models.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_models.created_at IS '创建时间';


--
-- Name: COLUMN llm_models.updated_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_models.updated_at IS '更新时间';


--
-- Name: COLUMN llm_models.user_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_models.user_id IS '归属用户（冗余=provider.user_id，由应用层同步；''=系统/legacy，仅管理员可见）';


--
-- Name: llm_providers; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.llm_providers (
    id character varying(36) NOT NULL,
    vendor_name character varying(128) NOT NULL,
    name character varying(128) NOT NULL,
    base_url character varying(512) NOT NULL,
    protocol character varying(16) DEFAULT 'openai_compat'::character varying NOT NULL,
    encrypted_key text DEFAULT ''::text NOT NULL,
    key_suffix character varying(8) DEFAULT ''::character varying NOT NULL,
    status smallint DEFAULT 1 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    user_id character varying(128) DEFAULT ''::character varying NOT NULL
);


--
-- Name: TABLE llm_providers; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.llm_providers IS '模型供应商表（API 接入配置；protocol 决定走哪条客户端链路，API Key 加密存储）';


--
-- Name: COLUMN llm_providers.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.id IS '供应商实例唯一标识（UUID）';


--
-- Name: COLUMN llm_providers.vendor_name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.vendor_name IS '供应商品牌名（如 DeepSeek/OpenAI）';


--
-- Name: COLUMN llm_providers.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.name IS '实例显示名（用户自定义）';


--
-- Name: COLUMN llm_providers.base_url; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.base_url IS 'API 基础地址';


--
-- Name: COLUMN llm_providers.protocol; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.protocol IS '协议：openai_compat（默认，OpenAI 兼容）/ anthropic（Anthropic Messages）';


--
-- Name: COLUMN llm_providers.encrypted_key; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.encrypted_key IS 'API Key 密文（AES-256-GCM，base64(nonce+ciphertext+tag)）；空=未配置';


--
-- Name: COLUMN llm_providers.key_suffix; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.key_suffix IS 'API Key 末 4 位明文（前端识别用）；key<=4 位时为 ****';


--
-- Name: COLUMN llm_providers.status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.status IS '启用状态：0=禁用 1=启用（默认）';


--
-- Name: COLUMN llm_providers.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.created_at IS '创建时间';


--
-- Name: COLUMN llm_providers.updated_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.updated_at IS '更新时间';


--
-- Name: COLUMN llm_providers.user_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.llm_providers.user_id IS '归属用户 id（按用户隔离；''=系统/legacy，仅管理员可见）';


--
-- Name: mcp_servers; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.mcp_servers (
    id character varying(36) NOT NULL,
    name character varying(128) NOT NULL,
    slug character varying(64) NOT NULL,
    transport smallint NOT NULL,
    endpoint character varying(1024) NOT NULL,
    args text DEFAULT '[]'::text NOT NULL,
    env_enc text DEFAULT ''::text NOT NULL,
    env_mask text DEFAULT '{}'::text NOT NULL,
    headers_enc text DEFAULT ''::text NOT NULL,
    headers_mask text DEFAULT '{}'::text NOT NULL,
    status smallint DEFAULT 1 NOT NULL,
    tool_timeout_secs integer DEFAULT 60 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    user_id character varying(128) DEFAULT ''::character varying NOT NULL,
    CONSTRAINT chk_mcp_status CHECK ((status = ANY (ARRAY[0, 1]))),
    CONSTRAINT chk_mcp_transport CHECK ((transport = ANY (ARRAY[1, 2])))
);


--
-- Name: TABLE mcp_servers; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.mcp_servers IS 'MCP Server 配置表（接入外部工具集；敏感字段 env/headers 采用 AES-256-GCM 加密存储 + 掩码展示）';


--
-- Name: COLUMN mcp_servers.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.id IS 'MCP Server 唯一标识（UUID）';


--
-- Name: COLUMN mcp_servers.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.name IS '显示名';


--
-- Name: COLUMN mcp_servers.slug; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.slug IS 'URL 友好标识（全局唯一）';


--
-- Name: COLUMN mcp_servers.transport; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.transport IS '传输方式：1=stdio（子进程）2=streamable_http（远程）';


--
-- Name: COLUMN mcp_servers.endpoint; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.endpoint IS 'stdio: 可执行命令名（禁 shell 元字符）；http: 完整 URL';


--
-- Name: COLUMN mcp_servers.args; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.args IS 'stdio 启动参数列表（JSON 数组）；http 留空';


--
-- Name: COLUMN mcp_servers.env_enc; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.env_enc IS '环境变量密文（AES-256-GCM 加密的 JSON map）；空 map 时为空串';


--
-- Name: COLUMN mcp_servers.env_mask; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.env_mask IS '环境变量掩码（脱敏 JSON，回前端用，明文不外泄）';


--
-- Name: COLUMN mcp_servers.headers_enc; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.headers_enc IS 'HTTP 自定义请求头密文（同 env 加密）';


--
-- Name: COLUMN mcp_servers.headers_mask; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.headers_mask IS 'HTTP 请求头掩码（同 env 掩码）';


--
-- Name: COLUMN mcp_servers.status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.status IS '启用状态：0=禁用 1=启用（默认）';


--
-- Name: COLUMN mcp_servers.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.created_at IS '创建时间';


--
-- Name: COLUMN mcp_servers.updated_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.updated_at IS '更新时间';


--
-- Name: COLUMN mcp_servers.user_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.mcp_servers.user_id IS '归属用户 id（按用户隔离；''=系统/legacy，仅管理员可见）';


--
-- Name: memories; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.memories (
    id character varying(36) NOT NULL,
    user_id character varying(36) NOT NULL,
    scope smallint DEFAULT 0 NOT NULL,
    assistant_id character varying(36),
    type smallint DEFAULT 0 NOT NULL,
    content text NOT NULL,
    source_session_id character varying(64),
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT chk_memories_scope CHECK ((scope = ANY (ARRAY[0, 1]))),
    CONSTRAINT chk_memories_type CHECK ((type = ANY (ARRAY[0, 1])))
);


--
-- Name: TABLE memories; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.memories IS '已确认的跨会话记忆（用户级 scope=0 跨助手共享 / 助手级 scope=1 仅该助手；会话不参与隔离，仅 source_session_id 溯源）';


--
-- Name: COLUMN memories.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memories.id IS '记忆唯一标识（UUID v7）';


--
-- Name: COLUMN memories.user_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memories.user_id IS '所属用户（记忆按用户隔离）';


--
-- Name: COLUMN memories.scope; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memories.scope IS '作用域：0=用户级（默认，跨所有助手共享）1=助手级（仅 assistant_id 命中时注入）';


--
-- Name: COLUMN memories.assistant_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memories.assistant_id IS 'scope=1 时绑定的助手 id；scope=0 时为 NULL';


--
-- Name: COLUMN memories.type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memories.type IS '记忆类型：0=习惯/偏好 1=坑/避坑';


--
-- Name: COLUMN memories.content; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memories.content IS '记忆正文（自由文本，注入 system prompt 给 agent）';


--
-- Name: COLUMN memories.source_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memories.source_session_id IS '产生该记忆的来源会话（仅溯源/管理用，不作为隔离维度）';


--
-- Name: COLUMN memories.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memories.created_at IS '创建时间';


--
-- Name: COLUMN memories.updated_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memories.updated_at IS '更新时间';


--
-- Name: memory_proposals; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.memory_proposals (
    id character varying(36) NOT NULL,
    user_id character varying(36) NOT NULL,
    session_id character varying(64) NOT NULL,
    assistant_id character varying(36),
    scope smallint DEFAULT 0 NOT NULL,
    type smallint DEFAULT 0 NOT NULL,
    content text NOT NULL,
    reason text DEFAULT ''::text NOT NULL,
    status smallint DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT chk_proposals_scope CHECK ((scope = ANY (ARRAY[0, 1]))),
    CONSTRAINT chk_proposals_status CHECK ((status = ANY (ARRAY[0, 1, 2]))),
    CONSTRAINT chk_proposals_type CHECK ((type = ANY (ARRAY[0, 1])))
);


--
-- Name: TABLE memory_proposals; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.memory_proposals IS '记忆建议（agent 通过 propose_memory 工具产出，待用户在卡片上确认；accept 后转正入 memories）';


--
-- Name: COLUMN memory_proposals.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memory_proposals.id IS '建议唯一标识（UUID v7）';


--
-- Name: COLUMN memory_proposals.user_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memory_proposals.user_id IS '所属用户';


--
-- Name: COLUMN memory_proposals.session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memory_proposals.session_id IS '产生建议的会话';


--
-- Name: COLUMN memory_proposals.assistant_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memory_proposals.assistant_id IS '提议时的助手（scope=1 时填）';


--
-- Name: COLUMN memory_proposals.scope; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memory_proposals.scope IS '建议的作用域：0=用户级 1=助手级';


--
-- Name: COLUMN memory_proposals.type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memory_proposals.type IS '建议类型：0=习惯 1=坑';


--
-- Name: COLUMN memory_proposals.content; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memory_proposals.content IS '建议记忆正文';


--
-- Name: COLUMN memory_proposals.reason; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memory_proposals.reason IS 'agent 给出的「为什么值得记」理由（展示在卡片上帮助用户判断）';


--
-- Name: COLUMN memory_proposals.status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memory_proposals.status IS '状态：0=待确认 1=已加入（转正写入 memories）2=已忽略';


--
-- Name: COLUMN memory_proposals.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.memory_proposals.created_at IS '创建时间';


--
-- Name: monitor_plugin_versions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.monitor_plugin_versions (
    id integer NOT NULL,
    plugin_id character varying(128) NOT NULL,
    version integer NOT NULL,
    source_code text NOT NULL,
    change_description text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: TABLE monitor_plugin_versions; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.monitor_plugin_versions IS '插件版本历史表（每个版本保存一份 Rhai 源码；主表 active_version 指向本表 version）';


--
-- Name: COLUMN monitor_plugin_versions.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugin_versions.id IS '自增主键';


--
-- Name: COLUMN monitor_plugin_versions.plugin_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugin_versions.plugin_id IS '所属插件';


--
-- Name: COLUMN monitor_plugin_versions.version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugin_versions.version IS '版本号（单调递增整数）';


--
-- Name: COLUMN monitor_plugin_versions.source_code; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugin_versions.source_code IS 'Rhai 脚本源码（运行时由 Rhai 引擎解释执行）';


--
-- Name: COLUMN monitor_plugin_versions.change_description; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugin_versions.change_description IS '本次发版变更说明（区别于主表的整体 description）';


--
-- Name: COLUMN monitor_plugin_versions.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugin_versions.created_at IS '发版时间';


--
-- Name: monitor_plugin_versions_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.monitor_plugin_versions_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: monitor_plugin_versions_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.monitor_plugin_versions_id_seq OWNED BY public.monitor_plugin_versions.id;


--
-- Name: monitor_plugins; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.monitor_plugins (
    id integer NOT NULL,
    plugin_id character varying(128) NOT NULL,
    description text DEFAULT ''::text NOT NULL,
    active_version integer,
    enabled boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: TABLE monitor_plugins; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.monitor_plugins IS '监控插件主表（插件元信息 + 当前激活版本；仅 enabled=TRUE 且 active_version 非空的插件会被加载执行）';


--
-- Name: COLUMN monitor_plugins.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugins.id IS '自增主键';


--
-- Name: COLUMN monitor_plugins.plugin_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugins.plugin_id IS '插件业务标识（全局唯一）';


--
-- Name: COLUMN monitor_plugins.description; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugins.description IS '插件整体说明';


--
-- Name: COLUMN monitor_plugins.active_version; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugins.active_version IS '当前激活版本号（指向 monitor_plugin_versions.version）；NULL=尚无激活版本';


--
-- Name: COLUMN monitor_plugins.enabled; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugins.enabled IS '是否启用';


--
-- Name: COLUMN monitor_plugins.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugins.created_at IS '创建时间';


--
-- Name: COLUMN monitor_plugins.updated_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.monitor_plugins.updated_at IS '更新时间';


--
-- Name: monitor_plugins_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.monitor_plugins_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: monitor_plugins_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.monitor_plugins_id_seq OWNED BY public.monitor_plugins.id;


--
-- Name: scheduled_tasks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.scheduled_tasks (
    id character varying(64) NOT NULL,
    user_id character varying(128) NOT NULL,
    assistant_id character varying(64) NOT NULL,
    name text NOT NULL,
    instruction text NOT NULL,
    schedule_cron character varying(128) NOT NULL,
    timezone character varying(64) DEFAULT 'Asia/Shanghai'::character varying NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    scheduler_job_id character varying(64),
    next_run_at timestamp with time zone,
    last_run_at timestamp with time zone,
    last_run_status smallint,
    last_session_id character varying(64),
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT chk_scheduled_tasks_status CHECK ((last_run_status = ANY (ARRAY[0, 1, 2])))
);


--
-- Name: TABLE scheduled_tasks; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.scheduled_tasks IS '定时任务定义表（基于某助手按 cron 周期触发 agent run，结果落 source_type=1 会话；调度引擎 tokio-cron-scheduler 的 job 表仅存调度元数据，本表是业务实体唯一数据源）';


--
-- Name: COLUMN scheduled_tasks.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.id IS '任务唯一标识（UUID v7）';


--
-- Name: COLUMN scheduled_tasks.user_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.user_id IS '创建者（归属/鉴权/记忆隔离；执行以此身份跑）';


--
-- Name: COLUMN scheduled_tasks.assistant_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.assistant_id IS '基于哪个助手执行（FK 逻辑关联 assistants.id，不建硬 FK 以便助手删除后任务仍可被标记停用）';


--
-- Name: COLUMN scheduled_tasks.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.name IS '任务名（生成会话标题前缀）';


--
-- Name: COLUMN scheduled_tasks.instruction; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.instruction IS '触发时发给 agent 的指令（如"出昨天的销售报表"）';


--
-- Name: COLUMN scheduled_tasks.schedule_cron; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.schedule_cron IS '标准 cron 表达式（NL 转换后落库的成品；调度不依赖 LLM）';


--
-- Name: COLUMN scheduled_tasks.timezone; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.timezone IS '时区（默认 Asia/Shanghai，避免服务器时区坑）';


--
-- Name: COLUMN scheduled_tasks.enabled; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.enabled IS '启停开关（停用=从调度器 remove，不删业务行）';


--
-- Name: COLUMN scheduled_tasks.scheduler_job_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.scheduler_job_id IS 'tokio-cron-scheduler 返回的 job UUID（增删改同步用）';


--
-- Name: COLUMN scheduled_tasks.next_run_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.next_run_at IS '下次触发时间（详情页展示，由库计算后回填）';


--
-- Name: COLUMN scheduled_tasks.last_run_status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.last_run_status IS '最近运行结果：0成功/1失败/2超时';


--
-- Name: COLUMN scheduled_tasks.last_session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.scheduled_tasks.last_session_id IS '最近运行产生的会话 id（详情页直达）';


--
-- Name: session_settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.session_settings (
    session_id character varying(64) NOT NULL,
    user_id character varying(128) DEFAULT ''::character varying NOT NULL,
    title text DEFAULT ''::text NOT NULL,
    agent_type character varying(32) DEFAULT 'custom'::character varying NOT NULL,
    model_id character varying(64),
    thinking_level text DEFAULT 'high'::text NOT NULL,
    sandbox_mode text DEFAULT 'workspace-write'::text NOT NULL,
    approval_policy text DEFAULT 'unless-trusted'::text NOT NULL,
    assistant_id character varying(64),
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    token_total bigint DEFAULT 0 NOT NULL,
    token_threshold bigint DEFAULT 0 NOT NULL,
    source_type smallint DEFAULT 0 NOT NULL,
    schedule_task_id character varying(64),
    trigger_kind character varying(16) DEFAULT 'cron'::character varying NOT NULL,
    CONSTRAINT chk_settings_approval CHECK ((approval_policy = ANY (ARRAY['never'::text, 'on-request'::text, 'on-request-rule-request-permission'::text, 'unless-trusted'::text, 'auto'::text]))),
    CONSTRAINT chk_settings_sandbox CHECK ((sandbox_mode = ANY (ARRAY['read-only'::text, 'workspace-write'::text, 'danger-full-access'::text]))),
    CONSTRAINT chk_settings_thinking CHECK ((thinking_level = ANY (ARRAY['low'::text, 'medium'::text, 'high'::text, 'xhigh'::text, 'max'::text]))),
    CONSTRAINT chk_settings_trigger CHECK ((trigger_kind = ANY (ARRAY['cron'::text, 'catchup'::text, 'manual'::text])))
);


--
-- Name: TABLE session_settings; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.session_settings IS '会话级配置合并大表（标题+模型+思考级别+沙箱审批+助手绑定；title/agent_type 物化供列表 SQL 排序筛选分页）';


--
-- Name: COLUMN session_settings.session_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.session_id IS '会话 ID（UUID v7，前 48 位即创建毫秒；列表按其字符串倒序=创建时间倒序）';


--
-- Name: COLUMN session_settings.user_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.user_id IS '会话归属用户（列表按用户隔离；与 adk sessions.user_id 对齐）';


--
-- Name: COLUMN session_settings.title; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.title IS '会话标题（重命名/首轮默认标题落列；空串=未设置，前端回退显示）';


--
-- Name: COLUMN session_settings.agent_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.agent_type IS '会话所属 agent 类型（物化自会话 state，供列表筛选）';


--
-- Name: COLUMN session_settings.model_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.model_id IS '绑定模型 id；NULL=未绑定具体模型（default/auto/空 → 运行时解析全局默认）';


--
-- Name: COLUMN session_settings.thinking_level; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.thinking_level IS '思考级别：low/medium/high/xhigh/max（默认 high）';


--
-- Name: COLUMN session_settings.sandbox_mode; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.sandbox_mode IS '沙箱模式：read-only 只读 / workspace-write 工作区写（默认）/ danger-full-access 完全访问';


--
-- Name: COLUMN session_settings.approval_policy; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.approval_policy IS '审批策略：never 从不 / on-request 请求时 / on-request-rule-request-permission / unless-trusted 除可信外都审批（默认）/ auto 自动批准（仅定时任务无人值守用）';


--
-- Name: COLUMN session_settings.assistant_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.assistant_id IS '绑定助手 id（SSE 未带 assistant_id 时从本表兜底读）；NULL=未绑定';


--
-- Name: COLUMN session_settings.updated_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.updated_at IS '更新时间（任意会话级配置变更时刷新）';


--
-- Name: COLUMN session_settings.token_total; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.token_total IS '会话级上下文 token 用量累计峰值（对齐 codex 会话级 token_info：跨轮持久、压缩随历史重写）。每轮 run 结束落库，进程重启后重进会话立即可见';


--
-- Name: COLUMN session_settings.token_threshold; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.token_threshold IS 'token 用量压缩阈值（soft_gate，随模型上下文窗口计算）。与 token_total 配对展示「已用 / 阈值」';


--
-- Name: COLUMN session_settings.source_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.source_type IS '会话来源：0=手动 1=定时任务（普通会话列表过滤 source_type=0）';


--
-- Name: COLUMN session_settings.schedule_task_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.session_settings.schedule_task_id IS '定时任务会话归属的任务 id（scheduled_tasks.id；手动会话为 NULL）';


--
-- Name: shell_rules; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.shell_rules (
    id character varying(36) NOT NULL,
    pattern character varying(512) NOT NULL,
    decision smallint NOT NULL,
    priority integer DEFAULT 0 NOT NULL,
    enabled smallint DEFAULT 1 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT shell_rules_decision_check CHECK ((decision = ANY (ARRAY[0, 1, 2]))),
    CONSTRAINT shell_rules_enabled_check CHECK ((enabled = ANY (ARRAY[0, 1])))
);


--
-- Name: TABLE shell_rules; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.shell_rules IS 'Shell 命令审批规则表（命令匹配到自动决策；仅 enabled=1 的规则进内存缓存，按 priority 降序遍历返回首个命中）';


--
-- Name: COLUMN shell_rules.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.shell_rules.id IS '规则唯一标识（UUID）';


--
-- Name: COLUMN shell_rules.pattern; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.shell_rules.pattern IS 'glob 匹配模式（支持 * ?，大小写敏感），对命令字符串匹配';


--
-- Name: COLUMN shell_rules.decision; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.shell_rules.decision IS '决策：0=放行(Allow) 1=阻断(Deny) 2=需审批(Ask)';


--
-- Name: COLUMN shell_rules.priority; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.shell_rules.priority IS '优先级（高优先先匹配；同优先按 created_at）';


--
-- Name: COLUMN shell_rules.enabled; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.shell_rules.enabled IS '1=启用（默认）0=禁用';


--
-- Name: COLUMN shell_rules.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.shell_rules.created_at IS '创建时间';


--
-- Name: user_identities; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.user_identities (
    id character varying(36) NOT NULL,
    provider character varying(64) NOT NULL,
    external_id character varying(128) NOT NULL,
    user_id character varying(36) NOT NULL,
    name character varying(128) DEFAULT ''::character varying NOT NULL,
    avatar character varying(512) DEFAULT ''::character varying NOT NULL,
    email character varying(256) DEFAULT ''::character varying NOT NULL,
    raw_payload text DEFAULT '{}'::text NOT NULL,
    linked_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: TABLE user_identities; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.user_identities IS '第三方身份绑定表（一个用户可绑定多个第三方身份；provider 取值 feishu/wechat/oidc）';


--
-- Name: COLUMN user_identities.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.user_identities.id IS '绑定记录唯一标识';


--
-- Name: COLUMN user_identities.provider; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.user_identities.provider IS '身份提供商：feishu（飞书）/ wechat（微信）/ oidc（通用 OIDC）';


--
-- Name: COLUMN user_identities.external_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.user_identities.external_id IS '第三方平台返回的用户唯一 ID';


--
-- Name: COLUMN user_identities.user_id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.user_identities.user_id IS '关联用户；用户删除时级联删除其所有绑定';


--
-- Name: COLUMN user_identities.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.user_identities.name IS '同步自第三方的昵称快照';


--
-- Name: COLUMN user_identities.avatar; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.user_identities.avatar IS '同步自第三方的头像快照';


--
-- Name: COLUMN user_identities.email; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.user_identities.email IS '同步自第三方的邮箱快照';


--
-- Name: COLUMN user_identities.raw_payload; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.user_identities.raw_payload IS '第三方返回的原始 payload（JSON 字符串，便于排查）';


--
-- Name: COLUMN user_identities.linked_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.user_identities.linked_at IS '首次绑定时间';


--
-- Name: users; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.users (
    id character varying(36) NOT NULL,
    name character varying(128) DEFAULT ''::character varying NOT NULL,
    avatar character varying(512) DEFAULT ''::character varying NOT NULL,
    email character varying(256) DEFAULT ''::character varying NOT NULL,
    status smallint DEFAULT 1 NOT NULL,
    is_admin smallint DEFAULT 0 NOT NULL,
    username character varying(64),
    password_hash text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: TABLE users; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON TABLE public.users IS '系统用户主表（本地账号 + SSO 登录后的用户记录；首个本地用户可 bootstrap 为管理员）';


--
-- Name: COLUMN users.id; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.users.id IS '用户唯一标识（UUID v4）';


--
-- Name: COLUMN users.name; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.users.name IS '显示名（昵称）';


--
-- Name: COLUMN users.avatar; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.users.avatar IS '头像 URL';


--
-- Name: COLUMN users.email; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.users.email IS '邮箱（SSO 同步或本地填写）';


--
-- Name: COLUMN users.status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.users.status IS '启用状态：0=禁用 1=启用（默认）；禁用用户登录被拒';


--
-- Name: COLUMN users.is_admin; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.users.is_admin IS '是否管理员：0=普通用户（默认）1=管理员';


--
-- Name: COLUMN users.username; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.users.username IS '登录用户名；SSO-only 用户为 NULL（唯一索引允许多 NULL）';


--
-- Name: COLUMN users.password_hash; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.users.password_hash IS '本地账号密码哈希；SSO 用户为 NULL';


--
-- Name: COLUMN users.created_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.users.created_at IS '创建时间';


--
-- Name: COLUMN users.updated_at; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON COLUMN public.users.updated_at IS '更新时间';


--
-- Name: kb_doc_meta id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kb_doc_meta ALTER COLUMN id SET DEFAULT nextval('public.kb_doc_meta_id_seq'::regclass);


--
-- Name: monitor_plugin_versions id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.monitor_plugin_versions ALTER COLUMN id SET DEFAULT nextval('public.monitor_plugin_versions_id_seq'::regclass);


--
-- Name: monitor_plugins id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.monitor_plugins ALTER COLUMN id SET DEFAULT nextval('public.monitor_plugins_id_seq'::regclass);


--
-- Data for Name: api_tokens; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.api_tokens (id, user_id, name, remark, token_hash, prefix, enabled, valid_from, expires_at, last_used_at, created_at, updated_at) FROM stdin;
\.


--
-- Data for Name: assistants; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.assistants (id, name, description, avatar, kind, agent_type, system_prompt, model_id, temperature, top_p, max_tokens, thinking_level, enabled_tools, enabled_mcps, env_vars, knowledge_enabled, greeting, share_token, fork_count, creator, visibility, sort_order, created_at, updated_at, kb_instance_id) FROM stdin;
\.


--
-- Data for Name: audit_logs; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.audit_logs (id, user_id, actor, source, operation, target_id, success, detail, ip, created_at) FROM stdin;
01a0274e-5c30-76a2-9d39-c8fa8707c9e0	019feab3-20d2-7993-8886-d05f225e4e54	marvelnet	web	login		1	{}		2026-08-22 10:30:41.968179+08
\.


--
-- Data for Name: kb_chunks; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.kb_chunks (id, document_id, chunk_index, content, word_count, header_path, created_at) FROM stdin;
\.


--
-- Data for Name: kb_doc_meta; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.kb_doc_meta (id, doc_id, doc_type, brand, dev_type, title, created_at) FROM stdin;
\.


--
-- Data for Name: kb_documents; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.kb_documents (id, kb_instance_id, doc_type, brand, dev_type, model, firmware_ver, title, source, word_count, chunk_count, status, uploaded_by, created_at, updated_at) FROM stdin;
\.


--
-- Data for Name: kb_instances; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.kb_instances (id, name, provider_kind, config, status, creator, created_at, updated_at, visibility) FROM stdin;
\.


--
-- Data for Name: llm_models; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.llm_models (id, provider_id, name, model, is_default, status, created_at, updated_at, tags, embedding_dimensions, embedding_default, context_window, user_id) FROM stdin;
\.


--
-- Data for Name: llm_providers; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.llm_providers (id, vendor_name, name, base_url, protocol, encrypted_key, key_suffix, status, created_at, updated_at, user_id) FROM stdin;
\.


--
-- Data for Name: mcp_servers; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.mcp_servers (id, name, slug, transport, endpoint, args, env_enc, env_mask, headers_enc, headers_mask, status, tool_timeout_secs, created_at, updated_at, user_id) FROM stdin;
\.


--
-- Data for Name: memories; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.memories (id, user_id, scope, assistant_id, type, content, source_session_id, created_at, updated_at) FROM stdin;
\.


--
-- Data for Name: memory_proposals; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.memory_proposals (id, user_id, session_id, assistant_id, scope, type, content, reason, status, created_at) FROM stdin;
\.


--
-- Data for Name: monitor_plugin_versions; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.monitor_plugin_versions (id, plugin_id, version, source_code, change_description, created_at) FROM stdin;
\.


--
-- Data for Name: monitor_plugins; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.monitor_plugins (id, plugin_id, description, active_version, enabled, created_at, updated_at) FROM stdin;
\.


--
-- Data for Name: scheduled_tasks; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.scheduled_tasks (id, user_id, assistant_id, name, instruction, schedule_cron, timezone, enabled, scheduler_job_id, next_run_at, last_run_at, last_run_status, last_session_id, created_at, updated_at) FROM stdin;
\.


--
-- Data for Name: session_settings; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.session_settings (session_id, user_id, title, agent_type, model_id, thinking_level, sandbox_mode, approval_policy, assistant_id, updated_at, token_total, token_threshold, source_type, schedule_task_id) FROM stdin;
\.


--
-- Data for Name: shell_rules; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.shell_rules (id, pattern, decision, priority, enabled, created_at) FROM stdin;
\.


--
-- Data for Name: user_identities; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.user_identities (id, provider, external_id, user_id, name, avatar, email, raw_payload, linked_at) FROM stdin;
\.


--
-- Data for Name: users; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.users (id, name, avatar, email, status, is_admin, username, password_hash, created_at, updated_at) FROM stdin;
019feab3-20d2-7993-8886-d05f225e4e54	marvelnet			1	1	marvelnet	$argon2id$v=19$m=19456,t=2,p=1$iKLbYrlR2ZIZmyOlmvqFIQ$UvnP9g/t7aCTJh0RKbUIC48lFWYQvpLa11rsZwRtej4	2026-08-22 10:29:55.424665+08	2026-08-22 10:29:55.424665+08
\.


--
-- Name: kb_doc_meta_id_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.kb_doc_meta_id_seq', 1, false);


--
-- Name: monitor_plugin_versions_id_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.monitor_plugin_versions_id_seq', 1, false);


--
-- Name: monitor_plugins_id_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.monitor_plugins_id_seq', 1, false);


--
-- Name: api_tokens api_tokens_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.api_tokens
    ADD CONSTRAINT api_tokens_pkey PRIMARY KEY (id);


--
-- Name: assistants assistants_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.assistants
    ADD CONSTRAINT assistants_pkey PRIMARY KEY (id);


--
-- Name: audit_logs audit_logs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.audit_logs
    ADD CONSTRAINT audit_logs_pkey PRIMARY KEY (id);


--
-- Name: kb_instances chk_kb_instances_visibility; Type: CHECK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE public.kb_instances
    ADD CONSTRAINT chk_kb_instances_visibility CHECK ((visibility = ANY (ARRAY[0, 1]))) NOT VALID;


--
-- Name: kb_chunks kb_chunks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kb_chunks
    ADD CONSTRAINT kb_chunks_pkey PRIMARY KEY (id);


--
-- Name: kb_doc_meta kb_doc_meta_doc_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kb_doc_meta
    ADD CONSTRAINT kb_doc_meta_doc_id_key UNIQUE (doc_id);


--
-- Name: kb_doc_meta kb_doc_meta_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kb_doc_meta
    ADD CONSTRAINT kb_doc_meta_pkey PRIMARY KEY (id);


--
-- Name: kb_documents kb_documents_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kb_documents
    ADD CONSTRAINT kb_documents_pkey PRIMARY KEY (id);


--
-- Name: kb_instances kb_instances_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kb_instances
    ADD CONSTRAINT kb_instances_pkey PRIMARY KEY (id);


--
-- Name: llm_models llm_models_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.llm_models
    ADD CONSTRAINT llm_models_pkey PRIMARY KEY (id);


--
-- Name: llm_providers llm_providers_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.llm_providers
    ADD CONSTRAINT llm_providers_pkey PRIMARY KEY (id);


--
-- Name: mcp_servers mcp_servers_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.mcp_servers
    ADD CONSTRAINT mcp_servers_pkey PRIMARY KEY (id);


--
-- Name: memories memories_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.memories
    ADD CONSTRAINT memories_pkey PRIMARY KEY (id);


--
-- Name: memory_proposals memory_proposals_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.memory_proposals
    ADD CONSTRAINT memory_proposals_pkey PRIMARY KEY (id);


--
-- Name: monitor_plugin_versions monitor_plugin_versions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.monitor_plugin_versions
    ADD CONSTRAINT monitor_plugin_versions_pkey PRIMARY KEY (id);


--
-- Name: monitor_plugins monitor_plugins_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.monitor_plugins
    ADD CONSTRAINT monitor_plugins_pkey PRIMARY KEY (id);


--
-- Name: monitor_plugins monitor_plugins_plugin_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.monitor_plugins
    ADD CONSTRAINT monitor_plugins_plugin_id_key UNIQUE (plugin_id);


--
-- Name: scheduled_tasks scheduled_tasks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.scheduled_tasks
    ADD CONSTRAINT scheduled_tasks_pkey PRIMARY KEY (id);


--
-- Name: session_settings session_settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.session_settings
    ADD CONSTRAINT session_settings_pkey PRIMARY KEY (session_id);


--
-- Name: shell_rules shell_rules_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.shell_rules
    ADD CONSTRAINT shell_rules_pkey PRIMARY KEY (id);


--
-- Name: mcp_servers uq_mcp_servers_slug; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.mcp_servers
    ADD CONSTRAINT uq_mcp_servers_slug UNIQUE (slug);


--
-- Name: monitor_plugin_versions uq_plugin_version; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.monitor_plugin_versions
    ADD CONSTRAINT uq_plugin_version UNIQUE (plugin_id, version);


--
-- Name: user_identities uq_provider_external; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_identities
    ADD CONSTRAINT uq_provider_external UNIQUE (provider, external_id);


--
-- Name: llm_models uq_provider_model; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.llm_models
    ADD CONSTRAINT uq_provider_model UNIQUE (provider_id, model);


--
-- Name: user_identities user_identities_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_identities
    ADD CONSTRAINT user_identities_pkey PRIMARY KEY (id);


--
-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);


--
-- Name: idx_api_tokens_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_api_tokens_user ON public.api_tokens USING btree (user_id);


--
-- Name: INDEX idx_api_tokens_user; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_api_tokens_user IS '按用户列其全部令牌';


--
-- Name: idx_assistants_creator; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_assistants_creator ON public.assistants USING btree (creator, kind, updated_at DESC);


--
-- Name: INDEX idx_assistants_creator; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_assistants_creator IS '按归属用户列出其助手（列表隔离；广场另走 idx_assistants_explore）';


--
-- Name: idx_assistants_explore; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_assistants_explore ON public.assistants USING btree (visibility, fork_count DESC, updated_at DESC);


--
-- Name: INDEX idx_assistants_explore; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_assistants_explore IS '广场探索列表索引（仅公开 + 按 fork_count 热度排）';


--
-- Name: idx_assistants_list; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_assistants_list ON public.assistants USING btree (kind, sort_order, updated_at DESC);


--
-- Name: INDEX idx_assistants_list; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_assistants_list IS '助手列表查询索引（分类 kind + 排序 sort_order, updated_at）';


--
-- Name: idx_audit_logs_created; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_audit_logs_created ON public.audit_logs USING btree (created_at);


--
-- Name: idx_audit_logs_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_audit_logs_user ON public.audit_logs USING btree (user_id);


--
-- Name: idx_kb_chunks_document; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kb_chunks_document ON public.kb_chunks USING btree (document_id);


--
-- Name: idx_kb_doc_meta_brand; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kb_doc_meta_brand ON public.kb_doc_meta USING btree (brand);


--
-- Name: INDEX idx_kb_doc_meta_brand; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_kb_doc_meta_brand IS '按厂商过滤';


--
-- Name: idx_kb_doc_meta_dev_type; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kb_doc_meta_dev_type ON public.kb_doc_meta USING btree (dev_type);


--
-- Name: INDEX idx_kb_doc_meta_dev_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_kb_doc_meta_dev_type IS '按设备类型过滤';


--
-- Name: idx_kb_doc_meta_type; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kb_doc_meta_type ON public.kb_doc_meta USING btree (doc_type);


--
-- Name: INDEX idx_kb_doc_meta_type; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_kb_doc_meta_type IS '按文档类型检索';


--
-- Name: idx_kb_documents_brand_dev; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kb_documents_brand_dev ON public.kb_documents USING btree (kb_instance_id, brand, dev_type);


--
-- Name: idx_kb_documents_instance; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kb_documents_instance ON public.kb_documents USING btree (kb_instance_id);


--
-- Name: idx_kb_instances_creator; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kb_instances_creator ON public.kb_instances USING btree (creator, created_at);


--
-- Name: INDEX idx_kb_instances_creator; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_kb_instances_creator IS '按归属用户列出其知识库实例（列表隔离）';


--
-- Name: idx_kb_instances_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_kb_instances_status ON public.kb_instances USING btree (status);


--
-- Name: idx_llm_models_provider; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_llm_models_provider ON public.llm_models USING btree (provider_id);


--
-- Name: INDEX idx_llm_models_provider; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_llm_models_provider IS '按供应商列出其模型';


--
-- Name: idx_llm_models_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_llm_models_user ON public.llm_models USING btree (user_id, created_at);


--
-- Name: INDEX idx_llm_models_user; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_llm_models_user IS '按归属用户列出其模型（列表隔离）';


--
-- Name: idx_llm_providers_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_llm_providers_status ON public.llm_providers USING btree (status);


--
-- Name: INDEX idx_llm_providers_status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_llm_providers_status IS '按状态筛选启用的供应商';


--
-- Name: idx_llm_providers_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_llm_providers_user ON public.llm_providers USING btree (user_id, created_at);


--
-- Name: INDEX idx_llm_providers_user; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_llm_providers_user IS '按归属用户列出其供应商（列表隔离）';


--
-- Name: idx_mcp_servers_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_mcp_servers_status ON public.mcp_servers USING btree (status);


--
-- Name: INDEX idx_mcp_servers_status; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_mcp_servers_status IS '按状态筛选启用的 MCP Server';


--
-- Name: idx_mcp_servers_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_mcp_servers_user ON public.mcp_servers USING btree (user_id, created_at);


--
-- Name: INDEX idx_mcp_servers_user; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_mcp_servers_user IS '按归属用户列出其 MCP Server（列表隔离）';


--
-- Name: idx_memories_assistant; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_memories_assistant ON public.memories USING btree (user_id, assistant_id) WHERE (scope = 1);


--
-- Name: INDEX idx_memories_assistant; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_memories_assistant IS '按用户+助手拉取助手级记忆（scope=1 部分索引）';


--
-- Name: idx_memories_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_memories_user ON public.memories USING btree (user_id);


--
-- Name: INDEX idx_memories_user; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_memories_user IS '按用户拉取其全部用户级记忆（注入 stable prefix）';


--
-- Name: idx_monitor_plugins_enabled; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_monitor_plugins_enabled ON public.monitor_plugins USING btree (enabled);


--
-- Name: INDEX idx_monitor_plugins_enabled; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_monitor_plugins_enabled IS '列出启用的插件';


--
-- Name: idx_plugin_versions_plugin; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_plugin_versions_plugin ON public.monitor_plugin_versions USING btree (plugin_id);


--
-- Name: INDEX idx_plugin_versions_plugin; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_plugin_versions_plugin IS '按插件列出版本历史';


--
-- Name: idx_proposals_session; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_proposals_session ON public.memory_proposals USING btree (session_id);


--
-- Name: INDEX idx_proposals_session; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_proposals_session IS '按会话查其产生的建议';


--
-- Name: idx_proposals_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_proposals_user ON public.memory_proposals USING btree (user_id, status);


--
-- Name: INDEX idx_proposals_user; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_proposals_user IS '按用户查待确认建议（卡片列表）';


--
-- Name: idx_scheduled_tasks_enabled_next; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_scheduled_tasks_enabled_next ON public.scheduled_tasks USING btree (enabled, next_run_at);


--
-- Name: INDEX idx_scheduled_tasks_enabled_next; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_scheduled_tasks_enabled_next IS '调度器扫到期启用任务（重启补偿补跑依据）';


--
-- Name: idx_scheduled_tasks_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_scheduled_tasks_user ON public.scheduled_tasks USING btree (user_id);


--
-- Name: INDEX idx_scheduled_tasks_user; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_scheduled_tasks_user IS '任务列表按归属人过滤';


--
-- Name: idx_session_settings_assistant; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_session_settings_assistant ON public.session_settings USING btree (assistant_id);


--
-- Name: INDEX idx_session_settings_assistant; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_session_settings_assistant IS '按绑定助手过滤会话';


--
-- Name: idx_session_settings_task; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_session_settings_task ON public.session_settings USING btree (schedule_task_id) WHERE (schedule_task_id IS NOT NULL);


--
-- Name: INDEX idx_session_settings_task; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_session_settings_task IS '按定时任务查运行历史（部分索引，仅定时会话）';


--
-- Name: idx_session_settings_user_id_desc; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_session_settings_user_id_desc ON public.session_settings USING btree (user_id, session_id DESC);


--
-- Name: INDEX idx_session_settings_user_id_desc; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_session_settings_user_id_desc IS '会话列表按用户过滤 + 创建时间倒序（UUID v7 字符串倒序）';


--
-- Name: idx_user_identities_provider; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_user_identities_provider ON public.user_identities USING btree (provider);


--
-- Name: INDEX idx_user_identities_provider; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_user_identities_provider IS '按平台筛选（统计/管理）';


--
-- Name: idx_user_identities_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_user_identities_user ON public.user_identities USING btree (user_id);


--
-- Name: INDEX idx_user_identities_user; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.idx_user_identities_user IS '按用户查其绑定的所有第三方身份';


--
-- Name: uq_api_tokens_hash; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_api_tokens_hash ON public.api_tokens USING btree (token_hash);


--
-- Name: INDEX uq_api_tokens_hash; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.uq_api_tokens_hash IS '令牌哈希唯一索引；同时作为 Bearer 验证的查找键（O(1)）';


--
-- Name: uq_assistants_share_token; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_assistants_share_token ON public.assistants USING btree (share_token) WHERE ((share_token)::text <> ''::text);


--
-- Name: INDEX uq_assistants_share_token; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.uq_assistants_share_token IS '分享口令全局唯一（部分唯一索引，跳过空口令避免大量空值冲突）';


--
-- Name: uq_llm_models_default; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_llm_models_default ON public.llm_models USING btree (user_id) WHERE (is_default = true);


--
-- Name: INDEX uq_llm_models_default; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.uq_llm_models_default IS '每用户至多一个默认 chat 模型（部分唯一索引，仅 is_default=TRUE 行参与，按 user_id）';


--
-- Name: uq_llm_models_embedding_default; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_llm_models_embedding_default ON public.llm_models USING btree (user_id) WHERE (embedding_default = true);


--
-- Name: INDEX uq_llm_models_embedding_default; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.uq_llm_models_embedding_default IS '每用户至多一个默认 embedding 模型（部分唯一索引，仅 embedding_default=TRUE 行参与，按 user_id）';


--
-- Name: uq_users_username; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX uq_users_username ON public.users USING btree (username);


--
-- Name: INDEX uq_users_username; Type: COMMENT; Schema: public; Owner: -
--

COMMENT ON INDEX public.uq_users_username IS 'username 唯一索引；PG/MySQL 唯一索引允许多个 NULL，SSO-only 用户互不冲突';


--
-- Name: api_tokens api_tokens_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.api_tokens
    ADD CONSTRAINT api_tokens_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: kb_chunks kb_chunks_document_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kb_chunks
    ADD CONSTRAINT kb_chunks_document_id_fkey FOREIGN KEY (document_id) REFERENCES public.kb_documents(id) ON DELETE CASCADE;


--
-- Name: kb_documents kb_documents_kb_instance_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.kb_documents
    ADD CONSTRAINT kb_documents_kb_instance_id_fkey FOREIGN KEY (kb_instance_id) REFERENCES public.kb_instances(id) ON DELETE CASCADE;


--
-- Name: llm_models llm_models_provider_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.llm_models
    ADD CONSTRAINT llm_models_provider_id_fkey FOREIGN KEY (provider_id) REFERENCES public.llm_providers(id) ON DELETE CASCADE;


--
-- Name: user_identities user_identities_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_identities
    ADD CONSTRAINT user_identities_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--


