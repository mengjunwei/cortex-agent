//! 监控插件代码生成 Agent 的 system prompt 与工具集
//!
//! 与上一版的差异：
//! - 旧版生成 Rust 源码（.so），需要 nm 端 libloading 加载
//! - 新版生成 **Rhai 脚本**，cortex-agent 进程内直接执行
//! - 新增 `validate_monitor_plugin` 工具，LLM 生成后必须自检

use std::sync::Arc;

use adk_rust::tool::FunctionTool;
use adk_rust::{
    ToolContext,
    serde_json::{Value, json},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::infra::db::DbPool;
use crate::infra::redis::SharedRedisPool;
use crate::domain::monitor::PluginManager;

mod validate;
use validate::create_validate_tool;

const REDIS_KEY: &str = "device:cmd:exec";
const SNMP_RES_DATA_CHAN: &str = "snmp:res:data:chan:test";
const SNMP_COLLECT_TIMEOUT_SECS: u64 = 60;

/// 创建监控插件 agent 可用的工具集
///
/// 工具列表：
/// - `validate_monitor_plugin`：三层校验（L1 语法 + L2 sandbox + L3 adk-code）
/// - `lookup_device_id`：根据设备 IP 查询 device_id（查 device.device 表）
/// - `snmp_test_collect`：对设备做 SNMP 采集（后续从 Redis 读写）
/// - `register_monitor_plugin`：校验通过后注册插件到 PluginManager
pub fn create_monitor_plugin_tools(
    _cfg: &AppConfig,
    db_pool: Option<DbPool>,
    redis_pool: Option<SharedRedisPool>,
    plugin_manager: Option<Arc<PluginManager>>,
) -> Vec<FunctionTool> {
    let mut tools = vec![create_validate_tool()];

    if let Some(pool) = db_pool {
        tools.push(create_lookup_device_tool(pool));
    }
    if let Some(pool) = redis_pool {
        tools.push(create_snmp_test_collect_tool(pool));
    }

    if let Some(pm) = plugin_manager {
        tools.push(create_register_tool(pm));
    }
    tools
}

// ─── lookup_device_id 工具 ──────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct LookupDeviceParams {
    /// 设备 IP 地址
    pub device_ip: String,
}

fn create_lookup_device_tool(pool: DbPool) -> FunctionTool {
    FunctionTool::new(
        "lookup_device_id",
        "根据设备 IP 地址查询 device_id。只需提供设备 IP，返回 device_id 供后续 SNMP 采集使用。",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let pool = pool.clone();
            async move {
                let device_ip = args["device_ip"].as_str().unwrap_or("").to_string();
                if device_ip.is_empty() {
                    return Ok(json!({ "ok": false, "error": "device_ip 不能为空" }));
                }

                let mut conn = match pool.get().await {
                    Ok(c) => c,
                    Err(e) => {
                        return Ok(json!({ "ok": false, "error": format!("获取数据库连接失败: {}", e) }));
                    }
                };

                use diesel::sql_types;
                use diesel::deserialize::QueryableByName;
                use diesel_async::RunQueryDsl;

                #[derive(QueryableByName)]
                struct DeviceIdRow {
                    #[diesel(sql_type = sql_types::Text)]
                    id: String,
                }

                match diesel::sql_query("SELECT id::text FROM device.device WHERE ip = $1")
                    .bind::<sql_types::Text, _>(&device_ip)
                    .get_results::<DeviceIdRow>(&mut conn)
                    .await
                {
                    Ok(rows) if !rows.is_empty() => {
                        Ok(json!({ "ok": true, "device_id": rows[0].id }))
                    }
                    Ok(_) => {
                        Ok(json!({ "ok": false, "error": format!("未找到 IP 为 {} 的设备", device_ip) }))
                    }
                    Err(e) => {
                        Ok(json!({ "ok": false, "error": format!("查询设备失败: {}", e) }))
                    }
                }
            }
        },
    )
    .with_parameters_schema::<LookupDeviceParams>()
}

