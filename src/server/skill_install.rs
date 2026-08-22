//! Skill 安装/删除 API — 从工作区路径或 tar.gz 上传安装,以及删除 user 级 skill。
//!
//! 沙箱内 skill 目录只读,agent/前端无法直接写,需后端代写:
//! - `POST /api/skills/install`:JSON body `{path, overwrite}`,从工作区绝对路径安装
//! - `POST /api/skills/upload`:multipart tar.gz 上传安装
//! - `POST /api/skills/delete`:JSON body `{name}`,删除 user 级 skill(仅管理员)
//!
//! 安装流程:校验源 → 解析 SKILL.md frontmatter 取 name → `is_valid_skill_name` 校验
//! → 复制到 `{skill_dir}/{name}/` → `SkillService::reload()` 热重载。
//!
//! 安全:name 仅允许 `[a-z0-9-]`(`is_valid_skill_name`,与 mention 正则一致),
//! 天然杜绝路径穿越;tar.gz 解包逐条目校验路径(拒绝绝对路径 / `..` / 链接条目)。
//! 删除按 catalog 中 scope 判定,内置(.builtin)拒绝。
//! 鉴权:安装/上传与 screenshots/upload 一致(auth 启用时强制登录);删除从严,
//! auth 启用时仅管理员。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, State},
    response::IntoResponse,
    routing::post,
};
use serde_json::json;

use crate::error::AppError;
use crate::server::AppState;
use crate::server::auth::OptionalAuthUser;
use crate::server::response::{self, code};
use crate::domain::skill::is_valid_skill_name;
use crate::domain::skill::loader::{parse_frontmatter, read_skill_file_text};

const SKILL_FILENAME: &str = "SKILL.md";
/// skill tar.gz 上传大小上限 50MB(skill 含 references/scripts 等资产,比图片宽松)
const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;
/// 解包后查找 SKILL.md 的最大递归深度(防恶意深层嵌套归档)
const MAX_FIND_DEPTH: u32 = 5;

/// 安装请求体(JSON)
#[derive(Debug, serde::Deserialize)]
struct InstallRequest {
    /// 源 skill 目录的绝对路径(须含 SKILL.md)
    path: String,
    /// 是否覆盖同名已存在的 skill
    #[serde(default)]
    overwrite: bool,
}

/// 删除请求体(JSON)
#[derive(Debug, serde::Deserialize)]
struct DeleteRequest {
    /// 待删 skill 的 name(catalog 中的名字,非路径)
    name: String,
}

/// Skill 安装/删除路由组(挂载到根路径,路由以 `/api/skills/` 开头)。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/skills/install", post(install_skill))
        .route(
            "/api/skills/upload",
            post(upload_skill).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/api/skills/delete", post(delete_skill))
}

/// `POST /api/skills/install` — 从工作区绝对路径安装 skill。
///
/// body `{path, overwrite}`。源目录须含 SKILL.md,name 从其 frontmatter 解析
/// (缺失回退目录名)。复制到 `{skill_dir}/{name}/`,再 reload。
async fn install_skill(
    State(state): State<Arc<AppState>>,
    OptionalAuthUser(opt_user): OptionalAuthUser,
    Json(req): Json<InstallRequest>,
) -> impl IntoResponse {
    if state.auth.is_some() && opt_user.is_none() {
        return Json(response::err(code::UNAUTHORIZED, "请先登录后再安装 Skill"));
    }
    let src = PathBuf::from(&req.path);
    if !src.is_absolute() {
        return Json(response::err(code::INVALID_PARAMS, "path 必须为绝对路径"));
    }
    if !src.is_dir() {
        return Json(response::err(
            code::INVALID_PARAMS,
            format!("源路径不存在或不是目录: {}", src.display()),
        ));
    }
    let skill_md = src.join(SKILL_FILENAME);
    if !skill_md.is_file() {
        return Json(response::err(
            code::INVALID_PARAMS,
            format!("源目录缺少 {SKILL_FILENAME}"),
        ));
    }
    do_install(&state, &src, &skill_md, req.overwrite)
}

