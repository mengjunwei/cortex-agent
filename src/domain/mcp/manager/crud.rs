//! CRUD 编排：在 Store 之上做归属校验、缓存失效、连接重建。

use crate::domain::mcp::dto::{CreateMcpServerInput, McpServerResponse, UpdateMcpServerInput};
use crate::domain::mcp::enums::Status;
use crate::domain::mcp::models::{McpServer, ServerHealth};
use crate::domain::mcp::store::McpServerStore;
use crate::error::AppError;

use super::McpManager;

impl McpManager {
    pub async fn create_server(
        &self,
        input: &CreateMcpServerInput,
        user_id: &str,
    ) -> Result<McpServerResponse, AppError> {
        let server = self.store.create(input, user_id).await?;
        Ok(McpServerStore::to_response(&server, ServerHealth::Unknown))
    }

    /// 归属校验：管理员或归属人放行（完全隔离）
    pub(super) fn allows(server: &McpServer, user_id: &str, is_admin: bool) -> bool {
        is_admin || server.user_id == user_id
    }

    pub async fn update_server(
        &self,
        id: &str,
        input: &UpdateMcpServerInput,
        user_id: &str,
        is_admin: bool,
    ) -> Result<Option<McpServerResponse>, AppError> {
        // 归属校验：仅归属人/管理员可改（fetch 后校验，未通过返回 None → 404）
        let existing = match self.store.get_by_id(id).await? {
            Some(s) if Self::allows(&s, user_id, is_admin) => s,
            _ => return Ok(None),
        };
        let _ = existing;
        let server = self.store.update(id, input).await?;
        match server {
            Some(s) => {
                self.evict(&s.id).await;
                let health = self.peek_health(&s.id).await;
                Ok(Some(McpServerStore::to_response(&s, health)))
            }
            None => Ok(None),
        }
    }

    pub async fn delete_server(
        &self,
        id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Result<bool, AppError> {
        // 归属校验
        match self.store.get_by_id(id).await? {
            Some(s) if Self::allows(&s, user_id, is_admin) => {}
            Some(_) => return Ok(false),
            None => return Ok(false),
        }
        self.evict(id).await;
        self.store.delete(id).await
    }

    /// 删除预检的归属校验：仅归属人/管理员可见影响清单
    pub async fn can_modify(&self, id: &str, user_id: &str, is_admin: bool) -> bool {
        match self.store.get_by_id(id).await {
            Ok(Some(s)) => Self::allows(&s, user_id, is_admin),
            _ => false,
        }
    }

    pub async fn list_servers(
        &self,
        user_id: &str,
        is_admin: bool,
    ) -> Result<Vec<McpServerResponse>, AppError> {
        let servers = self.store.list_for_owner(user_id, is_admin).await?;
        let mut out = Vec::with_capacity(servers.len());
        for s in servers {
            let health = if s.status == Status::Enabled {
                self.peek_health(&s.id).await
            } else {
                ServerHealth::Unknown
            };
            out.push(McpServerStore::to_response(&s, health));
        }
        Ok(out)
    }

    pub async fn list_servers_paged(
        &self,
        page: usize,
        page_size: usize,
        keyword: Option<&str>,
        user_id: &str,
        is_admin: bool,
    ) -> Result<(Vec<McpServerResponse>, i64), AppError> {
        let (servers, total) = self
            .store
            .list_paged(page, page_size, keyword, user_id, is_admin)
            .await?;
        let mut out = Vec::with_capacity(servers.len());
        for s in servers {
            let health = if s.status == Status::Enabled {
                self.peek_health(&s.id).await
            } else {
                ServerHealth::Unknown
            };
            out.push(McpServerStore::to_response(&s, health));
        }
        Ok((out, total))
    }

    pub async fn get_server(
        &self,
        id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Result<Option<McpServerResponse>, AppError> {
        let server = self.store.get_by_id(id).await?;
        match server {
            Some(s) if Self::allows(&s, user_id, is_admin) => {
                let health = if s.status == Status::Enabled {
                    self.peek_health(&s.id).await
                } else {
                    ServerHealth::Unknown
                };
                Ok(Some(McpServerStore::to_response(&s, health)))
            }
            _ => Ok(None),
        }
    }

    pub async fn batch_set_status(
        &self,
        ids: Option<&[String]>,
        keyword: Option<&str>,
        status_val: i16,
        user_id: &str,
        is_admin: bool,
    ) -> Result<usize, AppError> {
        match ids {
            Some(id_list) => {
                self.store
                    .set_status_batch(id_list, status_val, user_id, is_admin)
                    .await?;
                // 清理被改动的连接
                for id in id_list {
                    self.evict(id).await;
                }
                Ok(id_list.len())
            }
            None => {
                let affected = self
                    .store
                    .set_status_by_filter(keyword, status_val, user_id, is_admin)
                    .await?;
                // 清理所有连接（安全起见）
                self.evict_all().await;
                Ok(affected)
            }
        }
    }

    pub async fn batch_delete(
        &self,
        ids: Option<&[String]>,
        keyword: Option<&str>,
        user_id: &str,
        is_admin: bool,
    ) -> Result<usize, AppError> {
        match ids {
            Some(id_list) => {
                for id in id_list {
                    self.evict(id).await;
                }
                self.store.delete_batch(id_list, user_id, is_admin).await
            }
            None => {
                self.evict_all().await;
                self.store
                    .delete_by_filter(keyword, user_id, is_admin)
                    .await
            }
        }
    }
}
