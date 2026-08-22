//! Shell 命令安全分级 —— 将命令分为三档，供自动执行 / 拦截 / 请求用户确认决策。
//!
//! - [`Safety::Allowed`]：命中白名单（只读命令、受控脚本执行等），可自动执行
//! - [`Safety::Dangerous`]：命中危险命令检测（复用自 `run_command` 的逻辑），自动拦截
//! - [`Safety::NeedsPrompt`]：其余命令，需用户确认后再执行
//!
//! 分级流程：[`classify`] 先按 shell 操作符（`&&`、`||`、`;`、` | `）拆分为子命令，
//! 逐个用 [`classify_single`] 分级后合并（`Dangerous` > `NeedsPrompt` > `Allowed`）。
//!
//! > 与 [`crate::tools::redact`] 一样，这是 **best-effort 提示层**，不是安全边界：
//! > 不处理引号 / 转义 / 变量，substring 匹配必然可被绕过。真正防护依靠用户确认机制。

/// 命令安全等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Safety {
    /// 白名单命中，可自动执行
    Allowed,
    /// 危险命令命中，自动拦截
    Dangerous,
    /// 其余命令，需用户确认
    NeedsPrompt,
}

/// 永远安全的只读命令
const ALWAYS_SAFE: &[&str] = &[
    "ls",
    "cat",
    "grep",
    "head",
    "tail",
    "wc",
    "echo",
    "pwd",
    "cd",
    "find",
    "stat",
    "which",
    "whoami",
    "env",
    "date",
    "dir",
    "type",
    "tree",
    // Windows / PowerShell 原生命令
    "get-childitem",
    "gci",
    "select-string",
    "sls",
    "get-content",
    "gc",
    "write-output",
    "write-host",
    "get-location",
    "gl",
    "get-item",
    "gi",
    "test-path",
    "resolve-path",
    "get-process",
    "get-service",
    "where",
    "where.exe",
    "more",
    "sort",
    "clip",
    // PowerShell 管道 cmdlet (只读操作)
    "select-object",
    "where-object",
    "foreach-object",
    "sort-object",
    "format-table",
    "format-list",
    "format-wide",
    "group-object",
    "measure-object",
    "compare-object",
    "tee-object",
    "out-string",
    "out-null",
    "convertto-json",
    "convertfrom-json",
];

/// 脚本执行类命令（python/node/bash/sh/go/rustc 等，npm/cargo 由专用函数单独判定，不在此列）。
/// 注意：命中 SCRIPTS 后**不直接放行**，由 [`interpreter_safety`] 细分——内联代码（`-c`/`-e`）
/// 判 `NeedsPrompt`（载荷不可静态检查），跑脚本文件才 `Allowed`。对齐 codex exec_policy。
const SCRIPTS: &[&str] = &[
    "python", "python3", "py", "node", "npx", "rustc", "go", "ruby", "perl", "bash", "sh",
];

/// 文本处理工具
const TEXT_TOOLS: &[&str] = &[
    "sed", "awk", "tr", "cut", "sort", "uniq", "comm", "diff", "column", "jq", "yq",
];

/// 文件 / 系统信息查看工具
const FILE_INSPECT: &[&str] = &[
    "file", "du", "df", "lsblk", "mount", "uname", "hostname", "ipconfig",
];

/// 对一条（可能复合的）命令进行安全分级（假定命令将在 OS 沙箱内执行）。
///
/// 按 `&&`、`||`、`;`、` | ` 拆分为子命令逐个分级后合并：
/// 任一子命令 `Dangerous` → 整条 `Dangerous`；任一 `NeedsPrompt`（且无 `Dangerous`）→
/// `NeedsPrompt`；全部 `Allowed` → `Allowed`。空命令按更安全的默认返回 `Dangerous`。
///
/// 生产路径用 [`classify_with_sandbox`] 显式传沙箱态；本包装仅供测试断言「沙箱内」基线。
#[cfg_attr(not(test), allow(dead_code))]
pub fn classify(command: &str) -> Safety {
    classify_with_sandbox(command, true)
}

/// 同 [`classify`]，但显式给出「命令是否将在 OS 沙箱内执行」。
///
/// git/npm/cargo 只读子命令白名单**仅沙箱内成立**（`sandboxed=true`）：仓库配置
/// （`.git/config` 的 `core.pager` / `diff.*.command` / fsmonitor 等）可让只读子命令
/// 执行任意 helper——沙箱内 helper 只能以沙箱权限跑（与 `python x.py` 同级）；无沙箱
/// 裸跑（DangerFullAccess / 无 enforcer 平台）时该前提不成立 → 降级 `NeedsPrompt`
/// （对齐 codex 3b45c29「git 参数不足以建立信任」的结论，保留沙箱内的 UX）。
/// 纯只读命令（cat/ls）无 helper 执行面，不受此门控影响。
pub fn classify_with_sandbox(command: &str, sandboxed: bool) -> Safety {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Safety::Dangerous;
    }

    let mut result = Safety::Allowed;
    let mut seen = false;
    for sub in split_compound(trimmed) {
        let part = sub.trim();
        if part.is_empty() {
            continue;
        }
        seen = true;
        result = merge(result, classify_single(part, sandboxed));
        if matches!(result, Safety::Dangerous) {
            return Safety::Dangerous;
        }
    }

    if seen { result } else { Safety::Dangerous }
}