/// `POST /api/skills/upload` — multipart tar.gz 上传安装 skill。
///
/// 字段:`file`(tar.gz,必填) + `overwrite`("true"/"1"/"yes",可选)。解包到临时目录,
/// 在其中查找 SKILL.md,取其所在目录为源,复制到 `{skill_dir}/{name}/` 再 reload。
async fn upload_skill(
    State(state): State<Arc<AppState>>,
    OptionalAuthUser(opt_user): OptionalAuthUser,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if state.auth.is_some() && opt_user.is_none() {
        return Json(response::err(code::UNAUTHORIZED, "请先登录后再上传 Skill"));
    }
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name = String::new();
    let mut overwrite = false;
    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name() {
            Some("file") => {
                file_name = field.file_name().unwrap_or("skill.tar.gz").to_string();
                match field.bytes().await {
                    Ok(b) => file_bytes = Some(b.to_vec()),
                    Err(e) => {
                        return Json(response::err(
                            code::INVALID_PARAMS,
                            format!("读取上传数据失败: {e}"),
                        ));
                    }
                }
            }
            Some("overwrite") => {
                let v = field.text().await.unwrap_or_default();
                let t = v.trim().to_ascii_lowercase();
                overwrite = matches!(t.as_str(), "true" | "1" | "yes");
            }
            _ => {}
        }
    }
    let bytes = match file_bytes {
        Some(b) => b,
        None => return Json(response::err(code::INVALID_PARAMS, "未找到上传字段 file")),
    };
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Json(response::err(
            code::INVALID_PARAMS,
            format!("文件超过 {}MB 限制", MAX_UPLOAD_BYTES / 1024 / 1024),
        ));
    }

    // 按文件名后缀自动识别格式,解包到临时目录
    let temp_dir = std::env::temp_dir().join(format!(
        "cortex-skill-upload-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7().simple()
    ));
    if let Err(e) = std::fs::create_dir_all(&temp_dir) {
        return Json(response::err(
            code::INTERNAL,
            format!("创建临时目录失败: {e}"),
        ));
    }
    let fname = file_name.to_lowercase();
    if let Err(e) = if fname.ends_with(".zip") {
        unpack_zip(&bytes, &temp_dir)
    } else if fname.ends_with(".tar.gz") || fname.ends_with(".tgz") {
        unpack_tar_gz(&bytes, &temp_dir)
    } else if fname.ends_with(".tar") || fname.ends_with(".tar.xz") {
        unpack_tar(&bytes, &temp_dir)
    } else {
        // 默认尝试 tar.gz(最常见),失败再试 zip
        unpack_tar_gz(&bytes, &temp_dir)
            .or_else(|_| unpack_zip(&bytes, &temp_dir))
            .or_else(|_| unpack_tar(&bytes, &temp_dir))
    } {
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Json(response::err(
            code::INVALID_PARAMS,
            format!("解包失败(支持 .tar.gz/.tgz/.zip/.tar): {e}"),
        ));
    }

    // 在临时目录中查找 SKILL.md
    let skill_md = match find_skill_md(&temp_dir, 0) {
        Some(p) => p,
        None => {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Json(response::err(
                code::INVALID_PARAMS,
                format!("压缩包内未找到 {SKILL_FILENAME}"),
            ));
        }
    };
    let src_dir = match skill_md.parent() {
        Some(d) => d,
        None => {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Json(response::err(
                code::INVALID_PARAMS,
                "无法定位 SKILL.md 所在目录",
            ));
        }
    };
    let result = do_install(&state, src_dir, &skill_md, overwrite);
    // 复制完成(读临时目录)后清理,无论成败(best-effort)
    let _ = std::fs::remove_dir_all(&temp_dir);
    result
}

