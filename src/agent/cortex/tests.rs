use super::*;
use std::collections::HashMap;

use adk_rust::{Agent, InvocationContext, async_trait};

mod normalize_tests {
    use super::*;

    fn text(role: &str, t: &str) -> Content {
        Content {
            role: role.to_string(),
            parts: vec![Part::Text {
                text: t.to_string(),
            }],
        }
    }

    fn fc(id: &str, name: &str) -> Content {
        Content {
            role: "model".to_string(),
            parts: vec![Part::FunctionCall {
                name: name.to_string(),
                args: json!({}),
                id: Some(id.to_string()),
                thought_signature: None,
            }],
        }
    }

    fn fr(id: &str, name: &str) -> Content {
        Content {
            role: "function".to_string(),
            parts: vec![Part::FunctionResponse {
                function_response: FunctionResponseData::new(
                    name.to_string(),
                    json!({"result": "ok"}),
                ),
                id: Some(id.to_string()),
                annotations: None,
            }],
        }
    }

    fn count_fr(c: &Content) -> usize {
        c.parts
            .iter()
            .filter(|p| matches!(p, Part::FunctionResponse { .. }))
            .count()
    }

    #[test]
    fn paired_history_is_unchanged() {
        let mut conv = vec![
            text("system", "sys"),
            text("user", "q"),
            fc("c1", "shell"),
            fr("c1", "shell"),
            text("model", "done"),
        ];
        let len = conv.len();
        normalize_function_pairs(&mut conv);
        assert_eq!(conv.len(), len, "正常配对不应增删消息");
        assert!(conv.iter().any(|c| count_fr(c) == 1));
    }

    #[test]
    fn removes_orphan_function_response() {
        let mut conv = vec![
            text("system", "sys"),
            text("user", "q"),
            fr("ghost", "shell"),
            text("user", "more"),
        ];
        normalize_function_pairs(&mut conv);
        assert!(
            !conv.iter().any(|c| count_fr(c) > 0),
            "孤立 FunctionResponse 应被删除"
        );
    }

    #[test]
    fn backfills_missing_function_response() {
        let mut conv = vec![text("system", "sys"), text("user", "q"), fc("c1", "shell")];
        normalize_function_pairs(&mut conv);
        assert_eq!(conv.len(), 4);
        let placeholder = &conv[3];
        assert_eq!(placeholder.role, "function");
        assert_eq!(count_fr(placeholder), 1);
    }

    #[test]
    fn orphan_fr_after_compaction_split_is_removed() {
        let mut conv = vec![
            text("system", "sys"),
            text("user", "q"),
            fc("c1", "read_a"),
            fr("c1", "read_a"),
            fr("c2", "read_b"),
            text("user", "q2"),
        ];
        normalize_function_pairs(&mut conv);
        let fr_ids: Vec<String> = conv
            .iter()
            .flat_map(|c| c.parts.iter())
            .filter_map(|p| match p {
                Part::FunctionResponse { id, .. } => id.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(
            fr_ids,
            vec!["c1".to_string()],
            "只应保留配对完整的 FR(c1)，孤立的 c2 应删"
        );
    }

    #[test]
    fn empty_id_fc_gets_synthetic_placeholder() {
        let empty_id_fc = Content {
            role: "model".to_string(),
            parts: vec![Part::FunctionCall {
                name: "shell".to_string(),
                args: json!({}),
                id: None,
                thought_signature: None,
            }],
        };
        let mut conv = vec![text("system", "sys"), empty_id_fc];
        normalize_function_pairs(&mut conv);
        assert_eq!(conv.len(), 3, "空 id FC 应补占位 FR");
        let placeholder_id = conv[2]
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::FunctionResponse { id, .. } => id.clone(),
                _ => None,
            })
            .next()
            .expect("应有占位 FR id");
        assert!(
            placeholder_id.starts_with("call_s"),
            "占位 id 应为合成 id: {placeholder_id}"
        );
        let fc_id = conv[1]
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::FunctionCall { id, .. } => id.clone(),
                _ => None,
            })
            .next();
        assert_eq!(
            fc_id,
            Some(placeholder_id),
            "FC 本体应回写与占位 FR 相同的 id"
        );
    }

    #[test]
    fn multiple_orphan_fcs_in_one_message_each_backfilled() {
        let multi = Content {
            role: "model".to_string(),
            parts: vec![
                Part::FunctionCall {
                    name: "a".to_string(),
                    args: json!({}),
                    id: Some("ia".to_string()),
                    thought_signature: None,
                },
                Part::FunctionCall {
                    name: "b".to_string(),
                    args: json!({}),
                    id: Some("ib".to_string()),
                    thought_signature: None,
                },
            ],
        };
        let mut conv = vec![text("system", "sys"), multi];
        normalize_function_pairs(&mut conv);
        assert_eq!(conv.len(), 4, "应补 2 条占位 FR（system + model + 2 占位）");
        let ids: Vec<String> = conv[2..]
            .iter()
            .flat_map(|c| c.parts.iter())
            .filter_map(|p| match p {
                Part::FunctionResponse { id, .. } => id.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec!["ia".to_string(), "ib".to_string()],
            "占位 FR 按 FC 原序插入"
        );
    }
}