// ─── snmp_test_collect 工具 ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct SnmpOidParam {
    /// 要采集的 OID
    pub oid: String,
    /// 采集方式：get 或 walk，默认 get
    #[serde(default = "default_snmp_type")]
    pub method: String,
    /// OID 别名/说明，可选
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct SnmpTestCollectParams {
    /// 设备 IP 地址
    pub device_ip: String,
    /// 设备 ID（由 lookup_device_id 获取）
    pub device_id: String,
    /// prepare_oids() 返回的 OID 列表，格式：[{"oid":"...","method":"get|walk","label":"..."}]
    #[serde(default)]
    pub oids: Vec<SnmpOidParam>,
    /// 兼容旧单 OID 调用：要采集的 OID
    #[serde(default)]
    pub oid: Option<String>,
    /// 兼容旧单 OID 调用：采集方式 get 或 walk，默认 get
    #[serde(default = "default_snmp_type")]
    pub snmp_type: String,
}

fn default_snmp_type() -> String {
    "get".to_string()
}

#[derive(Debug, Clone)]
struct NormalizedOid {
    oid: String,
    method: String,
    label: Option<String>,
}

fn normalize_snmp_method(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "get" | "snmpget" => Some("get"),
        "walk" | "snmpwalk" => Some("walk"),
        _ => None,
    }
}

