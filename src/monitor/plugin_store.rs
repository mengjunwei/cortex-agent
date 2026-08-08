use crate::error::AppError;
use crate::infra::db::DbPool;
use crate::infra::store_base::Store;
use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;

#[derive(Debug, Clone, QueryableByName, serde::Serialize)]
pub struct PluginRow {
    #[diesel(sql_type = sql_types::Int4)]
    pub id: i32,
    #[diesel(sql_type = sql_types::Varchar)]
    pub plugin_id: String,
    #[diesel(sql_type = sql_types::Text)]
    pub description: String,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int4>)]
    pub active_version: Option<i32>,
    #[diesel(sql_type = sql_types::Bool)]
    pub enabled: bool,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, QueryableByName, serde::Serialize)]
pub struct VersionRow {
    #[diesel(sql_type = sql_types::Int4)]
    pub id: i32,
    #[diesel(sql_type = sql_types::Varchar)]
    pub plugin_id: String,
    #[diesel(sql_type = sql_types::Int4)]
    pub version: i32,
    #[diesel(sql_type = sql_types::Text)]
    pub source_code: String,
    #[diesel(sql_type = sql_types::Text)]
    pub change_description: String,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, QueryableByName)]
pub struct ActivePluginRow {
    #[diesel(sql_type = sql_types::Varchar)]
    pub plugin_id: String,
    #[diesel(sql_type = sql_types::Int4)]
    pub version: i32,
    #[diesel(sql_type = sql_types::Text)]
    pub source_code: String,
}

pub struct PluginStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for PluginStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl PluginStore {
    pub async fn new(pool: DbPool) -> Result<Self, AppError> {
        Ok(Self { pool })
    }

    /// 注册插件：插入/更新主表（含描述）+ 插入版本（含变更说明）+ 设置激活版本（同一连接内完成）
    ///
    /// - `description`：插件整体描述，写入主表（首次发布时尤为重要）
    /// - `change_description`：本次发版的变更说明，写入版本表
    pub async fn register_plugin(
        &self,
        plugin_id: &str,
        description: &str,
        source_code: &str,
        version: i32,
        change_description: &str,
    ) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;

        let desc_to_write = if description.is_empty() {
            None
        } else {
            Some(description)
        };

        diesel::sql_query(
            r#"
            INSERT INTO monitor_plugins (plugin_id, description, updated_at)
            VALUES ($1, COALESCE($2, ''), NOW())
            ON CONFLICT (plugin_id) DO UPDATE SET
                description = COALESCE($2, monitor_plugins.description),
                updated_at  = NOW()
            "#,
        )
        .bind::<sql_types::Text, _>(plugin_id)
        .bind::<sql_types::Nullable<sql_types::Text>, _>(desc_to_write)
        .execute(&mut conn)
        .await?;

        diesel::sql_query(
            r#"
            INSERT INTO monitor_plugin_versions (plugin_id, version, source_code, change_description)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (plugin_id, version) DO UPDATE SET
                source_code = EXCLUDED.source_code,
                change_description = EXCLUDED.change_description
            "#,
        )
        .bind::<sql_types::Text, _>(plugin_id)
        .bind::<sql_types::Int4, _>(version)
        .bind::<sql_types::Text, _>(source_code)
        .bind::<sql_types::Text, _>(change_description)
        .execute(&mut conn)
        .await?;

        diesel::sql_query(
            "UPDATE monitor_plugins SET active_version = $2, enabled = TRUE, updated_at = NOW() WHERE plugin_id = $1",
        )
        .bind::<sql_types::Text, _>(plugin_id)
        .bind::<sql_types::Int4, _>(version)
        .execute(&mut conn)
        .await?;