/// `POST /api/skills/delete` — 删除一个 user 级 Skill(整目录 + 热重载)。
///
/// body `{name}`。多用户(auth 启用)模式下仅管理员可删(删除不可逆,且 skill 目录
/// 全局共享,对齐监控插件/Shell 规则的治理强度);单用户(no-auth)模式放行。
/// 内置 Skill(编译期嵌入)拒绝删除。成功后自动 reload,新会话即不注入该 skill。
async fn delete_skill(
    State(state): State<Arc<AppState>>,
    OptionalAuthUser(opt_user): OptionalAuthUser,
    headers: axum::http::HeaderMap,
    Json(req): Json<DeleteRequest>,
) -> impl IntoResponse {
    // API Token 认证的请求仅允许删除会话(全系统删除类守卫,见 graphql.rs
    // reject_api_token_delete)——skill 删除不可逆且全局共享,同受此限。
    // 判定与 graphql_handler 一致:Bearer 头成功认证 = API Token。
    let via_api_token = opt_user.is_some()
        && headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split_once(' '))
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"));
    // 删除不可逆且影响全局:多用户模式下要求管理员(install/upload 仅登录即可,
    // 删除从严);单机 no-auth 无身份概念,放行(保持既有行为)。
    // 权限拒绝也落审计(success=false),与 GraphQL 层守卫拒绝同样留痕的口径一致。
    let denied = if via_api_token {
        Some("API Token 认证仅支持删除会话,删除 Skill 请使用账号登录")
    } else if state.auth.is_some() && !opt_user.as_ref().is_some_and(|u| u.is_admin) {
        Some("仅管理员可删除 Skill")
    } else {
        None
    };
    let name = req.name.trim().to_string();
    if let Some(msg) = denied {
        record_delete_audit(&state, &opt_user, via_api_token, &name, false, &headers);
        return Json(response::err(code::BUSINESS, msg));
    }
    let Some(skill_service) = state.skill_service.clone() else {
        record_delete_audit(&state, &opt_user, via_api_token, &name, false, &headers);
        return Json(response::err(code::BUSINESS, "Skill 服务未初始化,无法删除"));
    };
    let result = delete_skill_from_catalog(&skill_service, &name);
    record_delete_audit(&state, &opt_user, via_api_token, &name, result.is_ok(), &headers);
    match result {
        Ok(removed_dir) => {
            tracing::info!("[skill-delete] 删除成功: {name} ({})", removed_dir.display());
            Json(response::ok(json!({ "name": name, "deleted": true })))
        }
        Err(DeleteSkillError::InvalidName(msg)) => {
            Json(response::err(code::INVALID_PARAMS, msg))
        }
        Err(DeleteSkillError::App(e)) => Json(response::from_app_error(&e)),
    }
}

/// 落 skill 删除审计(含被守卫拒绝的尝试;异步,失败仅丢日志)。
/// `source` 分流对齐 graphql_handler:Bearer 认证记 `api_token`,否则 `web`。
fn record_delete_audit(
    state: &AppState,
    opt_user: &Option<crate::domain::auth::AuthUser>,
    via_api_token: bool,
    name: &str,
    success: bool,
    headers: &axum::http::HeaderMap,
) {
    crate::domain::audit::spawn_record(
        state.audit_store.as_ref(),
        crate::domain::audit::AuditEntry {
            user_id: opt_user
                .as_ref()
                .map(|u| u.user_id.clone())
                .unwrap_or_default(),
            actor: opt_user.as_ref().map(|u| u.name.clone()).unwrap_or_default(),
            source: if via_api_token { "api_token" } else { "web" }.to_string(),
            operation: "skill_delete".to_string(),
            target_id: name.to_string(),
            success,
            detail: String::new(),
            ip: super::audit::client_ip(headers),
        },
    );
}

