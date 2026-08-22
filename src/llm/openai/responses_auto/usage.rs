//! /responses usage 口径归一——净输入动态判定 + gross 折算。
//!
//! cortex 的上下文治理契约是 gross total（占用 = prompt + completion，不减
//! cache_read——见 cortex_agent/mod.rs 的口径注释）。OpenAI 官方 /responses 的
//! `input_tokens` 含 cached_tokens，上游 convert_usage 的 `total = input +
//! output` 天然 gross；但第三方 /responses 兼容网关（中转类）可能按 Anthropic
//! 风格把 `input_tokens` 报成**净输入**（cache 另计），占用随之被低估——
//! 软/硬闸延迟触发、切模型后剩余百分比跳变。
//!
//! 判定信号（数学上密闭）：cortex 对 /responses 每轮发送**全量对话**（无
//! previous_response_id 服务端状态），gross 口径的 input_tokens 随对话增长
//! 单调不减；若同一对话前缀扩展（指纹匹配）而上报 input 反而明显下降，
//! 唯一的解释是该端点报净输入。此后把 cached_tokens 折回 prompt/total
//! （对齐 anthropic_custom 的 gross 折算）。判定证据在诚实报数的 gross 端点
//! 上不可能出现；未触发判定的端点 usage 逐字节透传。
//!
//! **证据帧须 cached > input（决定性）**：gross 口径下 cached ⊆ input 恒成立
//! （OpenAI 官方语义），该条件使诚实 gross 端点**无论计数漂移、多上游轮询
//! 路由还是重复帧都产生不了证据**；净口径稳态帧 cache≈全前缀、input≈本轮
//! 增量，天然满足。
//!
//! **两次独立证据才 latch + 同指纹只计一次**：防的是单帧异常——网关一次性
//! 把 input/cached 字段报反、瞬时 bug 帧、混合口径网关的偶发串台——一次
//! 异常不得永久改变端点判定。净口径端点的证据天然重复（input≈本轮增量随
//! 轮次大小波动），通常 1-3 轮内确认，代价仅晚 1-2 轮。
//!
//! 已知口径差异：adk 上游 `with_usage_tracking` 的 gen_ai.usage 遥测在流经
//! 本层**之前**记录，latched 端点的 span 指标是折算前净值；计费/持久化走
//! 的都是折算后的 emit_usage，无功能影响（观测层无法在上游内部插钩）。

use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;

use adk_rust::{Content, LlmResponse};

/// /responses usage 口径缓存：key 同 RESPONSES_SUPPORT（`base_url|model|SHA-256(api_key)`，
/// 见 probe.rs）。
///
/// `net_input` 一旦置位进程内永不复位（写单调）：净口径是端点实现属性，不会逐请求
/// 漂移；误置位只可能来自端点报数自相矛盾（gross 却对纯扩展的对话报出下降的
/// input_tokens）——那本身就是需要治理的坏数据，按净口径折算反而是合理的止损。
///
/// `recent` 保留最近 [`RECENT_FP_LIMIT`] 次请求的指纹与原始 input_tokens（FIFO）。
/// 用环形多条而非单条 baseline：query_understanding / 标题生成 / 子 agent 等
/// 无关会话可能与主会话**逐轮交错**（且常解析到同端点同模型 → 同一缓存条目），
/// 单条 baseline 下主请求的 prev 永远是无关会话的指纹，纯聊天（无工具循环）
/// 场景检测永久失明。多条记录让主会话与 ring 里任意历史请求比对前缀，交错
/// 不再挡判定；安全性质不变——判定仍要求「前缀扩展 + 明显收缩」同时成立，
/// gross 端点在数学上触发不了（见模块文档）。
#[derive(Default)]
pub(super) struct UsageConventionEntry {
    /// 端点已判定为净输入口径（input_tokens 不含 cache）。
    /// pub(super)：mod.rs 的测试断言 latch 状态用
    pub(super) net_input: bool,
    /// 已累计的独立收缩证据次数。**两次才 latch**（`net_input`）：单次证据可能
    /// 是单帧异常（字段报反 / 瞬时 bug 帧 / 混合口径网关的一次串台）——一次
    /// 异常不得永久改变端点判定；诚实 gross 端点被 `cached > input` 条件整体
    /// 排除在证据之外（见模块文档）。净口径端点每轮 input≈本轮增量、随轮次
    /// 大小来回波动，证据自然重复出现，通常 1-3 轮内确认。计数器跨请求/跨 run
    /// 持久（挂在端点条目上）。
    net_evidence: u32,
    /// 上一次计证据请求的全量指纹：同一指纹（重发 / 用户 regenerate）不得重复
    /// 计证据——否则「故障转移漂移 + 重新生成同一请求」会用同一条观察凑满两次。
    /// 正常会话逐轮扩展、指纹必然不同，该守卫零误伤。
    last_evidence_fp: Option<u64>,
    /// 首见原始 usage 已记日志（端点口径实证用，避免刷屏）
    logged_raw: bool,
    /// 最近若干次请求的指纹与原始 input（见类型文档）
    recent: VecDeque<PrevUsageReq>,
}

