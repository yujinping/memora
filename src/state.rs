//! 应用共享状态。
//!
//! @author yujinping
//! @intent P2：状态从「元库连接」扩展为「元库连接 + 存储后端注册表」，
//!          使中间件能在一次调用内完成「token → 项目 → 后端 → 仓库集合」的解析，
//!          业务 handler 不必再感知存储细节（见 docs §12.4）。
//! @intent P4.4：加入网页控制台的会话表。会话是**进程内**状态，不做持久化——
//!          服务重启即全部失效，等于强制重新认证（见 `session` 模块文档）。

use crate::config::Config;
use crate::error::ProjectResolveError;
use crate::meta;
use crate::session::SessionStore;
use crate::storage::{BackendKind, ProjectRepos, StorageRegistry};
use sea_orm::DatabaseConnection;
use std::sync::Arc;

/// Axum 共享状态：元库连接 + 运行配置 + 存储后端注册表 + 会话表。
#[derive(Clone)]
pub struct AppState {
    /// 全局元库连接（`_meta.db`）
    pub meta: DatabaseConnection,
    /// 运行配置
    pub config: Config,
    /// 可用存储后端注册表
    pub storage: Arc<StorageRegistry>,
    /// 网页控制台的会话表（进程内，不持久化）
    ///
    /// @intent 用 `Arc` 而非直接持有：`AppState` 每次 `clone` 都会复制字段，
    ///         会话表若被克隆成两份，登录写进 A、鉴权查 B，表现为随机掉登录态。
    pub sessions: Arc<SessionStore>,
}

impl AppState {
    /// 组装状态。
    ///
    /// @intent 提供构造函数而非让调用方逐个字段拼装：状态每加一个字段，字面量构造的
    ///         调用点就全部要改；而新增字段时「忘了在某处初始化」的后果往往是运行期
    ///         的偶然行为差异（例如会话表被克隆成两份），比编译错误难查得多。
    pub fn new(meta: DatabaseConnection, config: Config, storage: StorageRegistry) -> Self {
        Self {
            meta,
            config,
            storage: Arc::new(storage),
            sessions: Arc::new(SessionStore::new()),
        }
    }

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