/// 命令文本含未受单引号保护的命令替换 / 进程替换展开（`$(...)`、`` `...` ``、`<(...)`）。
///
/// 近似判据，刻意过严而非漏判：首词白名单不可信的前提是参数静态可见，而展开标记意味着
/// 实际执行的命令要等 shell 求值后才知道——`cat $(evil)` 的首词是 `cat`，跑的却是 evil。
/// 引号语义（POSIX）：单引号内一切字面（豁免）；双引号内 `$()`/反引号**仍展开**（命中，
/// 正确）；裸 token 展开（命中）。PowerShell 的 `$( )` 子表达式同样展开；反引号在
/// PowerShell 是转义符非命令替换，故反引号判定仅限 Unix。`\` 转义下一字符仅按 POSIX
/// 处理（Windows 路径反斜杠不是转义）。已知不精确点（`$'...'` ANSI-C 引号、引号奇偶
/// 错乱文本）偏向误杀而非漏杀。
fn has_shell_expansion(cmd: &str) -> bool {
    let unix = !cfg!(target_os = "windows");
    let b = cmd.as_bytes();
    let (mut in_single, mut in_double) = (false, false);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            // 单引号内反斜杠是字面量；单/双引号外（POSIX）转义下一字符
            b'\\' if in_single => {}
            b'\\' if unix => i += 1,
            b'$' if !in_single && b.get(i + 1) == Some(&b'(') => return true,
            b'<' if !in_single && b.get(i + 1) == Some(&b'(') => return true,
            b'`' if !in_single && unix => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// 对单个子命令分级：先判危险，再查白名单，其余 `NeedsPrompt`。
/// `sandboxed` 语义见 [`classify_with_sandbox`]（git/npm/cargo 白名单的门控开关）。
fn classify_single(cmd: &str, sandboxed: bool) -> Safety {
    if is_dangerous_command(cmd) {
        return Safety::Dangerous;
    }

    // 命令替换/进程替换不可静态求值：首词白名单不可信，整条降级审批
    // （`cat $(evil)` 不能因首词 cat 而放行；对齐 codex 对动态 word 的审批姿态）。
    if has_shell_expansion(cmd) {
        return Safety::NeedsPrompt;
    }

    let Some(first) = cmd.split_whitespace().next() else {
        return Safety::NeedsPrompt;
    };
    let lower = first.to_ascii_lowercase();
    let name = lower.rsplit(['/', '\\']).next().unwrap_or(&lower[..]);

    if ALWAYS_SAFE.contains(&name) {
        return Safety::Allowed;
    }

    match name {
        // 白名单前提 = 沙箱兜底（见 classify_with_sandbox 文档）；无沙箱落 NeedsPrompt
        "git" if sandboxed => return git_subcommand_safety(cmd),
        "npm" if sandboxed => return npm_subcommand_safety(cmd),
        "cargo" if sandboxed => return cargo_subcommand_safety(cmd),
        _ => {}
    }

    if SCRIPTS.contains(&name) {
        // 解释器类：内联代码(-c/-e)必审批、跑文件放行（见 interpreter_safety）。
        // 对齐 codex exec_policy：复杂脚本永不 auto-approve，`bash/python -c` 显式禁。
        return interpreter_safety(cmd);
    }
    if TEXT_TOOLS.contains(&name) || FILE_INSPECT.contains(&name) {
        return Safety::Allowed;
    }

    Safety::NeedsPrompt
}

/// 解释器类命令（python/node/bash/sh/perl/ruby/go/rustc 等）的安全判定。
///
/// 核心意图：「**内联代码必审批、跑脚本文件放行**」——内联载荷不可静态检查，最易藏恶意代码
/// （`python -c "import os;os.system(...)"`、`bash -c "rm -rf /"`、`node --eval "…"`）；
/// 跑脚本文件（`python x.py`）内容由模型写入工作区、用户可见，纵深防御靠 OS 沙箱
/// （默认关网、收窄 `$HOME` 读）。对齐 codex exec_policy：复杂脚本永不 auto-approve。
///
/// 实现要点（best-effort，非 AST 解析——树级判断由 codex 的 tree-sitter 完成，本层只挡直白形式）：
/// - **引号剥离**后看 argv，防 `python "-c" "…"` 绕过（shell 语义里引号会被 shell 剥掉）。
/// - 以**首个非选项参数（脚本文件/模块名）**为界：其**之前**的是解释器标志（`-c/-e/-i/-m/--eval/…`
///   → 内联，NeedsPrompt）；其**之后**的是脚本自有参数（`train.py -c cfg`、`manage.py runserver -e prod`
///   → 放行，不误判）。`-m` 模块执行不可静态检查，一律判内联。
/// - 显式拦截长选项 `--eval/--print/--execute/--command/--interactive` 等。
fn interpreter_safety(cmd: &str) -> Safety {
    let mut toks = cmd.split_whitespace();
    let prog = toks.next().unwrap_or(""); // 程序名（用于区分 -E/-e 语义）
    let prog_name = command_name(prog);
    let mut saw_inline = false; // 解释器级内联/模块执行标志（脚本文件名之前出现即算）
    for t in toks {
        let t = t.trim_matches(['\'', '"']); // 剥引号，防 `python "-c" ...` 绕过
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        // 独立的内联代码标志：-c / -e（node 内联）/ -i（交互）/ -（stdin）。
        // `-E` 大小写敏感：python/ruby 的 `-E` 是「忽略环境变量/编码」（合法），node 的 `-e` 是内联执行。
        // lower 后是同一个 "-e"，按解释器名区分——python/ruby/perl 无内联 -e，放行其 `-E`；node 的 -e 仍拦。
        let is_python_like = prog_name.starts_with("python")
            || prog_name.starts_with("ruby")
            || prog_name.starts_with("perl");
        if matches!(lower.as_str(), "-c" | "-i" | "-") || (lower == "-e" && !is_python_like) {
            saw_inline = true;
            continue;
        }
        // -m 模块执行（`python -m pip ...`）：不可静态检查，按内联对待
        if lower == "-m" || lower.starts_with("-m") && !lower.starts_with("--") {
            saw_inline = true;
            continue;
        }
        // 长选项内联：node --eval/--print、perl/ruby --execute、--interactive 等
        if lower.starts_with("--eval")
            || lower.starts_with("--print")
            || lower.starts_with("--execute")
            || lower.starts_with("--command")
            || lower.starts_with("--interactive")
        {
            saw_inline = true;
            continue;
        }
        // 粘连内联短选项：-c'code' / -c"code" / -c=code（非长选项），含组合短选项如 -Oc / -uc。
        // 剥离首个 '-' 后按单字符扫描，任一字符命中 c/e 即视为内联执行——getopt 会把 `-Oc` 拆成
        // `-O -c`，只看 starts_with("-c") 会让 `python -Oc 'x'` 绕过。长选项（--）不进入此分支。
        if lower.starts_with('-') && !lower.starts_with("--") {
            let shorts = &lower[1..];
            // 纯 ASCII 字母组合（getopt 组合短选项形态）：逐字符判内联。
            // len==1 时只认 `c`（python/bash/sh 内联执行）。`-e` 单独出现时已被上方独立标志分支
            // (`-e`/`-c`/`-i`/`-`) 拦截，不到这里——此处 shorts.len()==1 且为 'e' 的是 python/ruby
            // 的 `-E`（忽略环境变量/编码标志，合法常用），放过免误伤。组合里含 c/e 仍拦（-Oc/-uc/-pe）。
            if shorts.chars().all(|c| c.is_ascii_alphabetic())
                && (shorts.contains('c') || (shorts.len() > 1 && shorts.contains('e')))
            {
                saw_inline = true;
                continue;
            }
            // 以 c/e 开头后接引号/等号/代码（-c'x'、-e=…、-c import os）
            if (shorts.starts_with('c') || shorts.starts_with('e')) && shorts.len() > 1 {
                saw_inline = true;
                continue;
            }
            // 其他解释器选项（python -O / -B / -u / -E、node --max-old-space-size=…）：跳过看脚本名
            continue;
        }
        // 首个非选项参数 = 脚本文件/模块名/命令 → 其后均为脚本自有参数，不再判内联
        break;
    }
    if saw_inline {
        Safety::NeedsPrompt
    } else {
        Safety::Allowed
    }
}

/// 判断命令是否为纯只读（整条命令、含复合，每个子命令首词都命中 [`ALWAYS_SAFE`]）。
///
/// Windows 无 OS 沙盒兜底时，仅纯只读命令可自动执行；其余 Allowed 命令（解释器/构建/文本
/// 工具等可执行任意代码或有副作用）一律走审批——与 codex 在无沙盒平台的安全姿态一致。
///
/// 必须逐子命令判定：只看整条命令首词会让 `ls && python evil.py` 这类复合命令被误判为只读，
/// 在 Windows 自动执行后半段的危险子命令。逐个子命令首词都命中只读白名单才放行，任一不命中
/// 即整体降级为需审批。
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn is_pure_readonly(cmd: &str) -> bool {
    split_compound(cmd).iter().all(|sub| {
        // 含展开的子命令不是纯只读：实际命令要等 shell 求值，首词白名单不可信
        if has_shell_expansion(sub) {
            return false;
        }
        let Some(first) = sub.split_whitespace().next() else {
            return false; // 空子命令（如连续分隔符）不算只读，保守降级
        };
        let name = command_name(first);
        ALWAYS_SAFE.iter().any(|s| name.eq_ignore_ascii_case(s))
    })
}

/// 提取命令名：剥离路径前缀取尾段（`C:\bin\py.exe` / `/usr/bin/python3` → `py.exe` / `python3`）。
///
/// 按借用返回输入的子串（零分配、不 lowercase）。供 [`is_pure_readonly`] 与 [`classify_single`]
/// 共用，避免两处各自实现首词提取导致判定分叉。
fn command_name(first_token: &str) -> &str {
    first_token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(first_token)
}

/// git 只读子命令 → `Allowed`，其余 → `NeedsPrompt`
fn git_subcommand_safety(cmd: &str) -> Safety {
    const SAFE: &[&str] = &[
        "status",
        "log",
        "diff",
        "show",
        "branch",
        "remote",
        "blame",
        "ls-files",
        "rev-parse",
    ];
    subcommand_in(cmd, SAFE)
}

/// npm 受控子命令（run/test/exec/ls/list）→ `Allowed`，其余（含 install）→ `NeedsPrompt`
fn npm_subcommand_safety(cmd: &str) -> Safety {
    const SAFE: &[&str] = &["run", "test", "exec", "ls", "list"];
    subcommand_in(cmd, SAFE)
}

/// cargo 构建类子命令 → `Allowed`，其余 → `NeedsPrompt`
fn cargo_subcommand_safety(cmd: &str) -> Safety {
    const SAFE: &[&str] = &["check", "test", "build", "clippy", "fmt", "doc", "run"];
    subcommand_in(cmd, SAFE)
}

/// 取命令第二个 token（小写），命中白名单 → `Allowed`，否则 → `NeedsPrompt`
fn subcommand_in(cmd: &str, safe: &[&str]) -> Safety {
    let Some(sub) = cmd.split_whitespace().nth(1) else {
        return Safety::NeedsPrompt;
    };
    let sub = sub.to_ascii_lowercase();
    if safe.contains(&sub.as_str()) {
        Safety::Allowed
    } else {
        Safety::NeedsPrompt
    }
}

/// 合并两个分级结果：`Dangerous` > `NeedsPrompt` > `Allowed`
fn merge(a: Safety, b: Safety) -> Safety {
    match (a, b) {
        (Safety::Dangerous, _) | (_, Safety::Dangerous) => Safety::Dangerous,
        (Safety::NeedsPrompt, _) | (_, Safety::NeedsPrompt) => Safety::NeedsPrompt,
        (Safety::Allowed, Safety::Allowed) => Safety::Allowed,
    }
}

/// 按 shell 操作符（`&&`、`||`、`;`、` | `）拆分命令，返回各子命令切片。
///
/// best-effort：不处理引号 / 转义。按字节匹配 ASCII 分隔符，仅在字符边界切片（UTF-8 安全）。
fn split_compound(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let len = delim_len_at(bytes, i);
        if len > 0 {
            parts.push(&s[start..i]);
            i += len;
            start = i;
        } else {
            i += 1;
        }
    }
    parts.push(&s[start..]);
    parts
}

