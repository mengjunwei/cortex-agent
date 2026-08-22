//! 运行环境探测 —— 启动时一次性探明沙箱内「有哪些 runtime」，注入 system prompt。
//!
//! 缘由：agent 不知道运行环境里装了哪些 runtime（如 Python 是个虚拟环境、Node 装了），
//! 只能靠试错发现。本模块在启动时探一次「有哪些 runtime」，把结果缓存进 `MANIFEST`，
//! prompt 层注入后模型一上来就知道「这里有 python(venv) + node」。
//!
//! 刻意保持极简：只探「有没有 + 是否虚拟环境」，不探 pip 有无 / 全局包 / 具体库是否可 import
//! —— 那些让模型自己按需确认（`pip --version` / `require('x')` 试探）。对齐 codex「只给环境
//! 上下文、不过度枚举能力」的哲学，也避免注入过长污染缓存前缀。
//!
//! 设计：
//! - 进程级静态（装好的 runtime 不会在运行中变）→ `OnceLock` 缓存一次，永不重算。
//! - 每条命令 3s 超时（tokio），防偶发挂起拖垮启动。
//! - 探测在宿主进程跑；bubblewrap 沙箱只读 bind 宿主 `/usr`、`/usr/local/node` 等，
//!   故宿主探测结果 = 沙箱内可见的 runtime。
//! - 任一命令缺失/失败 → 该行省略（不阻塞流程）；全部失败 → manifest 为空（注入层跳过）。

use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

/// 单条探测命令的超时上限。`python --version` / `node --version` 正常 <1s。
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// 探测结果缓存。`init()` 在启动时写入一次；未写时 `manifest()` 返回空串（不阻塞注入）。
static MANIFEST: OnceLock<String> = OnceLock::new();

/// 启动时探测可用 runtime 并缓存。在 bootstrap（async runtime 内）调用一次。
///
/// 只探「有没有 + 是否虚拟环境」，能力细节（pip/全局包）让模型自己按需确认。
/// 失败静默：任一 runtime 缺失只省略对应行；网络/命令异常不 panic、不返回错误。
pub(crate) async fn init() {
    let mut lines: Vec<String> = Vec::new();

    // Python：优先 python3，回退 python。额外探是否虚拟环境。
    // 注意 or_else 的闭包不能 .await（非 async），故用显式 match 做回退。
    let py = match capture("python3", &["--version"]).await {
        Some(v) => Some(v),
        None => capture("python", &["--version"]).await,
    };
    if let Some(ver) = py {
        // venv/conda 环境下 sys.prefix != sys.base_prefix（python3.3+ 标准判据）。
        let label = venv_label(capture("python3", &["-c", PY_VENV_PROBE]).await.as_deref());
        let tail = if label.is_empty() {
            String::new()
        } else {
            format!(" ({label})")
        };
        lines.push(format!("- Python: {ver}{tail}"));
    }

    // Node：仅版本。全局包 / 具体库细节让模型自己按需确认（npm ls -g / require 试探）。
    if let Some(node_ver) = capture("node", &["--version"]).await {
        lines.push(format!("- Node.js: {node_ver}"));
    }

    let manifest = if lines.is_empty() {
        String::new()
    } else {
        format!("## Runtimes\n\n{}", lines.join("\n"))
    };
    // init 理论上只调一次；若重复调用，`set` 返回 Err 是正常的，忽略即可。
    let _ = MANIFEST.set(manifest);
}

/// 取已缓存的 runtime 清单（`init()` 未调用时返回 ""，调用方据此跳过注入）。
pub(crate) fn manifest() -> &'static str {
    MANIFEST.get().map(|s| s.as_str()).unwrap_or("")
}

/// 把 venv 探测脚本的 stdout 映射为注入标签。
///
/// - `venv`  → `virtual environment`（含 venv / virtualenv / conda）
/// - `system`→ `system`（非虚拟环境）
/// - `None`（命令失败，如极旧 python 无 base_prefix）/ 未知输出 → `""`（不加标签，让模型自验）
fn venv_label(probe_output: Option<&str>) -> &'static str {
    match probe_output.map(str::trim) {
        Some("venv") => "virtual environment",
        Some("system") => "system",
        _ => "",
    }
}

/// 运行一条命令，成功（exit 0）返回 stdout trim 文本；超时/失败/缺失 → None。
async fn capture(cmd: &str, args: &[&str]) -> Option<String> {
    let run = async {
        tokio::process::Command::new(cmd)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .output()
            .await
    };
    let out = tokio::time::timeout(PROBE_TIMEOUT, run).await.ok()?.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Python 虚拟环境探测脚本：venv/conda 环境下 sys.prefix != sys.base_prefix（python3.3+）。
const PY_VENV_PROBE: &str =
    "import sys; print('venv' if sys.prefix != sys.base_prefix else 'system')";

#[cfg(test)]
mod tests {
    use super::venv_label;

    #[test]
    fn venv_label_maps_probe_output() {
        assert_eq!(venv_label(Some("venv")), "virtual environment");
        assert_eq!(venv_label(Some("venv\n")), "virtual environment"); // trim 容忍换行
        assert_eq!(venv_label(Some("system")), "system");
        assert_eq!(venv_label(None), "", "命令失败 → 不加标签");
        assert_eq!(venv_label(Some("garbage")), "", "未知输出 → 不加标签");
    }
}
