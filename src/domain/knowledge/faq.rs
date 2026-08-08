//! FAQ 自动学习 — 走多 provider（与普通文档同一套抽象，不再 dify 特例）。
//!
//! 流程：
//! - 候选生成/重生成（`generate_candidates`/`regenerate_candidates`）：LLM 从会话提取 FAQ 候选，
//!   `mark_duplicates` 走 `list_instance` 查重（按归一化标题）。
//! - 提交写入（`commit_faqs`）：每条 FAQ 调 `upload_to_instance` 写入指定知识库实例
//!   （dify 或内置都行，由实例 provider 决定）。
//!
//! `brand`/`dev_type` 既作为 LLM 提取的会话上下文（prompt 输入），也在 `commit_faqs` 提交时
//! 写入文档 metadata（供内置 provider 检索过滤：填了要遵守，不填都符合）。

use super::faq_helpers::{build_candidate, merge_similar_candidates, normalize_topic_key};
use super::{FaqCandidate, KnowledgeManager, MAX_FAQ_CONTENT_CHARS, backend};
use crate::error::AppError;
use adk_rust::{Content, Llm, LlmRequest};
use futures::StreamExt;
use std::sync::Arc;

impl KnowledgeManager {
    // ============ FAQ 学习（两阶段：先返回候选供前端审查，再提交写入） ============

    /// 第一阶段：从会话生成 FAQ 候选（不写入），并标记与现有文档的重名情况。
    ///
    /// `instance_id`：查重的目标知识库实例；`brand`/`dev_type`：LLM 提取的会话上下文。
    pub async fn generate_candidates(
        &self,
        instance_id: &str,
        brand: &str,
        dev_type: &str,
        dev_model: &str,
        raw_conversation: &str,
        model: Arc<dyn Llm>,
        model_name: &str,
    ) -> Result<Vec<FaqCandidate>, AppError> {
        let prepared = Self::prepare_conversation(raw_conversation);
        let conversation = self
            .compress_if_too_long(&prepared, &model, model_name)
            .await;

        if conversation.trim().is_empty() {
            return Err(AppError::BusinessError(
                "清洗后会话内容为空，无法提取 FAQ".to_string(),
            ));
        }

        let mut candidates = self
            .extract_faqs(
                brand,
                dev_type,
                dev_model,
                &conversation,
                None,
                None,
                &model,
                model_name,
            )
            .await;
        candidates = merge_similar_candidates(candidates);
        self.mark_duplicates(instance_id, &mut candidates).await;
        Ok(candidates)
    }

    /// 第一阶段（变体）：对指定主题重新生成 FAQ 候选。
    #[allow(clippy::too_many_arguments)]
    pub async fn regenerate_candidates(
        &self,
        instance_id: &str,
        brand: &str,
        dev_type: &str,
        dev_model: &str,
        raw_conversation: &str,
        target_title: Option<&str>,
        feedback: Option<&str>,
        model: Arc<dyn Llm>,
        model_name: &str,
    ) -> Result<Vec<FaqCandidate>, AppError> {
        let prepared = Self::prepare_conversation(raw_conversation);
        let conversation = self
            .compress_if_too_long(&prepared, &model, model_name)
            .await;

        if conversation.trim().is_empty() {
            return Err(AppError::BusinessError(
                "清洗后会话内容为空，无法提取 FAQ".to_string(),
            ));
        }

        let mut candidates = self
            .extract_faqs(
                brand,
                dev_type,
                dev_model,
                &conversation,
                target_title,
                feedback,
                &model,
                model_name,
            )
            .await;
        candidates = merge_similar_candidates(candidates);
        self.mark_duplicates(instance_id, &mut candidates).await;
        Ok(candidates)
    }