/// 公共安装流程:解析 name → 复制到 `{skill_dir}/{name}/` → reload。
///
/// 复用给 install(源=工作区目录)与 upload(源=临时解包目录)两条路径。
/// `skill_md` 为源 SKILL.md 绝对路径,`src_dir` 为其所在目录。
fn do_install(
    state: &AppState,
    src_dir: &Path,
    skill_md: &Path,
    overwrite: bool,
) -> Json<serde_json::Value> {
    let name = match extract_skill_name(skill_md) {
        Ok(n) => n,
        Err(msg) => return Json(response::err(code::INVALID_PARAMS, msg)),
    };
    let skill_service = match state.skill_service.as_ref() {
        Some(s) => s.clone(),
        None => return Json(response::err(code::INTERNAL, "Skill 服务未初始化,无法安装")),
    };
    let dest = skill_service.skill_dir().join(&name);
    match install_to_dest(src_dir, &dest, overwrite) {
        Ok(()) => match skill_service.reload() {
            Ok(()) => {
                tracing::info!("[skill-install] 安装成功: {name} <- {}", src_dir.display());
                Json(response::ok(json!({
                    "name": name,
                    "path": dest.to_string_lossy(),
                })))
            }
            Err(e) => Json(response::err(
                code::INTERNAL,
                format!("文件已复制但热重载失败: {e}"),
            )),
        },
        Err(e) => Json(response::from_app_error(&e)),
    }
}

/// 从 SKILL.md 解析并校验 skill name。
///
/// frontmatter 有效时取 `name`(trim),为空则回退 SKILL.md 所在目录名(对齐 loader 行为)。
/// 最终经 [`is_valid_skill_name`] 校验(仅 `[a-z0-9-]`,与 mention 正则一致),失败返回中文错误。
fn extract_skill_name(skill_md: &Path) -> Result<String, String> {
    let (content, _) =
        read_skill_file_text(skill_md).map_err(|e| format!("读取 {SKILL_FILENAME} 失败: {e}"))?;
    let fm = parse_frontmatter(&content).ok_or_else(|| {
        format!("{SKILL_FILENAME} 缺少有效 frontmatter(需 ---\\n...\\n---\\n 含 description)")
    })?;
    let raw = fm.name.trim();
    let name = if raw.is_empty() {
        skill_md
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.trim().to_string())
            .ok_or_else(|| "无法从目录名解析 skill name".to_string())?
    } else {
        raw.to_string()
    };
    if !is_valid_skill_name(&name) {
        return Err(format!(
            "skill name '{name}' 非法(需 ^[a-z0-9-]+$,1-64 字符,禁首尾/连续连字符)"
        ));
    }
    Ok(name)
}

/// 复制源 skill 目录到目标。目标已存在时按 `overwrite` 决定覆盖或报冲突。
fn install_to_dest(src: &Path, dest: &Path, overwrite: bool) -> Result<(), AppError> {
    if dest.exists() {
        if !overwrite {
            return Err(AppError::ConflictError(format!(
                "Skill 已存在: {}(可用 overwrite=true 覆盖)",
                dest.display()
            )));
        }
        std::fs::remove_dir_all(dest).map_err(|e| {
            AppError::FileError(format!("删除旧 Skill 失败 {}: {e}", dest.display()))
        })?;
    }
    copy_dir_recursive(src, dest)?;
    Ok(())
}

/// 递归复制目录(含文件)。目标父目录由调用方/递归内创建。
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<(), AppError> {
    std::fs::create_dir_all(dest)
        .map_err(|e| AppError::FileError(format!("创建目录失败 {}: {e}", dest.display())))?;
    let entries = std::fs::read_dir(src)
        .map_err(|e| AppError::FileError(format!("读取目录失败 {}: {e}", src.display())))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let target = dest.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|e| {
                AppError::FileError(format!(
                    "复制文件失败 {} -> {}: {e}",
                    path.display(),
                    target.display()
                ))
            })?;
        }
    }
    Ok(())
}