/// 返回位置 `i` 处的分隔符长度（0 表示非分隔符）。
fn delim_len_at(bytes: &[u8], i: usize) -> usize {
    match bytes[i] {
        b';' => 1,
        b'&' if i + 1 < bytes.len() && bytes[i + 1] == b'&' => 2,
        b'|' if i + 1 < bytes.len() && bytes[i + 1] == b'|' => 2,
        b' ' if i + 2 < bytes.len() && bytes[i + 1] == b'|' && bytes[i + 2] == b' ' => 3,
        _ => 0,
    }
}

/// 危险命令检测（best-effort 提示性检查）——两级黑名单合并判定。
///
/// ⚠️ **安全说明**：这是一个**尽力而为的提示层**，不是真正的安全边界。
/// substring 匹配必然可被绕过（双空格、引号拼接、变量、base64 eval 等）。
/// 真正的防护依靠：
/// 1. `TOOL_CONFIRMATION` 机制（run_command 应标记为需用户确认，见设计 §10）
/// 2. 工作区是用户自己的仓库（用户对内容负责）
///
/// 此函数的价值在于拦截 LLM 误生成的"明显危险"命令（如 `rm -rf /`），
/// 而非防御恶意构造。维护者请勿将其视为安全保证。
///
/// 分级（供 Allow 规则豁免边界用，见 [`is_catastrophic_command`]）：
/// - **灾难级** [`CATASTROPHIC`]：破坏宿主系统/数据形态，不可被用户 Allow 规则豁免；
/// - **工具级** [`TOOL_RISKY`]：网络外泄/提权/进程杀戮，用户可经 Allow 规则显式豁免
///   （`shell_command` 在查规则前只硬拦灾难级）。
///
/// 检测策略：先归一化（压缩多空格为单空格），再做 token 级匹配，
/// 抵抗 `rm  -rf /`、`rm -fr /` 等简单变体。
fn is_dangerous_command(cmd: &str) -> bool {
    is_catastrophic_command(cmd)
        || {
            let normalized = normalize_command(cmd);
            TOOL_RISKY.iter().any(|b| normalized.contains(b))
        }
}