/// 单条指纹记录（见 [`UsageConventionEntry`]）。
#[derive(Clone, Copy)]
pub(super) struct PrevUsageReq {
    /// contents 条数
    items: usize,
    /// 全量对话的滚动哈希（见 [`conv_fingerprint`]）
    hash: u64,
    /// 上报的原始 input_tokens（净/gross 均按原样记录，比较必须在同一口径下）
    input_tokens: i64,
}

/// 指纹 ring 容量：主会话与 QU/标题/子 agent 交错时典型间隔 1-3 个无关请求，
/// 8 条足够覆盖一轮工具循环 + 多路并发；每条 24B，内存可忽略
const RECENT_FP_LIMIT: usize = 8;

/// std::sync::Mutex（非 tokio）：临界区纯计算、无 await；同步锁避免把纯内存
/// 操作排进异步调度器。与 RESPONSES_SUPPORT 的选型差异即在于此。
pub(super) static RESPONSES_USAGE_CONVENTION: LazyLock<
    std::sync::Mutex<HashMap<String, UsageConventionEntry>>,
> = LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// 对话指纹：对 contents 逐条做 FNV-1a 滚动哈希并保留前缀累计值。
///
/// `prefix_hash(k)` 可与任意更早请求的全量哈希比对，判定「本次请求是否为旧请求
/// 的纯扩展」（对话只增未减）。哈希对象是条目的 Debug 表示——只要求确定性，
/// 不要求跨版本稳定（比对双方在同一进程内产生）。
pub(super) struct ConvFingerprint {
    /// cum[i] = 前 i+1 条的滚动哈希
    cum: Vec<u64>,
}

/// FNV-1a 参数
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

pub(super) fn conv_fingerprint(contents: &[Content]) -> ConvFingerprint {
    let mut cum = Vec::with_capacity(contents.len());
    let mut h = FNV_OFFSET;
    for c in contents {
        let repr = format!("{c:?}");
        for b in repr.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        // 条目边界分隔：防相邻条目的 Debug 串拼接歧义
        h ^= 0x1f;
        h = h.wrapping_mul(FNV_PRIME);
        cum.push(h);
    }
    ConvFingerprint { cum }
}

impl ConvFingerprint {
    pub(super) fn items(&self) -> usize {
        self.cum.len()
    }

    /// 前 k 条的滚动哈希（k ≥ 1）；与更早请求的 [`PrevUsageReq::hash`] 比对即
    /// 「前 k 条逐条相同」。k = 0（空对话）无意义，返回 None 不参与判定。
    pub(super) fn prefix_hash(&self, k: usize) -> Option<u64> {
        k.checked_sub(1).and_then(|i| self.cum.get(i)).copied()
    }

    pub(super) fn full_hash(&self) -> u64 {
        self.cum.last().copied().unwrap_or(FNV_OFFSET)
    }
}