fn parse_snmp_oids(args: &Value) -> Result<Vec<NormalizedOid>, String> {
    let mut out = Vec::new();

    let array_value = args
        .get("oids")
        .or_else(|| args.get("prepare_oids"))
        .or_else(|| args.get("prepareOids"));

    if let Some(value) = array_value {
        let parsed_value = if let Some(s) = value.as_str() {
            serde_json::from_str::<Value>(s)
                .map_err(|e| format!("prepare_oids JSON 字符串解析失败: {e}"))?
        } else {
            value.clone()
        };

        let arr = parsed_value
            .as_array()
            .ok_or_else(|| "oids/prepare_oids 必须是数组".to_string())?;

        for item in arr {
            let oid = item
                .get("oid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if oid.is_empty() {
                continue;
            }

            let method_raw = item
                .get("method")
                .or_else(|| item.get("snmp_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("get");
            let Some(method) = normalize_snmp_method(method_raw) else {
                return Err(format!("OID {oid} 的 method 仅支持 get 或 walk"));
            };
            let label = item
                .get("label")
                .and_then(|v| v.as_str())
                .map(str::to_string);

            out.push(NormalizedOid {
                oid,
                method: method.to_string(),
                label,
            });
        }
    }

    // 兼容旧单 OID 参数：{ oid, snmp_type }
    if out.is_empty() {
        let oid = args["oid"].as_str().unwrap_or("").trim().to_string();
        if !oid.is_empty() {
            let method_raw = args["snmp_type"]
                .as_str()
                .or_else(|| args["method"].as_str())
                .unwrap_or("get");
            let Some(method) = normalize_snmp_method(method_raw) else {
                return Err("snmp_type 仅支持 get 或 walk".to_string());
            };
            out.push(NormalizedOid {
                oid,
                method: method.to_string(),
                label: None,
            });
        }
    }

    if out.is_empty() {
        return Err("oids/prepare_oids 不能为空，或提供兼容参数 oid".to_string());
    }

    Ok(out)
}

async fn collect_snmp_group(
    redis_pool: SharedRedisPool,
    device_ip: &str,
    device_id: &str,
    snmp_type: &str,
    oids: Vec<String>,
) -> Result<Value, String> {
    let task_id = uuid::Uuid::now_v7().to_string();
    let res_redis_key = format!("{SNMP_RES_DATA_CHAN}{task_id}");
    let params = json!({
        "task_id": task_id,
        "device_id": device_id,
        "device_ip": device_ip,
        "exec_type": "snmp",
        "err_over": true,
        "task_source": 3,
        "snmp_type": snmp_type,
        "res_redis_key": res_redis_key,
        "cmd_list": [oids],
    });
    let task_params =
        serde_json::to_string(&params).map_err(|e| format!("构造采集任务失败: {e}"))?;

    use bb8_redis::redis::AsyncCommands;

    let mut conn = redis_pool
        .get()
        .await
        .map_err(|e| format!("获取 Redis 连接失败: {e}"))?;

    let push_result: bb8_redis::redis::RedisResult<i64> = conn.lpush(REDIS_KEY, task_params).await;
    push_result.map_err(|e| format!("写入 SNMP 采集任务失败: {e}"))?;

    let started = std::time::Instant::now();
    loop {
        let res: bb8_redis::redis::RedisResult<Option<String>> =
            conn.rpop(&res_redis_key, None).await;
        match res {
            Ok(Some(body)) => {
                let parsed: Value = serde_json::from_str(&body)
                    .map_err(|e| format!("解析 SNMP 采集结果失败: {e}; raw={body}"))?;
                let content = parsed
                    .get("cmd_result")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|v| v.get("content"))
                    .cloned()
                    .unwrap_or(Value::Null);

                return Ok(json!({
                    "snmp_type": snmp_type,
                    "content": content,
                    "raw": parsed,
                }));
            }
            Ok(None) => {
                if started.elapsed() >= std::time::Duration::from_secs(SNMP_COLLECT_TIMEOUT_SECS) {
                    return Err(format!(
                        "SNMP {} 采集超时（{} 秒），res_redis_key={}",
                        snmp_type, SNMP_COLLECT_TIMEOUT_SECS, res_redis_key
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            Err(e) => return Err(format!("读取 SNMP 采集结果失败: {e}")),
        }
    }
}

fn try_merge_oid_values(groups: &[Value]) -> Option<Value> {
    let mut merged = serde_json::Map::new();
    for group in groups {
        let content = group.get("content")?;
        let value = if let Some(s) = content.as_str() {
            serde_json::from_str::<Value>(s).ok()?
        } else {
            content.clone()
        };
        let obj = value.as_object()?;
        for (key, value) in obj {
            merged.insert(key.clone(), value.clone());
        }
    }
    Some(Value::Object(merged))
}

/// SNMP 采集测试工具
///
/// 接收 prepare_oids() 返回的 OID 列表，按 method 拆成 get/walk 两组分别批量下发。
/// 将归一化 OID 列表按 get/walk 拆分，同时生成 oid_meta（保留 method/label）
fn split_oid_groups(normalized: Vec<NormalizedOid>) -> (Vec<Value>, Vec<String>, Vec<String>) {
    let mut oid_meta = Vec::new();
    let mut get_oids = Vec::new();
    let mut walk_oids = Vec::new();
    for item in normalized {
        oid_meta.push(json!({
            "oid": item.oid,
            "method": item.method,
            "label": item.label,
        }));
        if item.method == "walk" {
            walk_oids.push(item.oid);
        } else {
            get_oids.push(item.oid);
        }
    }
    (oid_meta, get_oids, walk_oids)
}

/// 采集单个 get/walk 分组：调用 collect_snmp_group 并回填 oids 字段
async fn collect_one_group(
    redis_pool: SharedRedisPool,
    device_ip: &str,
    device_id: &str,
    method: &str,
    oids: &[String],
) -> Result<Value, String> {
    let mut group =
        collect_snmp_group(redis_pool, device_ip, device_id, method, oids.to_vec()).await?;
    if let Some(obj) = group.as_object_mut() {
        obj.insert("oids".to_string(), json!(oids));
    }
    Ok(group)
}

/// 根据分组构造返回 content：单组直取 content，多组聚合为 [{snmp_type, content}]
fn build_content(groups: &[Value]) -> Value {
    if groups.len() == 1 {
        groups[0].get("content").cloned().unwrap_or(Value::Null)
    } else {
        json!(
            groups
                .iter()
                .map(|g| json!({
                    "snmp_type": g.get("snmp_type").cloned().unwrap_or(Value::Null),
                    "content": g.get("content").cloned().unwrap_or(Value::Null),
                }))
                .collect::<Vec<_>>()
        )
    }
}

fn create_snmp_test_collect_tool(redis_pool: SharedRedisPool) -> FunctionTool {
    FunctionTool::new(
        "snmp_test_collect",
        "对指定设备批量下发 SNMP 采集任务并等待结果。优先传入 prepare_oids() 返回的 oids 数组：[{oid, method, label}]。工具会自动按 get/walk 分组分别执行，避免 snmpget 与 snmpwalk 混用。兼容旧参数 oid + snmp_type。",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let redis_pool = redis_pool.clone();
            async move {
                let device_ip = args["device_ip"].as_str().unwrap_or("").to_string();
                let device_id = args["device_id"].as_str().unwrap_or("").to_string();

                if device_ip.is_empty() || device_id.is_empty() {
                    return Ok(json!({ "ok": false, "error": "device_ip, device_id 均不能为空" }));
                }

                let normalized_oids = match parse_snmp_oids(&args) {
                    Ok(v) => v,
                    Err(e) => return Ok(json!({ "ok": false, "error": e })),
                };
                let (oid_meta, get_oids, walk_oids) = split_oid_groups(normalized_oids);

                let mut groups = Vec::new();
                if !get_oids.is_empty() {
                    match collect_one_group(
                        redis_pool.clone(),
                        &device_ip,
                        &device_id,
                        "get",
                        &get_oids,
                    )
                    .await
                    {
                        Ok(g) => groups.push(g),
                        Err(e) => return Ok(json!({ "ok": false, "error": e, "snmp_type": "get" })),
                    }
                }
                if !walk_oids.is_empty() {
                    match collect_one_group(
                        redis_pool.clone(),
                        &device_ip,
                        &device_id,
                        "walk",
                        &walk_oids,
                    )
                    .await
                    {
                        Ok(g) => groups.push(g),
                        Err(e) => return Ok(json!({ "ok": false, "error": e, "snmp_type": "walk" })),
                    }
                }

                let merged_oid_values = try_merge_oid_values(&groups);
                let oid_values_json = merged_oid_values
                    .as_ref()
                    .and_then(|v| serde_json::to_string(v).ok());
                let content = build_content(&groups);

                Ok(json!({
                    "ok": true,
                    "content": content,
                    "oid_values": merged_oid_values,
                    "oid_values_json": oid_values_json,
                    "groups": groups,
                    "oids": oid_meta,
                }))
            }
        },
    )
    .with_parameters_schema::<SnmpTestCollectParams>()
}

// ─── register_monitor_plugin 工具 ──────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct RegisterParams {
    /// 插件 ID，可选。传入 UUID v7 格式则直接使用，否则系统自动生成 UUID v7
    plugin_id: String,
    /// 插件整体描述。首次发布时必填，简明说明该插件采集什么指标；后续迭代可不填（保留原描述）
    #[serde(default)]
    description: String,
    /// 已通过校验的 Rhai 脚本源码
    script: String,
    /// 本次发版的变更说明。首次发布写"首次发布：…"；迭代时总结本次改动，例如"修复 OID 缺失时返回错误码"
    #[serde(default)]
    change_description: String,
}

fn create_register_tool(pm: Arc<PluginManager>) -> FunctionTool {
    FunctionTool::new(
        "register_monitor_plugin",
        "将已通过校验的 Rhai 监控插件注册到系统。调用前必须先通过 validate_monitor_plugin 校验。注册成功后插件立即生效，可通过 SNMP 采集数据。首次发布必须填写 description（插件整体描述）；每次发布都必须填写 change_description（本次变更说明）。",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let pm = pm.clone();
            async move {
                let plugin_id = args["plugin_id"].as_str().unwrap_or("").to_string();
                let description = args["description"].as_str().unwrap_or("").to_string();
                let script = args["script"].as_str().unwrap_or("").to_string();
                let change_description = args["change_description"].as_str().unwrap_or("").to_string();

                if plugin_id.is_empty() {
                    return Ok(json!({ "ok": false, "error": "plugin_id 不能为空" }));
                }
                if script.is_empty() {
                    return Ok(json!({ "ok": false, "error": "script 不能为空" }));
                }
                if change_description.is_empty() {
                    return Ok(json!({ "ok": false, "error": "change_description（本次变更说明）不能为空" }));
                }

                match pm.register(&plugin_id, &description, &script, &change_description).await {
                    Ok((final_id, version)) => {
                        tracing::info!("[monitor-plugin] registered {} (requested: {}) v{}", final_id, plugin_id, version);
                        Ok(json!({
                            "ok": true,
                            "plugin_id": final_id,
                            "version": version,
                            "message": format!("插件注册成功，ID: {}，版本 v{}", final_id, version)
                        }))
                    }
                    Err(e) => {
                        tracing::warn!("[monitor-plugin] register {} failed: {e}", plugin_id);
                        Ok(json!({
                            "ok": false,
                            "error": format!("注册失败: {}", e)
                        }))
                    }
                }
            }
        },
    )
    .with_parameters_schema::<RegisterParams>()
}

/// LLM 系统提示词 —— 约束 LLM 生成合法 Rhai 监控插件
pub fn get_system_prompt() -> String {
    r#"你是网络监控插件代码生成专家。你通过**多轮对话**与用户协作，生成 **Rhai 脚本**（不是 Rust），用于采集网络设备的监控指标。

## 交互流程（必须严格遵守，不得跳过任何步骤）

你的工作分为 **四个阶段**，每个阶段都必须有用户确认才能进入下一阶段。

---

### 阶段一：信息收集

当用户提出监控需求时（例如"帮我做 CPU 利用率监控"），你**不能直接生成代码**，而是必须逐项收集并确认以下信息：

1. **监控指标**：用户想监控什么？（CPU/内存/流量/温度/运行时长等）
2. **SNMP OID**：
   - 询问用户是否已有 OID
   - 如果用户不确定，根据监控指标给出常见 OID 建议，让用户确认或修改
   - 必须确认每个 OID 的采集方式：`get`（单值）还是 `walk`（遍历多值）
3. **解析逻辑**：确认原始值是否需要计算转换（如百分比、单位换算、阈值判断等）

收集完以上信息后，**必须向用户确认是否有补充**：
> "我已收集到以下信息：\n- 监控指标：xxx\n- SNMP OID：xxx（get/walk）\n- 解析逻辑：xxx\n\n请问还有需要补充或修改的吗？"

**只有当用户明确表示"没有"/"确认"/"继续"后，才能进入阶段二。** 如果用户补充了新内容，更新信息后再次确认。

---

### 阶段二：OID 实测或模拟

进入此阶段后，必须确认 OID 采集结果来源：

1. **询问用户是否有目标设备可以实测**：
   > "是否有一台目标设备可以用于实测 OID 采集结果？如果有，请提供设备 IP 地址；如果没有，我将自动生成模拟数据用于测试。"

2. **有设备实测（只需 IP）**：
   - 用户提供设备 IP 后，调用 `lookup_device_id`（传入 device_ip）获取 device_id
   - 将阶段一确认的完整 OID 列表整理为 `prepare_oids()` 相同格式：`[{"oid":"...","method":"get|walk"}]`（多值场景按需加 `label`）
   - 调用一次 `snmp_test_collect`，传入 `device_ip`、`device_id`、`oids`（完整数组）。工具内部会自动把 `get` 和 `walk` 分成两组分别批量执行，禁止把 snmpget/snmpwalk 混在同一次 Redis 任务里
   - 除非某组采集失败需要重试，否则不要对每个 OID 单独调用 `snmp_test_collect`
   - 优先使用工具返回的 `oid_values_json` / `oid_values` 作为后续测试数据；如果返回内容不是标准 oidValues，再将采集结果整理成 oidValues 格式

3. **无设备模拟**：根据 OID 类型自动生成合理的模拟值：
   - 数值型 OID（如 CPU 利用率）：生成 0-100 的典型值
   - 字符串型 OID（如设备名称）：生成示例字符串
   - 时间型 OID（如 sysUpTime）：生成合理的厘秒值

4. **确认是否有补充**：
   > "OID 采集结果如下（实测/模拟）：\n- xxx: {值}\n\n请问还有需要补充的吗？"

**只有当用户确认后，才能进入阶段三。**

---

### 阶段三：代码生成与自检

1. **生成 Rhai 脚本**（一个 ```rhai 代码块），使用阶段二确认的 OID 和解析逻辑。

```rhai
fn prepare_oids() {
    `[{"oid":"<OID>","method":"get|walk"}]`
}

fn parse(oid_values_json) {
    // ... 解析逻辑 ...
    // 单值场景不加 label；多值场景（如多核 CPU）按需添加
    `[{"success":true,"value":{"number":<数值>}}]`
}
```

2. **生成测试用例**（一个 ```json 代码块），至少 2 个，其中正常值用例必须使用阶段二确认的实测/模拟数据：

```json
[
  {
    "name": "正常值（实测/模拟）",
    "action": "parse",
    "oid_values_json": "{\"<OID>\":{\"oid_value_type\":2,\"value_str\":\"\",\"value_num\":<值>}}",
    "expect_success": true
  },
  {
    "name": "OID 缺失",
    "action": "parse",
    "oid_values_json": "{}",
    "expect_success": false
  }
]
```

3. **调用 `validate_monitor_plugin` 工具自检**，传入 script + test_cases（mode 默认 "fast"）。如果校验失败，根据返回的 `summary` 修正脚本，最多重试 3 轮。

4. **校验通过后，必须用自然语言总结校验结果**（不要只丢 JSON 或技术细节）：
   > 总结格式示例：
   > "✅ 校验完成，脚本在以下场景表现正确：\n1. OID 构造：正确生成了采集所需的 SNMP OID（xxx）\n2. 正常值解析：模拟采集到 3 个采样点（35.0、72.0、48.0），正确取最大值 72.0 并按规则加 20 得到最终值 92.0\n3. 异常容错：当 OID 数据缺失时，正确返回失败并提示「cpuUtilization OID 缺失」，不会崩溃\n\n所有测试用例均已通过。"
   >
   > 规则：
   > - 每个测试用例的**意图**和**实际结果**都要用一句话说清楚
   > - 把技术性的 JSON/OID 转化为用户能理解的语言（如「取最大值」「单位换算」「阈值判断」）
   > - 失败的用例要说明**失败原因**和**影响**
   > - 总结末尾附上「所有测试用例均已通过」或具体的失败计数

5. **总结后展示代码并确认**：
   > "插件脚本和测试用例已生成并通过校验。以上是完整代码，请问是否需要修改？确认无误后我将注册插件。"

**只有当用户明确确认后，才能进入阶段四。** 如果用户要求修改，修改后重新校验、重新总结并再次确认。

---

### 阶段四：注册

1. **调用 `register_monitor_plugin` 注册插件**，参数说明：
   - `script`：已通过校验的 Rhai 脚本
   - `plugin_id`：可选——传入 UUID v7 格式则直接使用，不传或非 UUID v7 格式则系统自动生成。**新插件不要传 plugin_id**，让系统生成；已有插件迭代时传入原 plugin_id 即可在原版本基础上递增
   - `description`：**插件整体描述**。首次发布必填，用一句话说明该插件采集什么指标（例如"采集交换机 CPU 利用率，基于 HOST-RESOURCES-MIB"）；已有插件迭代更新时可不填或更新描述
   - `change_description`：**本次发版变更说明，必填**。
     - 首次发布写："首次发布：实现 XXX 指标采集"
     - 后续迭代总结本次改动，例如："修复 OID 缺失时未返回错误的问题"、"新增接口流量 walk 支持"、"优化解析精度，改用 f64"

2. **用自然语言告知用户最终结果**（不要只返回 ID 和版本号）：
   > 示例："✅ 插件已成功发布！\n- 插件 ID：xxx\n- 版本：v2\n- 变更说明：修复 OID 缺失时未返回错误的问题\n\n该插件现在可以用于采集交换机 CPU 利用率（基于 HOST-RESOURCES-MIB），已在测试中验证正常值解析和异常容错均表现正确。"
   >
   > 规则：
   > - 用一句话概括该插件的**实际用途**（采集什么指标、基于什么协议）
   > - 附上插件 ID、版本号、变更说明
   > - 让用户知道下一步可以怎么用（如"已可用于设备监控配置"）

---

## Rhai 脚本规范

### parse 返回值结构

```json
[
  {
    "success": true | false,
    "value": { "number": 1234.56 } | { "string": "文本" },
    "label": "可选别名",
    "errors": ["失败原因"]
  }
]
```

**`label` 字段使用规则（严格遵守）**：
- **单值场景（默认不加）**：只采集一个指标（如整机 CPU 利用率、设备运行时长），**不要加 `label`**
- **多值场景（需用户确认后才能加）**：一个插件返回多个值（如多核 CPU 每核心利用率、多接口流量），用 `label` 区分各值
- 添加 `label` 前**必须和用户确认**："该指标有多个值（如各 CPU 核心），是否需要为每个值添加标签区分？"
- 用户未明确要求时，一律不加 `label`
- **`label` 值格式必须为 `键=值`**（`key=value` 规范），用于标识值的归属维度：
  - 多核 CPU：`"core=0"`、`"core=1"`、`"core=2"`
  - 多接口流量：`"if=eth0"`、`"if=eth1"`
  - 多温度传感器：`"sensor=inlet"`、`"sensor=outlet"`
  - 多磁盘：`"disk=0"`、`"disk=1"`
  - **禁止**使用纯名称（如 `"CPU0"`、`"eth0"`）或中文描述作为 label 值

### 可用 host function（已注册到 Rhai Engine）

| 函数 | 签名 | 说明 |
|------|------|------|
| `parse_json(s)` | `(字符串) -> map` | 把 JSON 字符串解析为 Rhai 对象 |
| `to_json(d)` | `(值) -> 字符串` | 把任意 Rhai 值序列化为 JSON 字符串 |
| `get_num(map, oid)` | `(map, 字符串) -> Option<数字>` | 从 OID 值 map 便捷取数字 |
| `get_num_str(map, oid)` | `(map, 字符串) -> Option<字符串>` | 从 OID 值 map 便捷取字符串 |
| `log_info(msg)` / `log_warn` / `log_error` | `(字符串) -> ()` | 日志输出（不影响结果） |

**禁止使用**未列出的函数。Rhai 是沙箱语言，无文件/网络/进程 API。

### OID 值 map 结构（parse_json 解析后的形态）

键是 OID 字符串，值是一个对象：
```rhai
let m = parse_json(oid_values_json);
// m[".1.3.6.1.2.1.1.3.0"] == {
//     oid_value_type: 2,        // 1=字符串, 2=数字
//     value_str: "",
//     value_num: 123456.0
// }
let n = get_num(m, ".1.3.6.1.2.1.1.3.0");  // Option<f64>
```

## 模板示例（sysUpTime 插件 — 单值场景，不加 label）

```rhai
fn prepare_oids() {
    `[{"oid":".1.3.6.1.2.1.1.3.0","method":"get"}]`
}

fn parse(oid_values_json) {
    let map = parse_json(oid_values_json);
    let raw = get_num(map, ".1.3.6.1.2.1.1.3.0");
    if raw.is_none() {
        return `[{"success":false,"errors":["sysUpTime OID 缺失"]}]`;
    }
    let seconds = raw.unwrap() / 100.0;
    `[{"success":true,"value":{"number":` + seconds + `}}]`
}
```

## 注意事项

- 数值计算注意精度：用 `f64`（Rhai 默认）
- 字符串拼接用模板：`` `${变量}` ``
- `Option` 类型用 `.is_none()` / `.unwrap()`（不要用 `match`）
- 不要在脚本里写注释以外的中文字符串字面量值（错误消息除外）
- 不要定义 `main` 函数，不要写模块声明
- **绝对不能跳过用户确认步骤直接生成代码或注册插件**"#
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_mentions_rhai_contract() {
        let s = get_system_prompt();
        assert!(s.contains("prepare_oids"));
        assert!(s.contains("fn parse(oid_values_json)"));
        assert!(s.contains("validate_monitor_plugin"));
        assert!(s.contains("register_monitor_plugin"));
        assert!(s.contains("get_num"));
        assert!(s.contains("阶段一"));
        assert!(s.contains("阶段二"));
        assert!(s.contains("阶段三"));
        assert!(s.contains("阶段四"));
        assert!(s.contains("还有需要补充或修改的吗"));
        assert!(s.contains("OID 实测或模拟"));
        assert!(s.contains("绝对不能跳过用户确认步骤"));
        assert!(s.contains("自然语言总结校验结果"));
        assert!(s.contains("自然语言告知用户最终结果"));
        assert!(s.contains("所有测试用例均已通过"));
        assert!(s.contains("label` 字段使用规则"));
        assert!(s.contains("单值场景（默认不加）"));
        assert!(s.contains("键=值"));
        assert!(s.contains("core=0"));
    }

    #[test]
    fn parse_snmp_oids_accepts_prepare_oids_batch() {
        let args = json!({
            "prepare_oids": [
                {"oid": ".1.3.6.1.2.1.1.3.0", "method": "snmpget", "label": "sysUpTime"},
                {"oid": ".1.3.6.1.2.1.2.2.1", "method": "SNMPWALK", "label": "ifTable"}
            ]
        });

        let oids = parse_snmp_oids(&args).unwrap();
        assert_eq!(oids.len(), 2);
        assert_eq!(oids[0].oid, ".1.3.6.1.2.1.1.3.0");
        assert_eq!(oids[0].method, "get");
        assert_eq!(oids[0].label.as_deref(), Some("sysUpTime"));
        assert_eq!(oids[1].oid, ".1.3.6.1.2.1.2.2.1");
        assert_eq!(oids[1].method, "walk");
    }

    #[test]
    fn merge_oid_values_accepts_json_string_and_object_content() {
        let groups = vec![
            json!({
                "snmp_type": "get",
                "content": "{\".1\":{\"oid_value_type\":2,\"value_str\":\"\",\"value_num\":42.0}}"
            }),
            json!({
                "snmp_type": "walk",
                "content": {
                    ".2.1": {"oid_value_type": 1, "value_str": "up", "value_num": 0.0}
                }
            }),
        ];

        let merged = try_merge_oid_values(&groups).unwrap();
        assert_eq!(merged[".1"]["value_num"], 42.0);
        assert_eq!(merged[".2.1"]["value_str"], "up");
    }

    #[test]
    fn tools_include_validate_and_register() {
        let cfg = crate::config::AppConfig {
            server: crate::config::ServerConfig {
                port: String::new(),
            },
            db: crate::config::DbConfig {
                db_type: "postgres".to_string(),
                host: String::new(),
                port: 5432,
                password: String::new(),
                user: String::new(),
                db: String::new(),
                connect_timeout: 10,
                statement_timeout: 30,
                pool_max_size: 10,
                pool_timeout: 5,
            },
            redis: crate::config::RedisConfig {
                host: String::new(),
                port: 6379,
                password: String::new(),
            },
            log: crate::config::LogConfig {
                debug: true,
                path: String::new(),
                level: "INFO".to_string(),
                otlp_enabled: false,
                otlp_endpoint: String::new(),
            },
            kb: Default::default(),
            context: Default::default(),
            agents: Default::default(),
            auth: Default::default(),
            skill: Default::default(),
            workspace: Default::default(),
            shell: Default::default(),
            mcp: Default::default(),
            assistant: Default::default(),
            object_storage: Default::default(),
            data_dir: "./data".to_string(),
        };

        // 无 PluginManager 且无 DB 时只有 validate 工具
        let tools = create_monitor_plugin_tools(&cfg, None, None, None);
        assert_eq!(tools.len(), 1);

        // 有 PluginManager 时有 validate + register 两个工具
        let pm = Arc::new(crate::domain::monitor::PluginManager::new());
        let tools = create_monitor_plugin_tools(&cfg, None, None, Some(pm));
        assert_eq!(tools.len(), 2);
    }
}