        Ok(())
    }

    pub async fn set_active_version(&self, plugin_id: &str, version: i32) -> Result<(), AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query(
            r#"
            UPDATE monitor_plugins
            SET active_version = $2, enabled = TRUE, updated_at = NOW()
            WHERE plugin_id = $1
            "#,
        )
        .bind::<sql_types::Text, _>(plugin_id)
        .bind::<sql_types::Int4, _>(version)
        .execute(&mut conn)
        .await?;
        Ok(())
    }

    pub async fn disable_plugin(&self, plugin_id: &str) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            "UPDATE monitor_plugins SET enabled = FALSE, updated_at = NOW() WHERE plugin_id = $1",
        )
        .bind::<sql_types::Text, _>(plugin_id)
        .execute(&mut conn)
        .await?;
        Ok(rows > 0)
    }

    pub async fn delete_plugin(&self, plugin_id: &str) -> Result<bool, AppError> {
        let mut conn = self.get_conn().await?;
        diesel::sql_query("DELETE FROM monitor_plugin_versions WHERE plugin_id = $1")
            .bind::<sql_types::Text, _>(plugin_id)
            .execute(&mut conn)
            .await?;
        let rows = diesel::sql_query("DELETE FROM monitor_plugins WHERE plugin_id = $1")
            .bind::<sql_types::Text, _>(plugin_id)
            .execute(&mut conn)
            .await?;
        Ok(rows > 0)
    }

    pub async fn list_plugins(&self) -> Result<Vec<PluginRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT id, plugin_id, description, active_version, enabled, created_at, updated_at
            FROM monitor_plugins
            ORDER BY created_at DESC
            "#,
        )
        .get_results::<PluginRow>(&mut conn)
        .await?;
        Ok(rows)
    }

    pub async fn get_plugin(&self, plugin_id: &str) -> Result<Option<PluginRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT id, plugin_id, description, active_version, enabled, created_at, updated_at
            FROM monitor_plugins WHERE plugin_id = $1
            "#,
        )
        .bind::<sql_types::Text, _>(plugin_id)
        .get_results::<PluginRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next())
    }

    pub async fn list_versions(&self, plugin_id: &str) -> Result<Vec<VersionRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT id, plugin_id, version, source_code, change_description, created_at
            FROM monitor_plugin_versions
            WHERE plugin_id = $1
            ORDER BY version DESC
            "#,
        )
        .bind::<sql_types::Text, _>(plugin_id)
        .get_results::<VersionRow>(&mut conn)
        .await?;
        Ok(rows)
    }

    pub async fn get_version(
        &self,
        plugin_id: &str,
        version: i32,
    ) -> Result<Option<VersionRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT id, plugin_id, version, source_code, change_description, created_at
            FROM monitor_plugin_versions
            WHERE plugin_id = $1 AND version = $2
            "#,
        )
        .bind::<sql_types::Text, _>(plugin_id)
        .bind::<sql_types::Int4, _>(version)
        .get_results::<VersionRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next())
    }

    pub async fn get_active_version(
        &self,
        plugin_id: &str,
    ) -> Result<Option<VersionRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT v.id, v.plugin_id, v.version, v.source_code, v.change_description, v.created_at
            FROM monitor_plugin_versions v
            INNER JOIN monitor_plugins p ON p.plugin_id = v.plugin_id
            WHERE p.plugin_id = $1 AND p.active_version = v.version AND p.enabled = TRUE
            "#,
        )
        .bind::<sql_types::Text, _>(plugin_id)
        .get_results::<VersionRow>(&mut conn)
        .await?;
        Ok(rows.into_iter().next())
    }

    pub async fn load_all_active_plugins(&self) -> Result<Vec<ActivePluginRow>, AppError> {
        let mut conn = self.get_conn().await?;
        let rows = diesel::sql_query(
            r#"
            SELECT p.plugin_id, v.version, v.source_code
            FROM monitor_plugins p
            INNER JOIN monitor_plugin_versions v
                ON p.plugin_id = v.plugin_id AND p.active_version = v.version
            WHERE p.enabled = TRUE AND p.active_version IS NOT NULL
            "#,
        )
        .get_results::<ActivePluginRow>(&mut conn)
        .await?;
        Ok(rows)
    }
}
