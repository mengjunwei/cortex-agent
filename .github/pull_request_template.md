<!--
感谢贡献！提交前请逐项完成下方检查。
规范依据：docs/architecture.md
-->

## 变更说明

<!-- 简述本 PR 做了什么、为什么。关联 issue（如 Closes #123）。-->

## 架构合规自查

提交前 **必须** 逐项确认（对应 `docs/architecture.md` §9）：

- [ ] 新代码归属符合 [决策树](../docs/architecture.md#3-决策树新代码该放哪里)
- [ ] 依赖方向未倒置（`use crate::server` 未出现在 `src/server/` 之外的模块）
- [ ] 未引入新的进程级全局（`OnceLock` / `lazy_static`），或符合 [例外清单](../docs/architecture.md#54-例外允许使用全局的场景)
- [ ] Handler 使用强类型 DTO，未用 `serde_json::Value -> Value`（GraphQL 边界除外）
- [ ] 错误类型已加入 `AppError`，未在领域/基础设施层使用 `anyhow` / `String`
- [ ] 日志使用 `tracing`，未在业务代码使用 `log::xxx!`
- [ ] 无未注释的 `unwrap()` / `expect()`（非测试代码）
- [ ] 敏感信息（API Key / 密码 / Token）未出现在日志或错误消息中
- [ ] 新模块有 `//!` 文档注释说明职责
- [ ] 新增 `<Entity>Store` 已 `impl Store` 并复用 `infra::store::{new_id, is_unique_violation}`，未重复实现连接获取样板
- [ ] 单个函数未超过 ~120 行（超过 ~80 行应已评估拆分）
- [ ] 新增的跨切依赖已加入 `AppDeps`，而非新建全局
- [ ] 若违反规范某条，已在下方"例外说明"给出充分理由

## 验证

- [ ] `cargo fmt --all -- --check` 通过
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` 通过
- [ ] `cargo test --all-features` 通过

## 例外说明

<!-- 若本 PR 有意偏离规范，说明条款编号与理由。-->