/// 末帧 usage 口径归一（gross 契约，见模块文档）。
///
/// 仅末帧带 usage_metadata（上游只在 response.completed 帧填充；partial 帧恒无，
/// 防御性跳过）。判定与折算的完整语义见 [`UsageConventionEntry`] 与模块文档：
/// 未触发净口径判定的端点，usage 各字段逐字节透传。
pub(super) fn normalize_responses_usage(
    resp: &mut LlmResponse,
    fp: &ConvFingerprint,
    cache_key: &str,
    log_tag: &str,
) {
    if resp.partial {
        return;
    }
    let Some(um) = resp.usage_metadata.as_mut() else {
        return;
    };
    let input = um.prompt_token_count as i64;
    let cached = um.cache_read_input_token_count.unwrap_or(0).max(0) as i64;
    let output = um.candidates_token_count as i64;

    // 临界区纯计算无 panic 路径；万一被上游 tracing 钩子毒化也降级继续
    // （HashMap 结构一致性不受影响，对齐 cortex_agent/mod.rs 的锁惯例）
    let mut cache = RESPONSES_USAGE_CONVENTION
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let entry = cache.entry(cache_key.to_string()).or_default();

    if !entry.logged_raw {
        entry.logged_raw = true;
        tracing::info!(
            "[openai-auto] /responses 原始 usage 首见（{log_tag}）: input={input} \
             cached={cached} output={output} total={}",
            um.total_token_count
        );
    }

    if !entry.net_input
        && let Some(hit) = entry.recent.iter().find(|p| {
            // gross 口径在纯扩展的对话上 input 单调不减；明显下降只能是净口径
            // （input 不含 cache）。margin 吸收同一前缀 tokenization 的零星抖动。
            // 对 ring 里任意一条比对（见 UsageConventionEntry 的交错说明）。
            fp.items() > p.items
                && fp.prefix_hash(p.items) == Some(p.hash)
                && input <= p.input_tokens.saturating_sub((p.input_tokens / 50).max(512))
            // 决定性净口径信号：cache 大于 input。gross 口径下 cached ⊆ input
            // 恒成立（OpenAI 官方语义），本条件使诚实 gross 端点**无论计数漂移、
            // 轮询路由还是重复帧都产生不了证据**（防多上游聚合网关误判）；净口径
            // 稳态帧 cache≈全前缀、input≈本轮增量，天然满足。零缓存帧（cached=0）
            // 恒不满足——但 latch 后对它折算也是 no-op，漏检无害
            && cached > input
        })
        // 同一指纹的重发/regenerate 不重复计证据（见 last_evidence_fp 文档）
        && entry.last_evidence_fp != Some(fp.full_hash())
    {
        entry.net_evidence = entry.net_evidence.saturating_add(1);
        entry.last_evidence_fp = Some(fp.full_hash());
        if entry.net_evidence == 1 {
            // 第一次证据：可能是单帧异常（字段报反 / 瞬时 bug 帧 / 混合口径
            // 串台），只记观察不动作，等第二次独立证据再 latch（见 net_evidence）
            tracing::info!(
                "[openai-auto] /responses usage 观察：疑似净输入（input_tokens 不含 \
                 cache，{log_tag}）: input {} → {}（对话 {} → {} 条，只增未减），\
                 再现一次即判定",
                hit.input_tokens,
                input,
                hit.items,
                fp.items()
            );
        } else {
            entry.net_input = true;
            tracing::warn!(
                "[openai-auto] /responses usage 口径判定：净输入（input_tokens 不含 \
                 cache，{log_tag}）: 第 {} 次证据 input {} → {}（对话 {} → {} 条，\
                 只增未减），后续把 cached_tokens 折回 gross",
                entry.net_evidence,
                hit.input_tokens,
                input,
                hit.items,
                fp.items()
            );
        }
    }

    if entry.net_input && cached > 0 {
        let prompt_gross = input + cached;
        um.prompt_token_count = prompt_gross.clamp(0, i32::MAX as i64) as i32;
        um.total_token_count = (prompt_gross + output).clamp(0, i32::MAX as i64) as i32;
        tracing::debug!(
            "[openai-auto] /responses usage 折算 gross（{log_tag}）: input {input} + \
             cached {cached} → prompt {prompt_gross}，total {}",
            prompt_gross + output
        );
    }

    // ring 记录原始（折算前）input：跨请求比较必须同口径（净 vs 净）
    entry.recent.push_back(PrevUsageReq {
        items: fp.items(),
        hash: fp.full_hash(),
        input_tokens: input,
    });
    if entry.recent.len() > RECENT_FP_LIMIT {
        entry.recent.pop_front();
    }
}