/// 解包 tar.gz 字节到目录。逐条目校验路径(拒绝对路径 / `..` / 链接),防 tar slipping。
fn unpack_tar_gz(bytes: &[u8], dir: &Path) -> Result<(), AppError> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    unpack_tar_archive(decoder, dir)
}

/// 解包纯 tar(无压缩)。
fn unpack_tar(bytes: &[u8], dir: &Path) -> Result<(), AppError> {
    unpack_tar_archive(bytes, dir)
}

/// tar 解包公共逻辑(逐条目校验)。
fn unpack_tar_archive<R: std::io::Read>(reader: R, dir: &Path) -> Result<(), AppError> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .map_err(|e| AppError::FileError(format!("读取 tar 条目失败: {e}")))?
    {
        let mut entry = entry.map_err(|e| AppError::FileError(format!("tar 条目读取失败: {e}")))?;
        let p = entry
            .path()
            .map_err(|e| AppError::FileError(format!("tar 路径解析失败: {e}")))?;
        if p.is_absolute()
            || p.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                )
            })
        {
            return Err(AppError::FileError(format!(
                "压缩包含不安全路径,拒绝解包: {}",
                p.display()
            )));
        }
        let et = entry.header().entry_type();
        if et.is_symlink() || et.is_hard_link() {
            return Err(AppError::FileError(format!(
                "压缩包含链接条目,拒绝解包: {}",
                p.display()
            )));
        }
        entry
            .unpack_in(dir)
            .map_err(|e| AppError::FileError(format!("tar 解包失败: {e}")))?;
    }
    Ok(())
}

/// 解包 zip 字节到目录。逐条目校验路径。
fn unpack_zip(bytes: &[u8], dir: &Path) -> Result<(), AppError> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| AppError::FileError(format!("读取 zip 失败: {e}")))?;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| AppError::FileError(format!("zip 条目 {i} 读取失败: {e}")))?;
        let name = file.name().to_string();
        let p = std::path::Path::new(&name);
        if p.is_absolute()
            || p.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                )
            })
        {
            return Err(AppError::FileError(format!(
                "zip 包含不安全路径,拒绝解包: {name}"
            )));
        }
        let target = dir.join(p);
        // 目录条目(以 / 结尾)
        if name.ends_with('/') {
            std::fs::create_dir_all(&target)
                .map_err(|e| AppError::FileError(format!("创建目录失败 {name}: {e}")))?;
            continue;
        }
        // 文件条目
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AppError::FileError(format!("创建父目录失败 {name}: {e}")))?;
        }
        let mut out = std::fs::File::create(&target)
            .map_err(|e| AppError::FileError(format!("创建文件失败 {name}: {e}")))?;
        std::io::copy(&mut file, &mut out)
            .map_err(|e| AppError::FileError(format!("写入文件失败 {name}: {e}")))?;
    }
    Ok(())
}

