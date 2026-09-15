//! 应用共享状态。
//!
//! @author yujinping
//! @intent P2：状态从「元库连接」扩展为「元库连接 + 存储后端注册表」，
//!          使中间件能在一次调用内完成「token → 项目 → 后端 → 仓库集合」的解析，
//!          业务 handler 不必再感知存储细节（见 docs §12.4）。

use crate::config::Config;
use crate::error::ProjectResolveError;
use crate::meta;
use crate::storage::{BackendKind, ProjectRepos, StorageRegistry};
use sea_orm::DatabaseConnection;
use std::sync::Arc;

/// Axum 共享状态：元库连接 + 运行配置 + 存储后端注册表。
#[derive(Clone)]
pub struct AppState {
    /// 全局元库连接（`_meta.db`）
    pub meta: DatabaseConnection,
    /// 运行配置
    pub config: Config,
    /// 可用存储后端注册表
    pub storage: Arc<StorageRegistry>,
}

impl AppState {
    /// 依据 Bearer token 解析「项目 + 存储后端」并产出仓库集合。
    ///
    /// @intent 后端选择在此收口：项目用哪种存储由其 `projects.backend` 登记值决定，
    ///         与全局默认后端解耦；未注册的后端视为服务端错误而非鉴权失败。
    pub async fn repositories_for_token(
        &self,
        token: &str,
    ) -> Result<ProjectRepos, ProjectResolveError> {
        let record = meta::resolve_project_by_token(&self.meta, token).await?;
        let kind = BackendKind::parse(&record.backend)?;
        let backend = self.storage.get(kind)?;
        let repos = backend.repositories_for(&record.project_id).await?;
        Ok(repos)
    }
}
