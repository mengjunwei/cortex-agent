//! Skill 安装 API — 从工作区路径或 tar.gz 上传安装 skill 到 skill 目录。
//!
//! 沙箱内 skill 目录只读,agent/前端无法直接写,需后端代写:
//! - `POST /api/skills/install`:JSON body `{path, overwrite}`,从工作区绝对路径安装
//! - `POST /api/skills/upload`:multipart tar.gz 上传安装
//!
//! 两者流程:校验源 → 解析 SKILL.md frontmatter 取 name → `is_valid_skill_name` 校验
//! → 复制到 `{skill_dir}/{name}/` → `SkillService::reload()` 热重载。
//!
//! 安全:name 仅允许 `[a-z0-9-]`(`is_valid_skill_name`,与 mention 正则一致),
//! 天然杜绝路径穿越;tar.gz 解包逐条目校验路径(拒绝绝对路径 / `..` / 链接条目)。
//! 鉴权与 screenshots/upload 一致:auth 启用时强制登录。

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
use crate::skill::is_valid_skill_name;
use crate::skill::loader::{parse_frontmatter, read_skill_file_text};

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

/// Skill 安装路由组(挂载到根路径,路由以 `/api/skills/` 开头)。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/skills/install", post(install_skill))
        .route(
            "/api/skills/upload",
            post(upload_skill).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
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
        return Json(response::err(
            code::UNAUTHORIZED,
            "请先登录后再安装 Skill",
        ));
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
        return Json(response::err(
            code::UNAUTHORIZED,
            "请先登录后再上传 Skill",
        ));
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
            },
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
        return Json(response::err(code::INVALID_PARAMS, format!("解包失败(支持 .tar.gz/.tgz/.zip/.tar): {e}")));
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
            return Json(response::err(code::INVALID_PARAMS, "无法定位 SKILL.md 所在目录"));
        }
    };
    let result = do_install(&state, src_dir, &skill_md, overwrite);
    // 复制完成(读临时目录)后清理,无论成败(best-effort)
    let _ = std::fs::remove_dir_all(&temp_dir);
    result
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
        None => {
            return Json(response::err(
                code::INTERNAL,
                "Skill 服务未初始化,无法安装",
            ))
        }
    };
    let dest = skill_service.skill_dir().join(&name);
    match install_to_dest(src_dir, &dest, overwrite) {
        Ok(()) => match skill_service.reload() {
            Ok(()) => {
                tracing::info!(
                    "[skill-install] 安装成功: {name} <- {}",
                    src_dir.display()
                );
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
    unpack_tar_archive(&bytes[..], dir)
}

/// tar 解包公共逻辑(逐条目校验)。
fn unpack_tar_archive<R: std::io::Read>(reader: R, dir: &Path) -> Result<(), AppError> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .map_err(|e| AppError::FileError(format!("读取 tar 条目失败: {e}")))?
    {
        let mut entry =
            entry.map_err(|e| AppError::FileError(format!("tar 条目读取失败: {e}")))?;
        let p = entry
            .path()
            .map_err(|e| AppError::FileError(format!("tar 路径解析失败: {e}")))?;
        if p.is_absolute()
            || p
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir))
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
            || p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        let content =
            format!("---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\nbody");
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
                .append_data(&mut f, "my-skill/SKILL.md", &mut std::io::Cursor::new(&b"body"[..]))
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
}