/// 在 `dir` 下递归查找首个 `SKILL.md`(深度受限,防恶意深层嵌套)。找到返回其路径。
fn find_skill_md(dir: &Path, depth: u32) -> Option<PathBuf> {
    if depth > MAX_FIND_DEPTH {
        return None;
    }
    let direct = dir.join(SKILL_FILENAME);
    if direct.is_file() {
        return Some(direct);
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(found) = find_skill_md(&p, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// 删除校验/执行中的可区分错误:非法 name 属参数错误(1001,对齐 install 的
/// extract_skill_name 口径),其余走 AppError 原映射(2001/2002/5001)。
#[derive(Debug)]
pub(crate) enum DeleteSkillError {
    /// name 非法(INVALID_PARAMS 1001)
    InvalidName(String),
    /// 其余业务/IO 错误(Business/NotFound/File…)
    App(AppError),
}

impl From<AppError> for DeleteSkillError {
    fn from(e: AppError) -> Self {
        DeleteSkillError::App(e)
    }
}

/// 删除一个 user 级 skill:校验 → 删目录 → 热重载。返回被删目录路径。
///
/// 供 `delete_skill` handler 调用;独立成函数便于单测(不带 AppState/鉴权)。
/// 规则:
/// - name 须过 [`is_valid_skill_name`](仅 `[a-z0-9-]`,天然拒绝 `.builtin` 与路径穿越)
/// - 仅 catalog 中实际存在、且 scope==User 的 skill 可删(Builtin 编译期嵌入,删了也会在
///   重启时自动恢复,直接拒绝)
/// - 删除目标取 `meta.path.parent()`(catalog 实际加载的目录),而非重新拼路径——
///   保证删的就是正在生效的那份
/// - 删完 `reload()`,让内存 catalog 立即生效(无需前端再点重新扫描)
pub(crate) fn delete_skill_from_catalog(
    skill_service: &crate::domain::skill::render::SkillService,
    name: &str,
) -> Result<PathBuf, DeleteSkillError> {
    if !is_valid_skill_name(name) {
        return Err(DeleteSkillError::InvalidName(format!(
            "skill name '{name}' 非法(需 ^[a-z0-9-]+$,1-64 字符,禁首尾/连续连字符)"
        )));
    }
    let meta = skill_service
        .find_by_name(name)
        .ok_or_else(|| AppError::NotFoundError(format!("Skill 不存在: {name}")))?;
    if meta.scope == crate::domain::skill::SkillScope::Builtin {
        return Err(AppError::BusinessError(format!(
            "内置 Skill 不允许删除: {name}"
        ))
        .into());
    }
    // meta.path 是 SKILL.md 绝对路径,其父目录即 skill 目录
    let skill_dir = meta
        .path
        .parent()
        .ok_or_else(|| {
            AppError::FileError(format!("无法定位 skill 目录: {}", meta.path.display()))
        })?
        .to_path_buf();
    if !skill_dir.is_dir() {
        // catalog 与磁盘不一致(如目录已被外部删除),按不存在处理
        return Err(AppError::NotFoundError(format!(
            "Skill 目录不存在: {}",
            skill_dir.display()
        ))
        .into());
    }
    std::fs::remove_dir_all(&skill_dir).map_err(|e| {
        AppError::FileError(format!("删除 Skill 目录失败 {}: {e}", skill_dir.display()))
    })?;
    skill_service.reload()?;
    Ok(skill_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use crate::domain::skill::SkillScope;
    use crate::domain::skill::render::SkillService;

    fn tmp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cortex-skill-install-test-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(dir: &Path, name: &str, desc: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let content = format!("---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\nbody");
        std::fs::write(dir.join(SKILL_FILENAME), content).unwrap();
    }

    #[test]
    fn extract_name_from_frontmatter() {
        let dir = tmp_dir("extract_fm");
        write_skill(&dir, "my-skill", "desc");
        let name = extract_skill_name(&dir.join(SKILL_FILENAME)).unwrap();
        assert_eq!(name, "my-skill");
    }

    #[test]
    fn extract_name_fallback_to_dirname() {
        let dir = tmp_dir("extract_fb").join("dir-skill");
        // frontmatter 无 name 字段 → 回退目录名
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILENAME), "---\ndescription: d\n---\n\nbody").unwrap();
        let name = extract_skill_name(&dir.join(SKILL_FILENAME)).unwrap();
        assert_eq!(name, "dir-skill");
    }

    #[test]
    fn extract_name_rejects_invalid() {
        let dir = tmp_dir("extract_bad").join("Bad_Name");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SKILL_FILENAME),
            "---\nname: Bad_Name\ndescription: d\n---\n\nbody",
        )
        .unwrap();
        assert!(extract_skill_name(&dir.join(SKILL_FILENAME)).is_err());
    }

    #[test]
    fn copy_dir_replicates_tree() {
        let src = tmp_dir("copy_src");
        write_skill(&src, "a", "d");
        std::fs::create_dir_all(src.join("references")).unwrap();
        std::fs::write(src.join("references").join("notes.md"), "notes").unwrap();
        let dest = tmp_dir("copy_dest");
        copy_dir_recursive(&src, &dest).unwrap();
        assert!(dest.join(SKILL_FILENAME).is_file());
        assert!(dest.join("references").join("notes.md").is_file());
    }

    #[test]
    fn install_to_dest_conflict_without_overwrite() {
        let src = tmp_dir("its_src");
        write_skill(&src, "a", "d");
        let dest = tmp_dir("its_dest");
        write_skill(&dest, "old", "old");
        let err = install_to_dest(&src, &dest, false).unwrap_err();
        assert!(matches!(err, AppError::ConflictError(_)));
    }

    #[test]
    fn install_to_dest_overwrites_when_requested() {
        let src = tmp_dir("its_src2");
        write_skill(&src, "a", "new");
        let dest = tmp_dir("its_dest2");
        write_skill(&dest, "old", "old");
        install_to_dest(&src, &dest, true).unwrap();
        // 覆盖后内容来自 src
        let text = std::fs::read_to_string(dest.join(SKILL_FILENAME)).unwrap();
        assert!(text.contains("new"));
        assert!(!text.contains("old"));
    }

    #[test]
    fn find_skill_md_locates_nested() {
        let root = tmp_dir("find");
        let nested = root.join("pkg").join("my-skill");
        write_skill(&nested, "my-skill", "d");
        let found = find_skill_md(&root, 0).unwrap();
        assert!(found.ends_with(SKILL_FILENAME));
    }

    #[test]
    fn unpack_tar_gz_extracts_valid_archive() {
        // 正常归档:tar.gz 含 my-skill/SKILL.md
        let dir = tmp_dir("unpack_ok");
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            let mut f = tar::Header::new_gnu();
            f.set_size("body".len() as u64);
            f.set_mode(0o644);
            f.set_cksum();
            builder
                .append_data(
                    &mut f,
                    "my-skill/SKILL.md",
                    &mut std::io::Cursor::new(&b"body"[..]),
                )
                .unwrap();
            builder.finish().unwrap();
        }
        let compressed = {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&buf).unwrap();
            enc.finish().unwrap()
        };
        unpack_tar_gz(&compressed, &dir).unwrap();
        assert!(dir.join("my-skill").join(SKILL_FILENAME).is_file());
    }

    #[test]
    fn unpack_tar_gz_rejects_symlink() {
        // 恶意归档:含 symlink 条目(指向 /etc/shadow)→ 拒绝,防越界链接攻击。
        // (tar builder 会拒绝 `..` 路径,故用 symlink 验证拒绝路径)
        let dir = tmp_dir("unpack_symlink");
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            let mut f = tar::Header::new_gnu();
            f.set_entry_type(tar::EntryType::Symlink);
            f.set_size(0);
            f.set_mode(0o777);
            f.set_link_name("/etc/shadow").unwrap();
            f.set_cksum();
            builder
                .append_data(&mut f, "evil", &mut std::io::Cursor::new(&b""[..]))
                .unwrap();
            builder.finish().unwrap();
        }
        let compressed = {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&buf).unwrap();
            enc.finish().unwrap()
        };
        let err = unpack_tar_gz(&compressed, &dir).unwrap_err();
        assert!(matches!(err, AppError::FileError(_)));
    }

    // ── delete_skill_from_catalog ──────────────────────

    /// 构造带 user skill + builtin skill 的 SkillService。
    /// 注意:SkillService::new 会自动安装编译期内置 skill-creator(Builtin),
    /// catalog 额外含它,不影响下面的断言。
    /// builtin-one 必须在 new 之后写入再 reload——new 在版本标记缺失时(临时目录
    /// 必然如此)会清空重写 .builtin/,先写会被抹掉;reload 只重扫磁盘不重装 builtin。
    fn svc_with_skills(label: &str) -> (SkillService, PathBuf) {
        let dir = tmp_dir(label);
        write_skill(&dir.join("my-skill"), "my-skill", "user skill");
        let svc = SkillService::new(dir.clone()).unwrap();
        write_skill(
            &dir.join(".builtin").join("builtin-one"),
            "builtin-one",
            "builtin skill",
        );
        svc.reload().unwrap();
        (svc, dir)
    }

    #[test]
    fn delete_removes_user_skill_and_reloads() {
        let (svc, dir) = svc_with_skills("del_user");
        assert!(svc.find_by_name("my-skill").is_some());
        let removed = delete_skill_from_catalog(&svc, "my-skill").unwrap();
        // 目录已删
        assert!(!removed.exists(), "skill 目录应被删除: {}", removed.display());
        assert!(!dir.join("my-skill").exists());
        // reload 已发生:catalog 不再含该 skill
        assert!(svc.find_by_name("my-skill").is_none());
        // 其他 skill 不受影响
        assert!(svc.find_by_name("builtin-one").is_some());
    }

    #[test]
    fn delete_rejects_builtin_skill() {
        let (svc, dir) = svc_with_skills("del_builtin");
        let err = delete_skill_from_catalog(&svc, "builtin-one").unwrap_err();
        assert!(matches!(
            err,
            DeleteSkillError::App(AppError::BusinessError(_))
        ));
        // 目录仍在(未删)
        assert!(dir.join(".builtin").join("builtin-one").exists());
        assert!(svc.find_by_name("builtin-one").is_some());
    }

    #[test]
    fn delete_rejects_invalid_name() {
        let (svc, _dir) = svc_with_skills("del_invalid");
        // 路径穿越 / 点号 / 大写下划线 / 斜杠——都过不了 is_valid_skill_name
        for bad in ["../etc", ".builtin", "Bad_Name", "a/b"] {
            let err = delete_skill_from_catalog(&svc, bad).unwrap_err();
            assert!(
                matches!(err, DeleteSkillError::InvalidName(_)),
                "name={bad} 应被拒绝为参数错误"
            );
        }
    }

    #[test]
    fn delete_missing_skill_returns_not_found() {
        let (svc, _dir) = svc_with_skills("del_missing");
        let err = delete_skill_from_catalog(&svc, "no-such-skill").unwrap_err();
        assert!(matches!(
            err,
            DeleteSkillError::App(AppError::NotFoundError(_))
        ));
    }

    #[test]
    fn delete_dir_already_gone_on_disk_returns_not_found() {
        // catalog 有条目但磁盘目录已被外部删除(catalog 与磁盘不一致分支)
        let (svc, dir) = svc_with_skills("del_gone");
        std::fs::remove_dir_all(dir.join("my-skill")).unwrap();
        let err = delete_skill_from_catalog(&svc, "my-skill").unwrap_err();
        assert!(matches!(
            err,
            DeleteSkillError::App(AppError::NotFoundError(_))
        ));
    }

    #[test]
    fn delete_user_override_of_builtin_revives_builtin() {
        // user 上传了与 builtin 同名的 skill:catalog 只剩 scope=User 一条;
        // 删除 user 目录后 builtin 应"复活"(scope 回到 Builtin,builtin 目录未动)。
        let (svc, dir) = svc_with_skills("del_override");
        // user 目录放同名 skill-creator 覆盖内置
        write_skill(
            &dir.join("skill-creator"),
            "skill-creator",
            "user override",
        );
        svc.reload().unwrap();
        let meta = svc.find_by_name("skill-creator").unwrap();
        assert_eq!(meta.scope, SkillScope::User);
        delete_skill_from_catalog(&svc, "skill-creator").unwrap();
        // builtin 目录仍在,catalog 复活为 Builtin
        assert!(dir.join(".builtin").join("skill-creator").exists());
        let revived = svc.find_by_name("skill-creator").unwrap();
        assert_eq!(revived.scope, SkillScope::Builtin);
    }
}