/// 灾难级黑名单：破坏宿主系统/数据的命令形态。命中即整体拦截，
/// **不可被用户 Allow 规则豁免**（无「用户已信任 rm -rf /」的合理场景）。
const CATASTROPHIC: &[&str] = &[
    // 破坏性删除（含 -rf/-fr 两种顺序）
    "rm -rf /",
    "rm -fr /",
    "rm -rf ~",
    "rm -fr ~",
    "rm -rf /*",
    "rm -rf $home",
    "rm -rf c:\\",
    // 磁盘/系统
    "mkfs",
    "dd if=",
    ":(){:|:&};:",
    // 权限提升中的根级全开
    "chmod -r 777 /",
    // 关机重启
    "shutdown",
    "reboot",
    "halt",
    "poweroff",
];

/// 工具级黑名单：提权/网络外泄/进程杀戮——有正当使用场景，
/// 用户可经 Allow 规则显式豁免（如「允许 agent 用 curl 下载依赖」）。
const TOOL_RISKY: &[&str] = &[
    // 权限提升（沙箱内不应需要）
    "sudo ",
    "chown -r ",
    // 网络外泄（默认拒绝防止数据外传；如需可放开）
    "curl ",
    "wget ",
    "nc ",
    "netcat",
    // 进程杀戮
    "kill -9 -1",
    "killall",
    "pkill",
];

/// 灾难级命令判定（[`CATASTROPHIC`] 子串 + rm 根路径 token 级检测）。
///
/// 供 `shell_command` 在用户 Allow 规则**之前**硬拦：规则可豁免审批提示
/// （NeedsPrompt 层），但不得豁免灾难拦截。
pub fn is_catastrophic_command(cmd: &str) -> bool {
    let normalized = normalize_command(cmd);
    if CATASTROPHIC.iter().any(|b| normalized.contains(b)) {
        return true;
    }

    // token 级检测：找到 rm，收集其后所有 flags（以 - 开头）与 paths（不以 - 开头），
    // 若 flags 合并含 r+f 且 paths 中有根路径标记 → 拦截。
    // 抵抗 "rm -rfv /"、"rm -rf --one-file-system /" 等变体
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    for (i, t) in tokens.iter().enumerate() {
        if *t == "rm" || t.ends_with("/rm") {
            let mut flags = String::new();
            let mut root_like = false;
            for tok in tokens[i + 1..].iter() {
                if tok.starts_with('-') {
                    flags.push_str(tok);
                } else if matches!(tok, &"/" | &"/*" | &"~" | &"$home" | &"c:\\" | &"c:/") {
                    root_like = true;
                }
            }
            if flags.contains('r') && flags.contains('f') && root_like {
                return true;
            }
        }
    }

    false
}