    /// 第二阶段：提交用户勾选的 FAQ 候选，写入指定知识库实例（走 provider.upload）。
    ///
    /// 每条 FAQ 作为一篇文档上传（title=主题，content=正文）。
    /// 返回成功写入的数量。
    pub async fn commit_faqs(
        &self,
        instance_id: &str,
        brand: &str,
        dev_type: &str,
        model: &str,
        items: &[FaqCandidate],
    ) -> Result<usize, AppError> {
        let mut count = 0;
        for item in items {
            let inp = backend::KbDocInput {
                brand: brand.to_string(),
                dev_type: dev_type.to_string(),
                model: model.to_string(),
                firmware_ver: String::new(),
                title: item.title.clone(),
                content: item.content.clone(),
                user_role: "faq".to_string(),
            };
            match self.upload_whole_to_instance(instance_id, inp).await {
                Ok(_) => count += 1,
                Err(e) => tracing::warn!(
                    "[commit_faqs] 写入 FAQ 失败: instance={}, title={}, err={}",
                    instance_id,
                    item.title,
                    e
                ),
            }
        }
        tracing::info!(
            "[commit_faqs] instance={} 写入 {} 条 FAQ（共提交 {} 条）",
            instance_id,
            count,
            items.len()
        );
        Ok(count)
    }