/// 回归：v1.0.0 起 assistant 正文不落库的根因——RunEndGuard 收尾 cancel 了 SSE 层
/// 共享 token，stream.rs 的 `!is_cancelled()` 落库门槛在自然收尾时恒为假。
/// 修复 = 根 run 派生树级 child_token，guard 只 cancel child。本组测试锁住两个方向：
/// ① 自然收尾不得置位注入的（父）token；② 父 token 取消仍能级联熔断 run。
mod run_end_token_tests {
    use super::*;
    use adk_rust::session::{CreateRequest, InMemorySessionService, SessionService};
    use adk_rust::{FinishReason, Llm, LlmRequest, LlmResponse, LlmResponseStream, Result};
    use futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 单轮文本 stub 模型：一次返回 turn_complete 完整文本，计数调用次数。
    struct OneShotTextModel {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Llm for OneShotTextModel {
        fn name(&self) -> &str {
            "stub-one-shot"
        }
        async fn generate_content(
            &self,
            _req: LlmRequest,
            _stream: bool,
        ) -> Result<LlmResponseStream> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let resp = LlmResponse {
                content: Some(Content {
                    role: "model".to_string(),
                    parts: vec![Part::Text {
                        text: "done".to_string(),
                    }],
                }),
                finish_reason: Some(FinishReason::Stop),
                turn_complete: true,
                ..Default::default()
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(resp) })))
        }
    }

    /// 装配一个带注入 token 的 root agent + 对应 run 上下文（内存会话）。
    async fn assemble(
        token: CancellationToken,
    ) -> (
        Arc<dyn Agent>,
        Arc<dyn InvocationContext>,
        Arc<OneShotTextModel>,
    ) {
        let service = InMemorySessionService::new();
        let session = service
            .create(CreateRequest {
                app_name: "app".to_string(),
                user_id: "u".to_string(),
                session_id: Some("s-regress".to_string()),
                state: HashMap::new(),
            })
            .await
            .expect("create session");
        let stub = Arc::new(OneShotTextModel {
            calls: AtomicUsize::new(0),
        });
        let agent: Arc<dyn Agent> = Arc::new(
            CortexAgentBuilder::new("root")
                .model(stub.clone())
                .instruction("test")
                .cancel_token(token)
                .build()
                .expect("build agent"),
        );
        let ctx = adk_rust::runner::InvocationContext::new(
            "inv-regress".to_string(),
            agent.clone(),
            "u".to_string(),
            "app".to_string(),
            "s-regress".to_string(),
            Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: "hi".to_string(),
                }],
            },
            Arc::from(session),
        )
        .expect("build ctx");
        (agent, Arc::new(ctx), stub)
    }

    #[tokio::test]
    async fn natural_run_end_leaves_injected_token_clean() {
        let parent = CancellationToken::new();
        let (agent, ctx, _stub) = assemble(parent.clone()).await;
        let mut s = agent.run(ctx).await.expect("run");
        while s.next().await.is_some() {}
        assert!(
            !parent.is_cancelled(),
            "自然收尾后注入的 SSE token 被置位 —— RunEndGuard 又 cancel 到了父 token，\
             stream.rs 落库门槛会全部失效（模型正文不落库回归）"
        );
    }

    #[tokio::test]
    async fn parent_cancel_still_short_circuits_run() {
        let parent = CancellationToken::new();
        parent.cancel();
        let (agent, ctx, stub) = assemble(parent.clone()).await;
        let mut s = agent.run(ctx).await.expect("run");
        while s.next().await.is_some() {}
        assert_eq!(
            stub.calls.load(Ordering::Relaxed),
            0,
            "父 token 预先取消应经 child_token 级联在循环顶熔断，不发起 LLM 调用"
        );
    }
}

mod estimate_tests {
    use super::*;

    #[test]
    fn function_call_args_are_counted_not_flat_64() {
        let big = "x".repeat(40_000);
        let conv = vec![Content {
            role: "model".to_string(),
            parts: vec![Part::FunctionCall {
                name: "create_file".to_string(),
                args: json!({ "path": "a.txt", "content": big }),
                id: Some("c1".to_string()),
                thought_signature: None,
            }],
        }];
        assert!(estimate_conv_tokens(&conv, 4) >= 10_000);
    }

    #[test]
    fn flat_parts_still_floor_at_64() {
        let conv = vec![Content {
            role: "model".to_string(),
            parts: vec![Part::FunctionCall {
                name: "ls".to_string(),
                args: json!({}),
                id: Some("c1".to_string()),
                thought_signature: None,
            }],
        }];
        assert!(estimate_conv_tokens(&conv, 4) >= 16);
    }
}
