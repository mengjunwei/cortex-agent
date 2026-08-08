# RustFS 部署指南（单机测试 · systemd 管理）

> S3 兼容对象存储服务器，作为 cortex-agent 的**共享文件层**：承载截图（`screenshots/`）、上传图（`uploads/`）、沙箱快照（`workspaces/`）、artifact。Apache 2.0、Rust 实现，可作为 MinIO 的二进制替换。官方仓库：[rustfs/rustfs](https://github.com/rustfs/rustfs)，官方文档：<https://docs.rustfs.com>。
>
> **本文定位**：单机测试部署，**不挂载独立磁盘**（数据直接写 `/home/rustfs-user/data`），systemd 托管、开机自启。生产/多盘/多节点集群见 [§八](#八生产环境升级要点) 与官方文档。
>
> **关联文档**：
> - [DEPLOY.md §3.1 `[object_storage]`](../DEPLOY.md) — cortex-agent 侧对接配置
> - [对象存储(RustFS)改造设计](./superpowers/specs/2026-08-04-object-storage-ha-design.md) — 架构与分期方案

---

## 一、适用场景与前置说明

| 项 | 说明 |
|---|---|
| 部署模式 | **SNSD**（单节点单盘），测试用 |
| 数据位置 | `/home/rustfs-user/data/rustfs0`（系统盘普通目录，**不挂载独立盘**） |
| 进程管理 | systemd（`rustfs.service`），开机自启 |
| 运行账号 | `rustfs-user`（系统账号，禁止登录） |
| 端口 | `9000`（S3 API）、`9001`（管理控制台） |

> ⚠️ **测试可接受，生产不建议**：数据落在系统盘上，盘坏则数据全丢，且对象增长可能撑满系统盘。上生产时把独立 XFS 盘挂到 `/home/rustfs-user/data/rustfs0` 即可，**cortex-agent 侧配置无需改动**。

---

## 二、环境要求

| 项 | 要求 | 说明 |
|---|---|---|
| 操作系统 | Linux x86_64 / aarch64 | 本文以麒麟 Kylin Server / RHEL 系为例（yum/dnf） |
| 内核 | ≥ 4.x（5.x/6.x 更佳） | 影响I/O 与网络性能 |
| 内存 | ≥ 2 GB | 测试下限；生产建议 128 GB+ |
| 文件系统 | XFS（推荐）/ ext4 | 测试用 ext4 系统盘亦可；生产强制 XFS |
| 时间同步 | 单机可忽略 | 多节点强制（chronyd/timesyncd），否则启动失败 |
| 工具 | `wget` / `unzip` | 麒麟可能缺 `unzip`：`yum install -y unzip` |
| 网络 | `dl.rustfs.com` 国内可直连 | 无需换源 |

放行端口（按实际防火墙二选一；测试机嫌麻烦可 `systemctl stop firewalld`）：

```bash
# firewalld
firewall-cmd --zone=public --add-port=9000/tcp --permanent
firewall-cmd --zone=public --add-port=9001/tcp --permanent
firewall-cmd --reload

# 或 UFW（Ubuntu/Debian 系）
sudo ufw allow 9000/tcp && sudo ufw allow 9001/tcp
```

---

## 三、安装步骤

### 3.1 创建专用用户

```bash
sudo useradd -r -m -d /home/rustfs-user -s /sbin/nologin rustfs-user
#   -r            系统账号
#   -m -d ...     创建家目录 /home/rustfs-user（数据/日志将放这里）
#   -s nologin    禁止交互登录
```

### 3.2 创建数据与日志目录

```bash
sudo mkdir -p /home/rustfs-user/data/rustfs0 /home/rustfs-user/logs
sudo chown -R rustfs-user:rustfs-user /home/rustfs-user
sudo chmod -R 750 /home/rustfs-user/data /home/rustfs-user/logs
```

### 3.3 下载并安装二进制

```bash
# 按架构选包
case "$(uname -m)" in
  x86_64)  ARCH=x86_64 ;;
  aarch64) ARCH=aarch64 ;;        # 鲲鹏 / 飞腾
  *) echo "不支持的架构: $(uname -m)"; exit 1 ;;
esac

cd /tmp
wget https://dl.rustfs.com/artifacts/rustfs/release/rustfs-linux-${ARCH}-musl-latest.zip
unzip rustfs-linux-${ARCH}-musl-latest.zip
chmod +x rustfs
sudo mv rustfs /usr/local/bin/
rustfs --version                  # 验证安装
```

### 3.4 配置环境变量 `/etc/default/rustfs`

> 此处 `RUSTFS_ACCESS_KEY` / `RUSTFS_SECRET_KEY` 是 RustFS 的**根凭证**，须与 cortex-agent 的 `config/config.toml` → `[object_storage]` 的 `access_key` / `secret_key` **保持一致**。下面用项目示例值 `cortex` / `cortex12345`。

```bash
sudo tee /etc/default/rustfs <<'EOF'
# === 与 cortex-agent config.toml [object_storage] 对齐 ===
RUSTFS_ACCESS_KEY=cortex
RUSTFS_SECRET_KEY=cortex12345

# 数据卷（单机测试：系统盘普通目录）
RUSTFS_VOLUMES="/home/rustfs-user/data/rustfs0"

# 监听
RUSTFS_ADDRESS=":9000"            # S3 API，所有网卡 9000
RUSTFS_CONSOLE_ENABLE=true        # Web 控制台（9001）

# 日志
RUST_LOG=error
RUSTFS_OBS_LOG_DIRECTORY="/home/rustfs-user/logs/"
EOF
```

> ⚠️ **生产必改**：用 `openssl rand -hex 16`（ACCESS_KEY）和 `openssl rand -base64 24`（SECRET_KEY）生成强随机值，并同步更新 `config.toml`。`cortex12345` 仅为联调示例。

### 3.5 创建 systemd 服务单元

```bash
sudo tee /etc/systemd/system/rustfs.service <<'EOF'
[Unit]
Description=RustFS Object Storage Server
Documentation=https://rustfs.com/docs/
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
NotifyAccess=main
User=rustfs-user
Group=rustfs-user

WorkingDirectory=/home/rustfs-user
EnvironmentFile=-/etc/default/rustfs
ExecStart=/usr/local/bin/rustfs $RUSTFS_VOLUMES

LimitNOFILE=1048576
LimitNPROC=32768
TasksMax=infinity

Restart=always
RestartSec=10s
OOMScoreAdjust=-1000
SendSIGKILL=no
TimeoutStartSec=120s
TimeoutStopSec=30s

# 安全加固（注意：数据在 /home 下，ProtectHome 必须为 false）
NoNewPrivileges=true
ProtectHome=false
PrivateTmp=true
ProtectClock=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
RestrictRealtime=true

# 用 journal —— 勿 append 到 rustfs.log：rustfs 自身已通过 RUSTFS_OBS_LOG_DIRECTORY
# 把滚动日志写进该文件，systemd 再 append stdout 会触发 FATAL "Log sink conflict"（见 §九 Q5）
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF
```

> ⚠️ **易错点**：`ProtectHome=true` 会让服务看不到 `/home/rustfs-user`，导致启动即失败。本部署数据在 `/home` 下，**必须 `false`**（已写好）。若改用 `/data` 等非 `/home` 路径，可恢复为 `true`。

> ⚠️ **日志 sink 冲突（高频踩坑）**：`StandardOutput`/`StandardError` 切勿 `append:` 到 `rustfs.log`。rustfs 自身已用 `RUSTFS_OBS_LOG_DIRECTORY` 把滚动日志写进该文件，systemd 再把 stdout 重定向到同一文件 → 启动即 `FATAL: Log sink conflict`、退出 1。上面已用 `journal`（stdout 进 journald，文件日志交给 rustfs 自己）。典型现象是「手动 `sudo -u rustfs-user rustfs ...` 能起、`systemctl start rustfs` 起不来」，排查见 §九 Q5。

### 3.6 启动与开机自启

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now rustfs          # 启动 + 开机自启
sudo systemctl status rustfs                # 应为 active (running)
```

---

## 四、创建 Bucket 与验证

> ⚠️ **硬前置——桶必须先建，否则 cortex-agent 启动失败**。
> cortex-agent 的对象存储客户端启动时会跑连通性自检（`src/infra/object_store.rs` 的 `op.check()`），若 `config.toml` 配置的桶（`cortex`）不存在，自检直接失败、主程序起不来。排查见 §九 Q2。
>
> **为什么不能代码自动建桶**：项目用的 opendal `0.51.2` 只能操作「已存在的桶」——其 `S3Builder` 没有 `auto_create_bucket`、`Operator` 也不暴露 `create_bucket`。所以 `cortex` 桶必须在部署阶段手动（或脚本）建一次。详见 §九 Q4。

RustFS 起来后，按下文为 cortex-agent 建好 `cortex` 桶（即 `config.toml` 里 `bucket = "cortex"`）。

> 🔑 **三处密钥必须完全一致**（最常见踩坑点）：
>
> | 位置 | 变量 | 用途 |
> |---|---|---|
> | RustFS 服务端 `/etc/default/rustfs` | `RUSTFS_ACCESS_KEY` / `RUSTFS_SECRET_KEY` | RustFS 启动认的**根凭证**，**控制台/mc 登录就用它** |
> | cortex `config/config.toml` 的 `[object_storage]` | `access_key` / `secret_key` | cortex 连 RustFS 用 |
> | 登录控制台 / `mc alias set` 时输入的 | — | 必须等于上面根凭证 |
>
> 任何一处对不上 → 控制台登录失败 / mc 报 Access Denied / cortex 自检失败。排查见 §九 Q1。

### 4.1 控制台

浏览器访问 `http://<服务器IP>:9001`，用 **`/etc/default/rustfs` 里的根凭证** 登录（不是 `config.toml` 里那个值）→ 右上角 **Create Bucket** → 名称填 `cortex` → 创建。

> 登录不上？`sudo cat /etc/default/rustfs` 看服务端真实根凭证，确认它和 `config.toml`、你输入的密码三者一致；不一致就改 `/etc/default/rustfs` 后 `sudo systemctl restart rustfs`。详见 §九 Q1。

### 4.2 命令行（mc）

RustFS 完全兼容 MinIO Client（`mc`）：

```bash
# 安装 mc
case "$(uname -m)" in
  x86_64)  MC_ARCH=amd64 ;;
  aarch64) MC_ARCH=arm64 ;;
esac
wget https://dl.min.io/client/mc/release/linux-${MC_ARCH}/mc
chmod +x mc && sudo mv mc /usr/local/bin/

# 配置别名 —— 用 /etc/default/rustfs 的根凭证；密钥含 + / = 等特殊字符必须加单引号
mc alias set cortex http://127.0.0.1:9000 '<RUSTFS_ACCESS_KEY>' '<RUSTFS_SECRET_KEY>'

# 创建 bucket
mc mb cortex/cortex

# 验证：传一个文件试试
echo "rustfs ok" > /tmp/_t.txt
mc cp /tmp/_t.txt cortex/cortex/
mc ls cortex/cortex
mc rm cortex/cortex/_t.txt
```

### 4.3 健康检查

```bash
ss -ntpl | grep 900                         # 9000 / 9001 在监听
curl http://127.0.0.1:9000/minio/health/live    # 健康端点
tail -f /home/rustfs-user/logs/rustfs*.log  # 无报错
```

---

## 五、对接 cortex-agent

### 5.1 编辑 `config/config.toml`

确认 `[object_storage]` 段与上面 RustFS 一致：

```toml
[object_storage]
enabled = true
endpoint = "http://localhost:9000"   # RustFS 与 cortex 同机；跨机填 RustFS 内网 IP
region = "us-east-1"
bucket = "cortex"                     # §4 已创建
access_key = "cortex"                 # 同 /etc/default/rustfs 的 RUSTFS_ACCESS_KEY
secret_key = "cortex12345"            # 同 RUSTFS_SECRET_KEY（敏感：不入日志）
path_style = true                     # RustFS/MinIO 用 true
presign_ttl_secs = 604800             # presigned URL 有效期，默认 7 天
```

> **跨机部署**：`endpoint` 改成 RustFS 服务器的内网地址（如 `http://rustfs.internal:9000`），并保证 cortex 节点与 RustFS 网络互通（上传图 presigned URL 给模型拉取，见[改造设计 §4.3](./superpowers/specs/2026-08-04-object-storage-ha-design.md)）。

### 5.2 启动 cortex-agent 验证

启动后，触发一次截图 / 上传图 / 沙箱会话，观察：

- RustFS 日志 `/home/rustfs-user/logs/rustfs.log` 出现对应 `PUT/GET`；
- `mc ls cortex/cortex/` 下出现 `screenshots/`、`uploads/`、`workspaces/` 前缀；
- cortex 日志无对象存储相关报错。

若想完全绕开对象存储做极简本地调试：`enabled = false`（截图/上传图/沙箱快照随之不可用，主程序仍可启动，见 [DEPLOY.md §七 降级策略](../DEPLOY.md)）。

---

## 六、常用运维命令

```bash
# 启停 / 重启 / 状态
sudo systemctl start|stop|restart rustfs
sudo systemctl status rustfs

# 日志
journalctl -u rustfs -f                       # stdout / 启动报错（StandardOutput=journal）
tail -f /home/rustfs-user/logs/rustfs.log     # rustfs 自身 rolling log（RUSTFS_OBS_LOG_DIRECTORY）

# 改完 /etc/default/rustfs 或 service 文件后生效
sudo systemctl daemon-reload && sudo systemctl restart rustfs

# 取消开机自启
sudo systemctl disable rustfs
```

### 目录结构

```
/home/rustfs-user/
├── data/rustfs0/        ← 数据（系统盘普通目录）
└── logs/                ← 日志（rustfs.log / rustfs-err.log）
/usr/local/bin/rustfs    ← 二进制
/etc/default/rustfs      ← 环境变量配置
/etc/systemd/system/rustfs.service
```

---

## 七、麒麟 / 国产化注意点

| 项 | 说明 |
|---|---|
| **架构** | 鲲鹏 / 飞腾是 **aarch64**，务必下 `rustfs-linux-aarch64-musl-latest.zip`；x86 包跑不起来 |
| **包管理** | 装 `unzip` 用 `yum install -y unzip`（非 apt） |
| **时间同步** | 麒麟默认用 `chronyd`，多节点前 `systemctl status chronyd` 确认启用 |
| **SELinux** | 若 enforcing 导致 RustFS 读不到 `/home/rustfs-user`：先 `setenforce 0` 排查，长期方案用 `semanage fcontext` 给数据目录打标签 |
| **下载** | `dl.rustfs.com` 为官方 CDN，国内直连，无需换源 |

---

## 八、生产环境升级要点

测试通过后上生产，**cortex-agent 侧 `config.toml` 不变**，只升级 RustFS 部署：

1. **独立数据盘**：用 XFS 格式化独立盘并挂到 `/home/rustfs-user/data/rustfs0`（替换原普通目录），cortex 无感。
   ```bash
   sudo mkfs.xfs -i size=512 -n ftype=1 -L RUSTFS0 /dev/sdb
   echo 'LABEL=RUSTFS0 /home/rustfs-user/data/rustfs0  xfs  defaults,noatime,nodiratime  0 0' | sudo tee -a /etc/fstab
   sudo mount -a && sudo chown -R rustfs-user:rustfs-user /home/rustfs-user
   ```
2. **多盘（SNMD）**：`RUSTFS_VOLUMES="/home/rustfs-user/data/rustfs0 /data/rustfs1 ..."`，RustFS 自动做纠删码冗余。
3. **多节点（MNMD）**：每节点同配，目录名跨节点一致；前置 Nginx/负载均衡做会话亲和与高可用（见[改造设计 §4.6](./superpowers/specs/2026-08-04-object-storage-ha-design.md)）。
4. **强密钥**：根凭证换 `openssl` 随机串，同步 `config.toml`。
5. **HTTPS**：前置 Nginx 做 TLS 终止，`config.toml` 的 `endpoint` 改 `https://`。

> 官方三种模式完整文档：
> - [单节点单盘 SNSD](https://docs.rustfs.com/installation/linux/single-node-single-disk)
> - [单节点多盘 SNMD](https://docs.rustfs.com/installation/linux/single-node-multiple-disk)
> - [多节点多盘 MNMD](https://docs.rustfs.com/installation/linux/multiple-node-multiple-disk)

---

## 九、常见问题排查

### Q1 控制台登录不上 / mc 报 Access Denied

99% 是**密钥不一致**。控制台与 `mc` 登录用的是 RustFS **服务端根凭证**（`/etc/default/rustfs` 的 `RUSTFS_ACCESS_KEY/SECRET_KEY`），不是 `config.toml` 里那个值——虽然二者最终必须一致。

```bash
sudo cat /etc/default/rustfs                          # ① 服务端根凭证（控制台/mc 用它登录）
grep -A8 '\[object_storage\]' config/config.toml      # ② cortex 连接用
```

两处的 access_key / secret_key 必须完全相同。不一致 → 改 `/etc/default/rustfs` 对齐 → `sudo systemctl restart rustfs`。

### Q2 cortex-agent 启动报「对象存储连通性自检失败」

`src/infra/object_store.rs` 启动会执行 `op.check()`，失败常见三类原因：

| 原因 | 排查 | 处理 |
|---|---|---|
| **桶没建** | `mc ls cortex` 看有无 `cortex` | `mc mb cortex/cortex`（见 §四） |
| **密钥不对** | 见 Q1 | 对齐三处密钥 |
| **RustFS 没起 / 端口不通** | `systemctl status rustfs`、`ss -ntpl \| grep 900`、`curl http://127.0.0.1:9000/minio/health/live` | 启服务 / 放行端口 |

只想跳过对象存储做最小本地调试：`config.toml` 设 `[object_storage].enabled = false`（截图/上传图/沙箱快照不可用，主程序仍可启动）。

### Q3 mc 连接报错（密钥含特殊字符）

密钥里若有 `+` `/` `=`（Base64 串常见），shell 会特殊解析，**必须用单引号包裹**：

```bash
mc alias set cortex http://127.0.0.1:9000 '<ACCESS_KEY>' '<SECRET_KEY>'
```

### Q4 能不能让 cortex 启动时自动建桶？

当前**不能**。opendal `0.51.2` 的 `S3Builder` 没有 `auto_create_bucket`、`Operator` 也不暴露 `create_bucket`——它只能操作已存在的桶。所以 `cortex` 桶必须在部署阶段手动/脚本建一次（见 §四）。

> 将来若要做「代码自动建桶」：依赖树里已有 opendal 传递进来的 `reqsign 0.16`（AWS SigV4）与 `hmac`/`sha2`，可在 `object_store.rs` 自检前用 `reqwest` + `reqsign` 签名发一个 `PUT /cortex` 建桶请求（需在 `Cargo.toml` 显式声明 `reqsign`——间接依赖在 Rust 2024 edition 不自动暴露）。现阶段手动建桶已足够。

### Q5 systemd 启动即 FATAL：「Log sink conflict ... same file」

现象：手动 `sudo -u rustfs-user /usr/local/bin/rustfs /home/rustfs-user/data/rustfs0` 能起，但 `systemctl start rustfs` 立刻退出 1，反复出现：

```
[FATAL] Observability initialization failed: Telemetry initialization failed:
Log sink conflict: stdout and rolling log resolve to the same file
/home/rustfs-user/logs/rustfs.log; route stdout to journald or choose a different log file
```

> 注：该报错默认进 systemd 的 stderr。若单元用了 `StandardError=append:...`，它会**被吞进文件而不是 journal**，导致 `journalctl -u rustfs` 只见 `Main process exited, status=1/FAILURE` 而看不到 rustfs 自身的 FATAL。排查时优先 `cat /home/rustfs-user/logs/rustfs-err.log`，或临时把 `StandardError` 改成 `journal` 让报错直接进 journalctl。

**原因**：rustfs 自身的 rolling log（`RUSTFS_OBS_LOG_DIRECTORY` → 写 `rustfs.log`）与 systemd 的 stdout 重定向（`StandardOutput=append:.../rustfs.log`）解析到**同一文件**，新版 rustfs 检测到两个日志 sink 撞车即拒绝启动。手动跑能起、systemd 起不来，差异就在此——手动跑 stdout 进终端，不触发冲突。

**处理**：把 `StandardOutput`/`StandardError` 改成 `journal`，文件日志交给 rustfs 自己的 rolling log（§3.5 已按此配置）：

```bash
sudo sed -i -e 's#^StandardOutput=.*#StandardOutput=journal#' \
            -e 's#^StandardError=.*#StandardError=journal#' \
            /etc/systemd/system/rustfs.service
sudo systemctl daemon-reload && sudo systemctl restart rustfs
```

改完：stdout 进 `journalctl -u rustfs`，rolling log 仍写 `/home/rustfs-user/logs/rustfs.log`，各管各、不再撞车。

---

## 参考来源

- [RustFS 官方文档 — Linux 前置条件与服务设置](https://docs.rustfs.com/installation/linux/prerequisites-and-service)
- [RustFS 官方文档 — 单节点单盘部署 (SNSD)](https://docs.rustfs.com/installation/linux/single-node-single-disk)
- [Bitnesia — RustFS 安装与 systemd 配置（含安全加固）](https://www.bitnesia.com/en/how-to-install-rustfs-s3-compatible)
- [RustFS GitHub](https://github.com/rustfs/rustfs)