/// 归一化：转小写 + 把连续空白压成单空格，抵抗 "rm  -rf /" / "rm\t-rf /"
fn normalize_command(cmd: &str) -> String {
    let lower = cmd.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut prev_ws = false;
    for ch in lower.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

/// 检测命令是否向 `forbidden_root`（含子目录）写入。命中返回写入目标描述。
///
/// **best-effort 提示层**（同 [`classify`]，非安全边界）：提取 shell 写入目标
/// （重定向 / cp / mv / install / rsync / scp / tee / mkdir / sed -i / dd of= /
/// python `open(..,'w')` 等），任一目标落在 forbidden_root 下即命中。
/// 旨在把模型从"改 skill 源码"引导回"用 edit_file 在工作区改 / 告知用户"——
/// 非对抗性检查；真正硬隔离由沙箱只读 bind 保证。
pub fn detect_write_into(cmd: &str, forbidden_root: &std::path::Path) -> Option<String> {
    let fr = forbidden_root.to_string_lossy();

    // python 写入启发：显式写方法(.write/write_text/write_bytes/writelines)或 open()+写模式。
    // 纯读 open(...).read() 不计,避免误拦"用 python 读 skill"。
    let py_write = cmd.contains("write_text")
        || cmd.contains("write_bytes")
        || cmd.contains(".write(")
        || cmd.contains(".writelines(")
        || (cmd.contains("open(") && write_mode_present(cmd));
    if py_write && cmd.contains(&*fr) {
        return Some(format!("(python 写入提及 {})", fr));
    }

    // 提取 shell 写入目标 token，逐个比对是否落在 forbidden_root 下
    for raw in extract_write_targets(cmd) {
        let t = raw.trim_matches(|c| c == '\'' || c == '"' || c == ' ');
        if t.starts_with('/') && std::path::Path::new(t).starts_with(forbidden_root) {
            return Some(t.to_string());
        }
    }
    None
}

/// 从命令文本抽取候选"写入目标" token（best-effort，不处理复杂引号/变量）。
fn extract_write_targets(cmd: &str) -> Vec<String> {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        // 独立重定向 > / >> / 2> / 1> / &> / 2>> / 1>> / &>>(下一个 token 是目标)
        if matches!(t, ">" | ">>" | "2>" | "1>" | "&>" | "2>>" | "1>>" | "&>>") {
            if let Some(nt) = toks.get(i + 1) {
                if !nt.starts_with('&') {
                    out.push((*nt).to_string()); // 跳过 >&fd 形式(如 2> &1)
                }
            }
            i += 2;
            continue;
        }
        // 粘连重定向：>foo / >>foo / 2>foo / 1>foo / &>foo / 2>>foo …（>&fd 跳过）
        // 长前缀优先,避免 "2>>" 被 "2>" 提前吃掉。
        for pre in ["&>>", "2>>", "1>>", "&>", "2>", "1>", ">>", ">"] {
            if let Some(rest) = t.strip_prefix(pre) {
                if !rest.is_empty() && !rest.starts_with('&') {
                    out.push(rest.to_string());
                }
                break;
            }
        }
        // dd of=PATH
        if let Some(rest) = t.strip_prefix("of=") {
            out.push(rest.to_string());
        }
        // 内联 tee：`... | tee PATH` —— 取其后第一个非选项 token
        if t == "tee" {
            for nt in &toks[i + 1..] {
                if !nt.starts_with('-') {
                    out.push((*nt).to_string());
                    break;
                }
            }
        }
        i += 1;
    }

    // 按子命令(拆 &&/||/;/|、跳过前导 VAR=/cd/export)的动词补目标:
    // cp/mv/install/rsync/scp → 子命令最后非选项 token;mkdir → 全部非选项;sed -i → 最后非选项。
    // 这样 `cd x && cp a <skill>/b`、`LD_LIBRARY_PATH=.. && mkdir <skill>/d` 也能识别。
    for sub in subcommands(cmd) {
        if let Some((verb, args)) = effective_verb(&sub) {
            let verb = verb.trim_end_matches(',');
            match verb {
                "cp" | "mv" | "install" | "rsync" | "scp" => {
                    if let Some(last) = last_non_option(args) {
                        out.push(last.to_string());
                    }
                }
                "mkdir" => {
                    for nt in non_options(args) {
                        out.push(nt.to_string());
                    }
                }
                "sed"
                    // sed -i ... FILE：带 -i 视为就地写,取最后的非选项 token
                    if args.iter().any(|x| x.starts_with("-i")) => {
                        if let Some(last) = last_non_option(args) {
                            out.push(last.to_string());
                        }
                    }
                _ => {}
            }
        }
    }
    out
}

/// 把命令按 shell 操作符(`&`/`|`/`;`)拆成子命令的 token 列表(忽略空段)。
/// `&&` 会因 split('&') 产生空段,被 filter_map 丢掉。
fn subcommands(cmd: &str) -> Vec<Vec<&str>> {
    cmd.split(['&', '|', ';'])
        .filter_map(|seg| {
            let toks: Vec<&str> = seg.split_whitespace().collect();
            if toks.is_empty() { None } else { Some(toks) }
        })
        .collect()
}

/// 取一个子命令的有效动词:跳过前导 `VAR=value` 环境赋值、`cd <dir>`、`export`。
/// 返回 (动词, 动词之后的参数 tokens)。
fn effective_verb<'a>(toks: &'a [&'a str]) -> Option<(&'a str, &'a [&'a str])> {
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if !t.starts_with('=') && t.contains('=') {
            i += 1; // VAR=value 前导赋值
        } else if t == "export" {
            i += 1;
        } else if t == "cd" {
            i += 2; // cd <dir> —— 跳过 cd 及其目标目录
        } else {
            return Some((t, &toks[i + 1..]));
        }
    }
    None
}

/// python 写入模式标记(用于把 open() 限定到写场景,减少对纯读的误拦)。
fn write_mode_present(cmd: &str) -> bool {
    [
        "'w'",
        "\"w\"",
        "'a'",
        "\"a\"",
        "'x'",
        "\"x\"",
        "mode='w'",
        "mode=\"w\"",
        "mode='a'",
        "mode=\"a\"",
        "mode='x'",
        "'wb'",
        "'ab'",
        "'xb'",
    ]
    .iter()
    .any(|m| cmd.contains(m))
}

fn non_options<'a>(args: &'a [&str]) -> Vec<&'a str> {
    args.iter()
        .copied()
        .filter(|a| !a.starts_with('-'))
        .collect()
}

fn last_non_option<'a>(args: &'a [&str]) -> Option<&'a str> {
    args.iter().copied().rev().find(|a| !a.starts_with('-'))
}

