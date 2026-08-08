//! `read_skill` 工具 — 让 LLM 主动拉取 skill 正文。
//!
//! 常驻注册在每个 custom agent 上(不受 enabled_tools 白名单约束)。
//! 模型看到 system prompt 里的 skill 目录后,可调用此工具按需拉取未提及 skill 的正文。

use std::sync::Arc;

use adk_rust::tool::FunctionTool;
use adk_rust::{
    ToolContext,
    serde_json::{Value, json},
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::skill::SkillService;

#[derive(Debug, Serialize, JsonSchema)]
struct ReadSkillParams {
    /// 要读取的 skill 名称(必须是目录中列出的)
    pub name: String,
}

pub fn create_read_skill_tool(svc: Arc<SkillService>, max_chars: usize) -> FunctionTool {
    FunctionTool::new(
        "read_skill",
        "读取指定 skill 的完整正文。参数 name 必须是目录中列出的 skill 名称。\
         返回 skill 的指令正文(带 <path> 路径信息,已去掉 frontmatter),按其指示执行任务。",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let svc = svc.clone();
            async move {
                let name = args["name"].as_str().unwrap_or("").trim().to_string();
                if name.is_empty() {
                    return Ok(json!({
                        "ok": false,
                        "message": "name 参数不能为空"
                    }));
                }
                match svc.read_skill_block(&name, max_chars) {
                    Some(text) => Ok(json!({
                        "ok": true,
                        "name": name,
                        "content": text
                    })),
                    None => Ok(json!({
                        "ok": false,
                        "message": format!("skill '{name}' 不存在")
                    })),
                }
            }
        },
    )
    .with_parameters_schema::<ReadSkillParams>()
}