    /// 批量标记候选与现有知识库的重名情况（走 list_instance，按归一化标题匹配）。
    ///
    /// 基于归一化主题键模糊匹配，避免同一主题以略不同标题重复入库。
    async fn mark_duplicates(&self, instance_id: &str, candidates: &mut [FaqCandidate]) {
        let f = backend::KbListFilter {
            page: 1,
            limit: 500,
            brand: None,
            dev_type: None,
            keyword: None,
        };
        let existing: std::collections::HashMap<String, String> =
            match self.list_instance(instance_id, f).await {
                Ok(page) => page
                    .data
                    .into_iter()
                    .filter_map(|d| {
                        let key = normalize_topic_key(&d.title);
                        if key.is_empty() {
                            None
                        } else {
                            Some((key, d.title))
                        }
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!("[mark_duplicates] 拉取文档列表失败({}), 跳过查重", e);
                    return;
                }
            };

        for c in candidates.iter_mut() {
            let key = normalize_topic_key(&c.title);
            if key.is_empty() {
                continue;
            }
            if let Some(existing_title) = existing.get(&key) {
                tracing::info!(
                    "[mark_duplicates] 命中近义同主题: 候选「{}」 ↔ 已有「{}」",
                    c.title,
                    existing_title
                );
                c.duplicate = true;
            }
        }
    }

    /// 用 LLM 从会话中提取 FAQ 候选
    ///
    /// - `target_title`：指定重新生成的主题（定向重生成，仅产出该主题）；为空则提取全部主题
    /// - `feedback`：用户补充的修改要求，原样注入 prompt
    #[allow(clippy::too_many_arguments)]
    async fn extract_faqs(
        &self,
        brand: &str,
        dev_type: &str,
        dev_model: &str,
        conversation: &str,
        target_title: Option<&str>,
        feedback: Option<&str>,
        model: &Arc<dyn Llm>,
        model_name: &str,
    ) -> Vec<FaqCandidate> {
        let target_clause = match target_title {
            Some(t) if !t.trim().is_empty() => format!(
                "\n## 本次任务（定向重生成）\n用户对之前生成的「{}」不满意，**只重新生成这一个主题**的 FAQ，输出数组中只包含这一条。",
                t
            ),
            _ => String::from(
                "\n## 提取规则\n1. 识别对话中涉及的**不同命令主题**（静态路由、端口配置、OSPF等是不同主题）\n2. 每个主题生成一个独立 FAQ\n3. 只涉及一个主题就输出一个\n4. **同义合并**：如果多个标题在描述同一件事（例：接口/端口、IPv6地址/IPv6、配置/设置），必须合并成同一条 FAQ，禁止输出含义重复的多条；保留用词最规范的标题（如「接口IPv6地址配置」优先于「端口IPv6配置」）",
            ),
        };

        let feedback_clause = match feedback {
            Some(f) if !f.trim().is_empty() => format!("\n## 用户额外要求\n{}", f),
            _ => String::new(),
        };

        let prompt = format!(
            r#"你是网络设备命令知识整理专家。分析以下对话，为每个命令主题生成标准化的 FAQ 文档。

你必须严格按模板输出，DeepSeek 等模型容易省略章节或示例，本任务禁止省略。每条 content 都必须完整包含 6 个二级标题，且标题名称、顺序、数量完全一致。

厂商: {}
设备类型: {}
设备型号: {}

完整对话:
{}{}{}

## 输出格式

**只输出 JSON 对象**，不要加任何说明文字或 markdown 代码围栏：
{{"faqs":[{{"title":"功能标题","content":"标准化文档正文"}}]}}

## title 命名规则
- 只描述功能意图，不含参数/IP/具体值，不加厂商和设备类型前缀
- 正确：静态路由配置、VLAN划分、OSPF区域配置、ACL访问控制
- 错误：H3C路由器配置到192.168.10.0/24下一跳10.0.0.1
- 控制在10字以内

## content 格式（严格按以下 6 部分，不要包含对话过程）

## 命令说明
（一句话）

## 命令格式
（**参数一律用方括号 [参数名]，严禁使用尖括号 <参数名>**）
[命令 [参数1] [参数2]]

## 参数说明
| 参数 | 说明 | 必填 | 示例 |
|------|------|------|------|
| [参数] | [说明] | 是 | [示例] |

## 配置示例
（必须给出可直接执行的完整命令；如果对话没有明确示例值，使用占位参数示例，如 [目标网段]、[掩码]、[下一跳IP]，不得留空，不得写"无"）

## 回退命令
（undo 命令，无则写"无"）

## 注意事项
（风险提示，无则写"无"）

## 关键约束（必须遵守）
1. 每条 content **控制在 {} 字以内**，删除冗余，保留命令知识本身
2. 命令格式与示例中的参数**只用方括号 [参数名]**，绝不能用 <参数名>
3. 只输出 JSON，content 中不要出现对话交互过程
4. 参数说明必须完整，示例必须可直接使用
5. content 必须且只能包含以下 6 个章节：命令说明、命令格式、参数说明、配置示例、回退命令、注意事项
6. 配置示例必须有内容；信息不足时用方括号占位参数构造通用示例，不允许缺失、不允许空白、不允许写"无"
7. 回退命令如果设备无对应命令才写"无"，否则必须给 undo/no shutdown/delete 等回退命令
8. 输出必须是合法 JSON 字符串，content 内换行使用 \n 转义，不要输出 markdown 代码块"#,
            brand, dev_type, dev_model, conversation, target_clause, feedback_clause, MAX_FAQ_CONTENT_CHARS
        );

        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "faqs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string", "description": "功能意图标题，10字以内" },
                            "content": { "type": "string", "description": "标准化命令文档正文，Markdown" }
                        },
                        "required": ["title", "content"]
                    }
                }
            },
            "required": ["faqs"]
        });

        let req = LlmRequest::new(model_name, vec![Content::new("user").with_text(&prompt)])
            .with_response_schema(schema)
            .with_config(adk_rust::GenerateContentConfig {
                max_output_tokens: Some(8192),
                temperature: Some(0.3),
                ..Default::default()
            });

        match model.generate_content(req, false).await {
            Ok(mut stream) => match stream.next().await {
                Some(Ok(resp)) => {
                    let text = resp
                        .content
                        .as_ref()
                        .map(|c| {
                            c.parts
                                .iter()
                                .filter_map(|p| p.text().map(|s| s.to_string()))
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    self.parse_faq_json(&text)
                }
                _ => Vec::new(),
            },
            Err(e) => {
                tracing::warn!("[extract_faqs] LLM 提取失败({})", e);
                Vec::new()
            }
        }
    }

    /// 从 LLM 输出中解析 FAQ 候选，并填充 char_count（duplicate 由调用方标记）
    fn parse_faq_json(&self, text: &str) -> Vec<FaqCandidate> {
        let trimmed = text.trim();

        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed)
            && let Some(arr) = obj.get("faqs").and_then(|v| v.as_array())
        {
            return arr
                .iter()
                .filter_map(|item| build_candidate(item.get("title")?, item.get("content")?))
                .collect();
        }

        let json_str = if let Some(start) = trimmed.find('[') {
            if let Some(end) = trimmed.rfind(']') {
                &trimmed[start..=end]
            } else {
                trimmed
            }
        } else {
            trimmed
        };

        match serde_json::from_str::<Vec<serde_json::Value>>(json_str.trim()) {
            Ok(arr) => arr
                .into_iter()
                .filter_map(|item| build_candidate(item.get("title")?, item.get("content")?))
                .collect(),
            Err(e) => {
                tracing::warn!(
                    "[parse_faq_json] JSON 解析失败: {}, raw={}",
                    e,
                    &text[..text.len().min(200)]
                );
                Vec::new()
            }
        }
    }
}