#[cfg(test)]
mod write_detect_tests {
    use super::detect_write_into;
    use std::path::Path;

    const ROOT: &str = "/data/skills";

    #[test]
    fn blocks_redirect_into_skill() {
        assert!(
            detect_write_into("cat > /data/skills/foo/scripts/x.py", Path::new(ROOT)).is_some()
        );
        assert!(detect_write_into("echo hi >> /data/skills/a/b", Path::new(ROOT)).is_some());
    }

    #[test]
    fn blocks_cp_mv_into_skill() {
        assert!(
            detect_write_into(
                "cp /tmp/a.py /data/skills/foo/scripts/a.py",
                Path::new(ROOT)
            )
            .is_some()
        );
        assert!(detect_write_into("mv /tmp/a /data/skills/foo", Path::new(ROOT)).is_some());
    }

    #[test]
    fn blocks_python_write_into_skill() {
        assert!(
            detect_write_into(
                "python3 -c \"open('/data/skills/foo/scripts/x.py','w')\"",
                Path::new(ROOT)
            )
            .is_some()
        );
    }

    #[test]
    fn allows_read_from_skill() {
        // 读 skill 不应被拦
        assert!(detect_write_into("cat /data/skills/foo/SKILL.md", Path::new(ROOT)).is_none());
        assert!(
            detect_write_into("grep bar /data/skills/foo/references/x.md", Path::new(ROOT))
                .is_none()
        );
        // 从 skill 读、写到 /tmp 也不拦
        assert!(detect_write_into("cp /data/skills/foo/x.py /tmp/y.py", Path::new(ROOT)).is_none());
        assert!(
            detect_write_into("cat /data/skills/foo/x.py > /tmp/y.py", Path::new(ROOT)).is_none()
        );
    }

    #[test]
    fn allows_writes_outside_skill() {
        assert!(detect_write_into("cat > /tmp/foo.txt", Path::new(ROOT)).is_none());
        assert!(detect_write_into("mkdir -p /tmp/work", Path::new(ROOT)).is_none());
    }

    #[test]
    fn blocks_compound_cp_with_prefix() {
        // F1: 带前缀(cd / export / VAR=)的复合命令里的 cp/mv/mkdir 也要拦
        assert!(detect_write_into("cd /tmp && cp a /data/skills/x/b", Path::new(ROOT)).is_some());
        assert!(
            detect_write_into(
                "LD_LIBRARY_PATH=/x && cp a /data/skills/x/b",
                Path::new(ROOT)
            )
            .is_some()
        );
        assert!(detect_write_into("export X=y; mv a /data/skills/x/b", Path::new(ROOT)).is_some());
        assert!(
            detect_write_into("cd /tmp && mkdir -p /data/skills/x/d", Path::new(ROOT)).is_some()
        );
    }

    #[test]
    fn blocks_numbered_fd_redirect() {
        // F2: 2> / 1> / &> 写向 skill(独立 token 与粘连两种)
        assert!(detect_write_into("foo 2> /data/skills/x/err", Path::new(ROOT)).is_some());
        assert!(detect_write_into("foo 1> /data/skills/x/o", Path::new(ROOT)).is_some());
        assert!(detect_write_into("foo 2>/data/skills/x/err", Path::new(ROOT)).is_some());
        // fd 重定向到非 skill 不拦;>&fd 也不拦
        assert!(detect_write_into("foo 2> /tmp/err", Path::new(ROOT)).is_none());
        assert!(detect_write_into("foo 2>&1", Path::new(ROOT)).is_none());
    }

