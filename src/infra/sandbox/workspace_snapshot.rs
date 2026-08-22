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

/// 快照不打包的目录名:`.cortex-tmp`(TMPDIR/XDG/HOME 重定向区)——ephemeral 临时数据无
/// 容灾价值,且其中 HOME cache symlink 桥若入包,unpack 侧拒绝链接条目会让节点切换恢复
/// 整体失败(见 [`unpack_tar_zst`] 的防 tar-slipping 校验)。
///
/// **任意深度生效**:重定向锚在命令 cwd(shell_sandbox 的 workspace 参数),模型传
/// `workdir: "sub"` 时临时区落在 `sub/.cortex-tmp`——若只跳根级,子目录临时区会进包:
/// ① 膨胀快照;② 体积撞 MAX_SNAPSHOT_BYTES 上限会让上传整体失败,远端快照变陈旧,
/// 故障切换恢复出旧状态(不可逆丢失真实工作成果)。`.cortex-tmp` 是 cortex 专用名,
/// 用户项目自有同名目录的可能性可忽略(代码工具的原子写用同级文件 `x.cortex-tmp`,
/// 不是同名目录,不受影响)。
const SNAPSHOT_SKIP_DIRS: &[&str] = &[".cortex-tmp"];

/// 打包递归深度上限:防病态深层目录(模型 `mkdir -p a/a/a/...` 数万层)把 spawn_blocking
/// 线程栈递归打爆(SIGSEGV 不可捕获,整个进程挂)。正常 workspace 远达不到;超限让本次
/// 快照报错(RUN_FINISHED 上传 warn 可忽略),而非进程崩溃。
const MAX_SNAPSHOT_DEPTH: usize = 64;

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
        append_workspace(&mut builder, dir, Path::new("."))
            .map_err(|e| AppError::ObjectStoreError(format!("快照 tar 打包失败: {e}")))?;
        builder
            .finish()
            .map_err(|e| AppError::ObjectStoreError(format!("快照 tar 收尾失败: {e}")))?;
    }
    let compressed = zstd::stream::encode_all(&tar_buf[..], 3)
        .map_err(|e| AppError::ObjectStoreError(format!("快照 zstd 压缩失败: {e}")))?;
    Ok(Bytes::from(compressed))
}

/// 递归打包 workspace(对齐 `tar::Builder::append_dir_all(".")` 语义),两处差异:
/// ① [`SNAPSHOT_SKIP_DIRS`](.cortex-tmp,任意深度)整目录跳过;
/// ② symlink/硬链等非常规条目一律不打包——unpack 侧为防 tar-slipping 拒绝链接条目,
///    打包侧不产出才能保证「自己打的包自己一定能解」。模型在 workspace 里 `ln -s`
///    出的链接不参与容灾(跨节点后缺失),warn 记录。
fn append_workspace(
    builder: &mut tar::Builder<&mut Vec<u8>>,
    base: &Path,
    rel: &Path,
) -> std::io::Result<()> {
    if rel.components().count() > MAX_SNAPSHOT_DEPTH {
        return Err(std::io::Error::other(format!(
            "快照目录深度超上限 {MAX_SNAPSHOT_DEPTH}: {}",
            rel.display()
        )));
    }
    for entry in std::fs::read_dir(base.join(rel))? {
        let entry = entry?;
        let name = entry.file_name();
        let child_rel = rel.join(&name);
        let ft = entry.file_type()?;
        if ft.is_dir() {
            if SNAPSHOT_SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            builder.append_dir(&child_rel, entry.path())?;
            append_workspace(builder, base, &child_rel)?;
        } else if ft.is_file() {
            builder.append_path_with_name(entry.path(), &child_rel)?;
        } else {
            tracing::warn!(
                "[snapshot] 跳过链接等非常规条目(不参与容灾): {}",
                child_rel.display()
            );
        }
    }
    Ok(())
}

/// 递归累加目录未压缩字节数(打包前预估,防大工作区 OOM)。
/// [`SNAPSHOT_SKIP_DIRS`](.cortex-tmp,任意深度)与打包侧同步跳过,预估口径一致;
/// 符号链接不计(打包侧也不产出链接条目)。深度超 [`MAX_SNAPSHOT_DEPTH`] 提前返回——
/// 它跑在 append_workspace 之前,不设限则病态深层目录在这里就把栈打爆(append 的深度
/// 上限永远走不到);提前返回只是低估体积,后续打包侧会报错兜底。
fn dir_size(dir: &Path) -> u64 {
    dir_size_inner(dir, 0)
}

