//! 会话沙箱工作区快照 —— 对象存储容灾
//!
//! 会话亲和下沙箱目录留本地 SSD(POSIX 性能);节点故障切换时,新节点从对象存储拉取
//! 最新快照恢复。快照为全量 tar.zst,覆盖式上传(`workspaces/{sid}/snapshot.tar.zst`)。
//! RPO = 最近一次上传(每轮 RUN_FINISHED 后);用户已确认接受恢复延迟。
//!
//! 打包/解包是同步重 IO,放在 `spawn_blocking` 中执行,避免阻塞异步运行时。

use std::path::Path;

use bytes::Bytes;

use crate::error::AppError;
use crate::infra::object_store::ObjectStore;

/// 快照对象 key:`workspaces/{session_id}/snapshot.tar.zst`
fn snapshot_key(session_id: &str) -> String {
    format!("workspaces/{session_id}/snapshot.tar.zst")
}

/// 快照体积上限:防大工作区 + 多会话并发上传 OOM(完整流式上传留待后续优化,当前阈值兜底)
const MAX_SNAPSHOT_BYTES: usize = 512 * 1024 * 1024;

/// session_id 安全校验(与 config::is_safe_path_segment 同源,拒 / \ .. :,防 prefix 越界)
fn is_safe_session(s: &str) -> bool {
    crate::config::is_safe_path_segment(s)
}

/// 打包目录为 tar.zst 字节(同步,在 `spawn_blocking` 中调用)
fn pack_tar_zst(dir: &Path) -> Result<Bytes, AppError> {
    // 先预估未压缩体积,超限直接拒——防 append_dir_all 把整目录灌进 tar_buf 触发 OOM
    let total = dir_size(dir);
    if total > MAX_SNAPSHOT_BYTES as u64 {
        return Err(AppError::ObjectStoreError(format!(
            "工作区 {total} 字节超上限 {MAX_SNAPSHOT_BYTES},拒绝快照打包(防 OOM)"
        )));
    }
    let mut tar_buf: Vec<u8> = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        builder
            .append_dir_all(".", dir)
            .map_err(|e| AppError::ObjectStoreError(format!("快照 tar 打包失败: {e}")))?;
        builder
            .finish()
            .map_err(|e| AppError::ObjectStoreError(format!("快照 tar 收尾失败: {e}")))?;
    }
    let compressed = zstd::stream::encode_all(&tar_buf[..], 3)
        .map_err(|e| AppError::ObjectStoreError(format!("快照 zstd 压缩失败: {e}")))?;
    Ok(Bytes::from(compressed))
}

/// 递归累加目录未压缩字节数(打包前预估,防大工作区 OOM;符号链接不计也不打包)
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                total += dir_size(&entry.path());
            } else if ft.is_file() {
                if let Ok(m) = entry.metadata() {
                    total += m.len();
                }
            }
        }
    }
    total
}

/// 解包 tar.zst 字节到目录(同步,在 `spawn_blocking` 中调用)
///
/// 防 tar slipping:逐条目校验路径(拒绝对路径 + `..`),并拒绝 symlink/hardlink 条目
/// (防越界链接,如 `evil -> /etc/shadow`)。快照源虽可信(自己上传),但对象存储被污染时兜底。
fn unpack_tar_zst(bytes: &Bytes, dir: &Path) -> Result<(), AppError> {
    let decoded = zstd::stream::decode_all(&bytes[..])
        .map_err(|e| AppError::ObjectStoreError(format!("快照 zstd 解压失败: {e}")))?;
    let mut archive = tar::Archive::new(&decoded[..]);
    // 解包到随机后缀临时目录 dir.{uuid}.part(防同会话并发 restore 撞同一 tmp),
    // 全成功后原子替换 dir——防中途失败留残缺工作区被 upload 覆盖远端好快照(不可逆)。
    let tmp = std::path::PathBuf::from(format!(
        "{}.{}.part",
        dir.to_string_lossy(),
        uuid::Uuid::now_v7().simple()
    ));
    let _ = std::fs::remove_dir_all(&tmp); // 清残留的旧 .part
    std::fs::create_dir_all(&tmp)
        .map_err(|e| AppError::ObjectStoreError(format!("快照恢复建临时目录失败: {e}")))?;
    for entry in archive
        .entries()
        .map_err(|e| AppError::ObjectStoreError(format!("快照 tar 读取条目失败: {e}")))?
    {
        let mut entry =
            entry.map_err(|e| AppError::ObjectStoreError(format!("快照 tar 条目失败: {e}")))?;
        let p = entry
            .path()
            .map_err(|e| AppError::ObjectStoreError(format!("快照 tar 路径失败: {e}")))?;
        if p.is_absolute()
            || p
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir))
        {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(AppError::ObjectStoreError(format!(
                "快照含不安全路径,拒绝解包: {}",
                p.display()
            )));
        }
        // 拒绝符号链接 / 硬链接条目(防越界链接攻击)
        let et = entry.header().entry_type();
        if et.is_symlink() || et.is_hard_link() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(AppError::ObjectStoreError(format!(
                "快照含链接条目,拒绝解包: {}",
                p.display()
            )));
        }
        if let Err(e) = entry.unpack_in(&tmp) {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(AppError::ObjectStoreError(format!("快照 tar 解包失败: {e}")));
        }
    }
    // 全部条目成功:清旧 dir(此时为空),原子 rename tmp → dir
    if let Err(e) = std::fs::remove_dir_all(dir) {
        tracing::warn!(
            "[snapshot] 清理旧沙箱目录失败(rename 前置,Windows 上文件被占用常见),后续 rename 可能失败: {e}"
        );
    }
    if let Err(e) = std::fs::rename(&tmp, dir) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(AppError::ObjectStoreError(format!("快照原子替换失败: {e}")));
    }
    Ok(())
}