    #[test]
    fn python_read_of_skill_not_blocked() {
        // F3: 纯读 open(...).read() 不应误拦;写模式仍拦
        assert!(
            detect_write_into(
                "python3 -c \"open('/data/skills/x/d').read()\"",
                Path::new(ROOT)
            )
            .is_none()
        );
        assert!(
            detect_write_into(
                "python3 -c \"open('/data/skills/x/d','w')\"",
                Path::new(ROOT)
            )
            .is_some()
        );
        assert!(
            detect_write_into(
                "python3 -c \"open('/data/skills/x/d').write_text('a')\"",
                Path::new(ROOT)
            )
            .is_some()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safelist_ls() {
        assert_eq!(classify("ls -la"), Safety::Allowed);
    }
    #[test]
    fn safelist_cat() {
        assert_eq!(classify("cat file.txt"), Safety::Allowed);
    }
    #[test]
    fn safelist_python() {
        assert_eq!(classify("python script.py"), Safety::Allowed);
    }
    #[test]
    fn safelist_python3() {
        assert_eq!(
            classify("python3 /path/to/script.py --arg val"),
            Safety::Allowed
        );
    }
    #[test]
    fn safelist_git_status() {
        assert_eq!(classify("git status"), Safety::Allowed);
    }
    #[test]
    fn safelist_cargo_build() {
        assert_eq!(classify("cargo build"), Safety::Allowed);
    }
    #[test]
    fn safelist_npm_run() {
        assert_eq!(classify("npm run dev"), Safety::Allowed);
    }
    #[test]
    fn dangerous_rm_rf() {
        assert_eq!(classify("rm -rf /"), Safety::Dangerous);
    }
    #[test]
    fn dangerous_sudo() {
        assert_eq!(classify("sudo apt install foo"), Safety::Dangerous);
    }
    #[test]
    fn dangerous_mkfs() {
        assert_eq!(classify("mkfs.ext4 /dev/sda"), Safety::Dangerous);
    }
    #[test]
    fn needs_prompt_pip() {
        assert_eq!(classify("pip install pandas"), Safety::NeedsPrompt);
    }
    #[test]
    fn needs_prompt_npm_install() {
        assert_eq!(classify("npm install express"), Safety::NeedsPrompt);
    }
    #[test]
    fn needs_prompt_unknown() {
        assert_eq!(classify("some-unknown-command --flag"), Safety::NeedsPrompt);
    }
    #[test]
    fn compound_dangerous() {
        assert_eq!(classify("ls && rm -rf /"), Safety::Dangerous);
    }
    #[test]
    fn compound_needs_prompt() {
        assert_eq!(classify("echo hi && pip install x"), Safety::NeedsPrompt);
    }
    #[test]
    fn compound_all_allowed() {
        assert_eq!(classify("ls && cat file.txt"), Safety::Allowed);
    }
    #[test]
    fn compound_pipe() {
        assert_eq!(classify("ls | grep foo"), Safety::Allowed);
    }

    #[test]
    fn pure_readonly_compound_bypass() {
        // 整条首词是只读（ls），但复合后半段是危险命令——必须整体判非只读，否则 Windows
        // 会把 `ls && python evil.py` 误当只读自动执行。第一轮 F2 修的是单条，此处补复合。
        assert!(!is_pure_readonly("ls && python evil.py"));
        assert!(!is_pure_readonly("cat a.txt; rm -rf /"));
        assert!(!is_pure_readonly("ls && pip install x"));
    }

    #[test]
    fn expansion_command_substitution_needs_prompt() {
        // 首词是白名单命令（cat/echo），但展开标记意味着实际命令不可静态求值
        assert_eq!(classify("cat $(whoami)"), Safety::NeedsPrompt);
        assert_eq!(classify("cat <(ps aux)"), Safety::NeedsPrompt);
        // 双引号内 $()/反引号仍展开（POSIX）——同样降级
        assert_eq!(classify("echo \"run: $(date)\""), Safety::NeedsPrompt);
        // 展开内含危险命令：Dangerous 优先级不降
        assert_eq!(classify("echo $(rm -rf /)"), Safety::Dangerous);
    }

    #[cfg(unix)]
    #[test]
    fn expansion_backtick_needs_prompt() {
        // 反引号是 Unix 命令替换（PowerShell 里是转义符，另行豁免）
        assert_eq!(classify("echo `date`"), Safety::NeedsPrompt);
        assert_eq!(classify("cat a.txt && echo `id`"), Safety::NeedsPrompt);
    }

    #[test]
    fn expansion_in_single_quotes_still_allowed() {
        // 单引号内是字面量，不展开——白名单命令保持放行
        assert_eq!(classify("echo '$(literal)'"), Safety::Allowed);
        // 双引号内的撇号不能翻转单引号态（否则 "it's $(x)" 会错位漏判/误判）
        assert_eq!(classify("grep \"it's ok\" file"), Safety::Allowed);
        // 裸 $VAR 引用不是命令替换，不误伤
        assert_eq!(classify("echo $HOME"), Safety::Allowed);
        assert_eq!(classify("cat $VIRTUAL_ENV/pyvenv.cfg"), Safety::Allowed);
    }

    #[test]
    fn pure_readonly_rejects_expansion() {
        // Windows 无沙箱自动执行路径：含展开的子命令绝不能判纯只读
        assert!(!is_pure_readonly("cat $(whoami)"));
        assert!(!is_pure_readonly("ls && echo $(date)"));
        assert!(is_pure_readonly("ls && cat a.txt"));
    }

    #[test]
    fn pure_readonly_all_readonly_compound() {
        // 每个子命令都只读 → 仍是纯只读
        assert!(is_pure_readonly("ls && cat file.txt"));
        assert!(is_pure_readonly("ls | grep foo"));
        assert!(is_pure_readonly("pwd"));
    }

    #[test]
    fn empty_command() {
        assert_eq!(classify(""), Safety::Dangerous);
    }

    // ---- B：两级黑名单（灾难级 vs 工具级） --------------------------------

    #[test]
    fn catastrophic_tier_matches() {
        // 灾难级：破坏宿主系统/数据形态
        assert!(is_catastrophic_command("rm -rf /"));
        assert!(is_catastrophic_command("rm  -fr /")); // 归一化压空白后命中
        assert!(is_catastrophic_command("rm -rfv /")); // token 级 r+f+root 检测
        assert!(is_catastrophic_command("mkfs.ext4 /dev/sda"));
        assert!(is_catastrophic_command("sudo shutdown now"));
        assert!(is_catastrophic_command("reboot"));
    }

    #[test]
    fn catastrophic_tier_rejects_tool_risky() {
        // 工具级不属灾难级（可被用户 Allow 规则豁免）
        assert!(!is_catastrophic_command("curl https://example.com"));
        assert!(!is_catastrophic_command("sudo apt install foo"));
        assert!(!is_catastrophic_command("pkill -f myapp"));
        assert!(!is_catastrophic_command("chown -R user:user /data"));
    }

    #[test]
    fn dangerous_covers_both_tiers() {
        // is_dangerous_command = 灾难级 ∪ 工具级（分级判定层两级都拦）
        assert!(is_dangerous_command("rm -rf /"));
        assert!(is_dangerous_command("mkfs.ext4 /dev/sda"));
        assert!(is_dangerous_command("curl https://example.com"));
        assert!(is_dangerous_command("sudo apt install foo"));
        assert!(!is_dangerous_command("ls -la"));
    }

    // ---- A′：git/npm/cargo 白名单沙箱门控 ----------------------------------

    #[test]
    fn vcs_whitelist_gated_off_without_sandbox() {
        // 无沙箱（DangerFullAccess / 无 enforcer 平台）：白名单前提不成立 → 审批
        assert_eq!(classify_with_sandbox("git status", false), Safety::NeedsPrompt);
        assert_eq!(classify_with_sandbox("cargo build", false), Safety::NeedsPrompt);
        assert_eq!(classify_with_sandbox("npm run test", false), Safety::NeedsPrompt);
        // 沙箱内：白名单恢复生效
        assert_eq!(classify_with_sandbox("git status", true), Safety::Allowed);
        assert_eq!(classify_with_sandbox("cargo build", true), Safety::Allowed);
    }

    #[test]
    fn vcs_whitelist_gate_spares_plain_readonly() {
        // 纯只读命令（无 helper 执行面）不受门控影响
        assert_eq!(classify_with_sandbox("ls -la", false), Safety::Allowed);
        assert_eq!(classify_with_sandbox("cat file.txt", false), Safety::Allowed);
        assert_eq!(classify_with_sandbox("python script.py", false), Safety::Allowed);
        // 门控只降级，不升级：非白名单子命令保持 NeedsPrompt
        assert_eq!(classify_with_sandbox("git push", false), Safety::NeedsPrompt);
        assert_eq!(classify_with_sandbox("git push", true), Safety::NeedsPrompt);
    }

    #[test]
    fn vcs_whitelist_gate_composes_across_subcommands() {
        // 复合命令逐子命令判定：无沙箱时任一 git/npm/cargo 子命令拉低整条
        assert_eq!(
            classify_with_sandbox("ls && git status", false),
            Safety::NeedsPrompt
        );
        assert_eq!(
            classify_with_sandbox("ls && git status", true),
            Safety::Allowed
        );
        // 危险命令不受 sandboxed 参数影响（恒 Dangerous）
        assert_eq!(
            classify_with_sandbox("git status && rm -rf /", false),
            Safety::Dangerous
        );
    }
    #[test]
    fn safelist_with_path_prefix() {
        assert_eq!(classify("/usr/bin/python3 script.py"), Safety::Allowed);
    }
    #[test]
    fn safelist_node() {
        assert_eq!(classify("node app.js"), Safety::Allowed);
    }
    #[test]
    fn interpreter_inline_python_c_needs_prompt() {
        // 内联代码必审批（堵 `python -c "恶意"` 绕过）
        assert_eq!(classify("python -c 'print(1)'"), Safety::NeedsPrompt);
    }
    #[test]
    fn interpreter_inline_bash_c_needs_prompt() {
        // 无危险字面的内联脚本 → NeedsPrompt（内联载荷不可静态检查，须审批）
        assert_eq!(classify("bash -c 'echo hi'"), Safety::NeedsPrompt);
    }
    #[test]
    fn interpreter_inline_with_dangerous_literal_still_blocked() {
        // 内联带明显危险字面 → Dangerous 优先拦截（比审批更严格）
        assert_eq!(classify("bash -c 'rm -rf /'"), Safety::Dangerous);
    }
    #[test]
    fn interpreter_inline_node_e_needs_prompt() {
        assert_eq!(classify("node -e 'console.log(1)'"), Safety::NeedsPrompt);
    }
    #[test]
    fn interpreter_inline_python_c_glued_needs_prompt() {
        // 粘连形式 -c'code' 也要拦
        assert_eq!(classify("python -c'import os'"), Safety::NeedsPrompt);
    }
    #[test]
    fn interpreter_file_run_still_allowed() {
        // 跑脚本文件保持放行（载荷由模型写入、用户可见）
        assert_eq!(classify("python script.py"), Safety::Allowed);
        assert_eq!(classify("python3 /abs/path/x.py --arg"), Safety::Allowed);
        assert_eq!(classify("go run main.go"), Safety::Allowed);
    }
    #[test]
    fn interpreter_script_own_args_not_inline() {
        // 脚本自有参数（文件名之后）不是解释器内联标志 → 不误判 NeedsPrompt
        assert_eq!(classify("python train.py -c config.yaml"), Safety::Allowed);
        assert_eq!(
            classify("python manage.py runserver -e prod"),
            Safety::Allowed
        );
        assert_eq!(classify("bash deploy.sh -c extra"), Safety::Allowed);
    }
    #[test]
    fn interpreter_long_option_eval_needs_prompt() {
        // 长选项内联执行（此前 `--` 一刀切放过）：node --eval / --print 必须拦
        assert_eq!(
            classify("node --eval 'require(\"child_process\")'"),
            Safety::NeedsPrompt
        );
        assert_eq!(classify("node --print 'process.env'"), Safety::NeedsPrompt);
    }
    #[test]
    fn interpreter_module_exec_needs_prompt() {
        // -m 模块执行不可静态检查 → 按内联对待
        assert_eq!(classify("python -m pip install evil"), Safety::NeedsPrompt);
    }
    #[test]
    fn interpreter_quoted_flag_still_blocked() {
        // 引号包裹的标志（shell 语义里引号会被剥掉）仍应识别为内联
        assert_eq!(classify("python \"-c\" \"import os\""), Safety::NeedsPrompt);
        assert_eq!(classify("bash '-c' 'id'"), Safety::NeedsPrompt);
    }
    #[test]
    fn interpreter_plain_flag_option_allowed() {
        // 纯解释器选项（非内联）跑文件仍放行
        assert_eq!(classify("python -u script.py"), Safety::Allowed);
        assert_eq!(classify("python -O script.py"), Safety::Allowed);
    }
    #[test]
    fn interpreter_combined_short_option_blocked() {
        // 组合短选项里夹带 c/e（getopt 会拆成 -O -c）：必须拦，否则 `python -Oc 'x'` 绕过
        assert_eq!(classify("python -Oc 'import os'"), Safety::NeedsPrompt);
        assert_eq!(classify("python -uc 'import os'"), Safety::NeedsPrompt);
        assert_eq!(classify("node -pe 'process.env'"), Safety::NeedsPrompt);
        // 不含 c/e 的组合短选项跑文件仍放行（-B、-O、-u 组合）
        assert_eq!(classify("python -Bu script.py"), Safety::Allowed);
        // 单独的 -E（忽略 PYTHON* 环境变量，合法常用）不误伤；独立 -e/-c 已被前面分支拦
        assert_eq!(classify("python -E script.py"), Safety::Allowed);
    }
}