fn dir_size_inner(dir: &Path, depth: usize) -> u64 {
    if depth > MAX_SNAPSHOT_DEPTH {
        return 0;
    }
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if SNAPSHOT_SKIP_DIRS.contains(&entry.file_name().to_string_lossy().as_ref()) {
                    continue;
                }
                total += dir_size_inner(&entry.path(), depth + 1);
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
            || p.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                )
            })
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
            return Err(AppError::ObjectStoreError(format!(
                "快照 tar 解包失败: {e}"
            )));
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

/// 本地目录是否为空(读取失败视为空)——**restore 门**:有任何条目(含 `.cortex-tmp`)即非空。
///
/// 刻意不忽略 `.cortex-tmp`:restore 每轮请求都会跑(sse 入口),`.cortex-tmp` 是"本节点跑过
/// 命令"的天然标记。若忽略它,模型 `rm -rf *`(glob 不匹配 dotfile)清理后只剩 `.cortex-tmp`
/// 会被误判"空"→ 下轮拉旧快照覆盖 → **已删文件复活**。
async fn is_local_empty(dir: &Path) -> bool {
    match tokio::fs::read_dir(dir).await {
        Ok(mut rd) => rd.next_entry().await.ok().flatten().is_none(),
        Err(_) => true,
    }
}