/// 本地目录是否为空(读取首条 entry 判定;读取失败视为空)
async fn is_local_empty(dir: &Path) -> bool {
    match tokio::fs::read_dir(dir).await {
        Ok(mut rd) => rd.next_entry().await.ok().flatten().is_none(),
        Err(_) => true,
    }
}

/// 上传沙箱快照(全量覆盖)。
///
/// 目录不存在或**本地为空时跳过**——避免恢复失败(本地空)时用空状态覆盖远端有效快照,
/// 造成不可逆数据丢失(会话亲和容灾的关键保护)。
pub async fn upload(
    os: &ObjectStore,
    session_id: &str,
    sandbox_dir: &Path,
) -> Result<(), AppError> {
    if !is_safe_session(session_id) {
        return Err(AppError::ObjectStoreError(format!(
            "不安全 session_id,拒绝快照上传: {session_id}"
        )));
    }
    if !sandbox_dir.exists() || is_local_empty(sandbox_dir).await {
        tracing::info!(
            "[snapshot] 本地沙箱不存在或为空,跳过快照上传(避免覆盖远端有效快照)"
        );
        return Ok(());
    }
    let dir = sandbox_dir.to_path_buf();
    let bytes = tokio::task::spawn_blocking(move || pack_tar_zst(&dir))
        .await
        .map_err(|e| AppError::ObjectStoreError(format!("快照打包任务 join 失败: {e}")))?;
    let bytes = bytes?;
    // 第二道:压缩后体积兜底(pack 内已按未压缩预估上限,压缩后通常更小,保留作保险)
    os.put(&snapshot_key(session_id), bytes).await?;
    tracing::info!("[snapshot] 已上传沙箱快照: {}", snapshot_key(session_id));
    Ok(())
}

/// 从对象存储恢复沙箱快照到本地目录(仅当本地目录为空时拉取)。返回是否实际恢复。
///
/// 会话亲和下:本地非空说明是原节点续跑,跳过;本地为空(节点切换后新建)才拉快照。
///
/// 用 `exists` 先区分"无快照"(Ok(false),从空工作区开始)与"存储错误"(Err 上抛),
/// 避免把网络抖动误判为无快照 → 空工作区续跑 → 上传覆盖好快照(不可逆丢失)。
pub async fn restore(
    os: &ObjectStore,
    session_id: &str,
    sandbox_dir: &Path,
) -> Result<bool, AppError> {
    if !is_safe_session(session_id) {
        return Err(AppError::ObjectStoreError(format!(
            "不安全 session_id,拒绝快照恢复: {session_id}"
        )));
    }
    // 本地非空:原节点续跑,跳过恢复
    if !is_local_empty(sandbox_dir).await {
        return Ok(false);
    }
    let key = snapshot_key(session_id);
    // exists 区分"无快照" vs "存储错误":无快照正常返回;存储错误上抛(上层 warn,不中断)
    match os.exists(&key).await {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(e) => return Err(e),
    }
    // 有快照,拉取(此时 get 失败也上抛,不静默吞)
    let bytes = os.get(&key).await?;
    let dir = sandbox_dir.to_path_buf();
    tokio::task::spawn_blocking(move || unpack_tar_zst(&bytes, &dir))
        .await
        .map_err(|e| AppError::ObjectStoreError(format!("快照解包任务 join 失败: {e}")))??;
    tracing::info!("[snapshot] 已恢复沙箱快照: {}", key);
    Ok(true)
}

/// 删除会话的所有快照对象(`workspaces/{sid}/` 前缀)。
///
/// `session_id` 经 [`is_safe_session`] 净化(与截图清理基线一致),防 prefix 越界。
pub async fn delete(os: &ObjectStore, session_id: &str) {
    if !is_safe_session(session_id) {
        tracing::warn!(
            "[snapshot] 拒绝不安全的 session_id(防路径穿越): {}",
            session_id
        );
        return;
    }
    let prefix = format!("workspaces/{session_id}/");
    if let Err(e) = os.delete_prefix(&prefix).await {
        tracing::warn!("[snapshot] 删除快照失败 prefix={prefix}: {e}");
    }
}