/// 目录是否含「快照实际会打包的内容」——**upload 门**。
///
/// 与 [`append_workspace`] 打包口径**完全一致**才算内容:常规文件,或非跳过清单里的目录。
/// 根级 `.cortex-tmp` 目录不算;symlink/fifo 等非常规条目不算(打包侧从不产出它们——
/// 否则「只剩 symlink」的工作区会传出零条目的空包覆盖远端有效快照,节点切换后恢复为空,
/// 正是该门要防的不可逆丢失)。同名**文件** `.cortex-tmp` 是常规文件,算内容。
async fn has_snapshot_content(dir: &Path) -> bool {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return false;
    };
    loop {
        match rd.next_entry().await {
            Ok(Some(e)) => {
                let counts = match e.file_type().await {
                    Ok(ft) if ft.is_file() => true,
                    // is_file/is_dir 对 symlink 均为 false(read_dir 不跟随),天然排除链接
                    Ok(ft) if ft.is_dir() => {
                        !SNAPSHOT_SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref())
                    }
                    _ => false,
                };
                if counts {
                    return true;
                }
            }
            Ok(None) => return false,
            Err(_) => return false,
        }
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
    if !sandbox_dir.exists() || !has_snapshot_content(sandbox_dir).await {
        tracing::info!(
            "[snapshot] 本地沙箱不存在或无快照内容(仅剩 .cortex-tmp 临时区),跳过快照上传(避免覆盖远端有效快照)"
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

#[cfg(test)]
mod tests {
    use super::*;

    // round-trip：常规文件保留；根级 .cortex-tmp（含 HOME cache symlink 桥）整体不进包；
    // 用户自建 symlink 不进包（否则 unpack 拒绝链接条目 → 节点切换恢复整体失败）。
    #[test]
    fn pack_skips_cortex_tmp_and_symlinks_round_trip() {
        let src = std::env::temp_dir().join("cortex_snap_test_src");
        let dst = std::env::temp_dir().join("cortex_snap_test_dst");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "hello").unwrap();
        std::fs::write(src.join("sub/b.txt"), "world").unwrap();
        // 根级 .cortex-tmp：临时区 + symlink 桥（HOME 重定向产物）
        std::fs::create_dir_all(src.join(".cortex-tmp/home")).unwrap();
        std::fs::write(src.join(".cortex-tmp/junk"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/root/.cargo", src.join(".cortex-tmp/home/.cargo")).unwrap();
        // 用户在 workspace 里 ln -s 的链接
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", src.join("link.txt")).unwrap();
        // 嵌套 .cortex-tmp（workdir 指向子目录时的临时区）同样不进快照——否则膨胀 +
        // 可能撞 512MB 上限导致上传失败、远端快照陈旧
        std::fs::create_dir_all(src.join("sub/.cortex-tmp")).unwrap();
        std::fs::write(src.join("sub/.cortex-tmp/scratch.bin"), "big").unwrap();

        let bytes = pack_tar_zst(&src).expect("打包应成功");
        unpack_tar_zst(&bytes, &dst).expect("解包应成功");

        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/b.txt")).unwrap(),
            "world"
        );
        assert!(
            !dst.join(".cortex-tmp").exists(),
            "根级 .cortex-tmp 不应进快照"
        );
        assert!(
            !dst.join("sub/.cortex-tmp").exists(),
            "嵌套 .cortex-tmp 不应进快照"
        );
        #[cfg(unix)]
        assert!(!dst.join("link.txt").exists(), "symlink 条目不应进快照");

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    // 判空双门语义:restore 门(is_local_empty)任何条目都算非空——`.cortex-tmp` 是"本节点
    // 跑过命令"的标记,防止 rm -rf * 后误判空→拉旧快照→已删文件复活;upload 门
    // (has_snapshot_content)与打包口径一致——只剩临时区时跳过上传,防空包覆盖远端好快照。
    #[tokio::test]
    async fn empty_gates_restore_counts_tmpdir_upload_does_not() {
        let dir = std::env::temp_dir().join("cortex_snap_test_gates");
        let _ = std::fs::remove_dir_all(&dir);
        // 仅剩 .cortex-tmp 目录
        std::fs::create_dir_all(dir.join(".cortex-tmp/home")).unwrap();
        assert!(
            !is_local_empty(&dir).await,
            "restore 门:有 .cortex-tmp 仍算非空"
        );
        assert!(
            !has_snapshot_content(&dir).await,
            "upload 门:仅 .cortex-tmp 不算内容"
        );

        // 有常规文件:两门都非空
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        assert!(!is_local_empty(&dir).await);
        assert!(has_snapshot_content(&dir).await);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // 打包谓词对齐:根级 symlink 从不入包(append_workspace 跳过),upload 门也不能算内容——
    // 否则「.cortex-tmp + 若干 dotfile symlink」(rm -rf * 不清 dotfile)会传出零条目空包
    // 覆盖远端有效快照,节点切换后恢复为空(不可逆丢失)。
    #[tokio::test]
    async fn root_symlinks_do_not_count_as_content() {
        let dir = std::env::temp_dir().join("cortex_snap_test_symlinks");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".cortex-tmp")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/usr/share/venv", dir.join(".venv")).unwrap();
        assert!(
            !has_snapshot_content(&dir).await,
            "仅 .cortex-tmp + symlink 不应算快照内容"
        );
        // round-trip 佐证:这种工作区打出来的包确实没有可恢复条目
        let bytes = pack_tar_zst(&dir).unwrap();
        let dst = std::env::temp_dir().join("cortex_snap_test_symlinks_dst");
        let _ = std::fs::remove_dir_all(&dst);
        unpack_tar_zst(&bytes, &dst).unwrap();
        assert!(
            !dst.join(".venv").exists() && !dst.join(".cortex-tmp").exists(),
            "包内不应有可恢复内容"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dst);
    }

    // 谓词一致性:根下名为 .cortex-tmp 的**文件**会被打包(append_workspace 的 is_file 分支),
    // upload 门也必须算内容——否则空包覆盖远端、恢复丢该文件。
    #[tokio::test]
    async fn root_cortex_tmp_file_counts_as_content() {
        let dir = std::env::temp_dir().join("cortex_snap_test_file");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".cortex-tmp"), "i am a file").unwrap();
        assert!(has_snapshot_content(&dir).await, "同名文件应算内容");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 深度上限:超 MAX_SNAPSHOT_DEPTH 的病态深层目录让快照报错,而非递归打爆栈。
    #[test]
    fn pack_rejects_pathological_depth() {
        let dir = std::env::temp_dir().join("cortex_snap_test_deep");
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = dir.clone();
        std::fs::create_dir_all(&p).unwrap();
        for _ in 0..(MAX_SNAPSHOT_DEPTH + 2) {
            p = p.join("d");
        }
        std::fs::create_dir_all(&p).unwrap();
        let r = pack_tar_zst(&dir);
        assert!(r.is_err(), "超深目录应报错而非崩溃/成功");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
