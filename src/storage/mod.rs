//! 存储抽象层：后端枚举、仓库工厂与依赖注入载体。
//!
//! @author yujinping
//! @intent P2：以 Strategy + Abstract Factory 组织持久化后端。业务层（MCP / REST / 路由）
//!          只依赖 `MemoryRepository` trait；后端的构造与选择全部收敛在本模块的
//!          `StorageRegistry` 一处（见 docs §12.2 / §12.4）。
//!
//! 注：P2 阶段仓库的**写侧**方法仅由测试驱动，P3 的 MCP 工具层接入后已全部成为
//! 生产路径，模块级 `allow(dead_code)` 已移除。
pub mod mem;
pub mod normalize;
pub mod repo;
pub mod sqlite_file;

/// 常用类型的扁平重导出，便于上层 `use crate::storage::{...}` 一把取用。
pub use mem::InMemBackend;
pub use repo::MemoryRepository;
pub use sqlite_file::SqliteFileBackend;

#[cfg(test)]
pub mod contract;

use crate::config::Config;
use crate::error::StorageError;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

/// 持久化后端类型标识。
///
/// @intent 该值会以字符串形式存入 `_meta.db.projects.backend`，故取值不可随意变更。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// 每项目一个 SQLite 文件（默认，默认路径 `data/{project_id}/mem.db`）
    SqliteFile,
    /// 单 SQLite 文件 + 租户列隔离（P6）
    SqliteSingle,
    /// Postgres（P6），schema 或租户列隔离
    Postgres,
    /// 纯内存，仅供测试与临时验证
    InMem,
}

impl BackendKind {
    /// 全部已知后端类型。
    ///
    /// @intent 存在意义是「穷尽性测试的锚点」：新增枚举变体后，用它驱动的 round-trip 用例
    ///         会立即暴露漏配的标识解析，故只在测试构建中需要。
    #[cfg(test)]
    pub const ALL: [BackendKind; 4] = [
        BackendKind::SqliteFile,
        BackendKind::SqliteSingle,
        BackendKind::Postgres,
        BackendKind::InMem,
    ];

    /// 入库 / 配置使用的字符串标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendKind::SqliteFile => "sqlite_file",
            BackendKind::SqliteSingle => "sqlite_single",
            BackendKind::Postgres => "postgres",
            BackendKind::InMem => "in_mem",
        }
    }

    /// 从字符串标识解析；未知标识返回 `Unsupported`。
    pub fn parse(s: &str) -> Result<Self, StorageError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sqlite_file" => Ok(BackendKind::SqliteFile),
            "sqlite_single" => Ok(BackendKind::SqliteSingle),
            "postgres" => Ok(BackendKind::Postgres),
            "in_mem" => Ok(BackendKind::InMem),
            other => Err(StorageError::Unsupported(other.to_string())),
        }
    }
}

/// 某项目的仓库集合。
///
/// @intent 中间件解析出项目后一次性产出，经请求扩展注入；handler 只从扩展取用。
///         `backend` 字段使 handler 无需再回查元库即可在诊断信息 / 响应中标注存储策略。
///         P5 新增 `ConversationRepository` 时在此追加字段即可，对已有 handler 无影响。
#[derive(Clone)]
pub struct ProjectRepos {
    /// 项目标识
    pub project_id: String,
    /// 该项目实际使用的存储后端
    pub backend: BackendKind,
    /// 记忆仓库
    pub memory: Arc<dyn MemoryRepository>,
}

/// 存储后端：按 project_id 产出仓库集合。
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// 后端类型标识。
    fn kind(&self) -> BackendKind;

    /// 取某项目的仓库集合；实现方需保证幂等（首次访问自动建库 / 建表）。
    async fn repositories_for(&self, project_id: &str) -> Result<ProjectRepos, StorageError>;

    /// 丢弃某项目：释放其资源并删除持久化数据（幂等，项目从未落盘视为成功）。
    ///
    /// @intent `DELETE /api/v1/projects/{id}` 的物理落地，必须由后端实现而非公共代码——
    ///         「数据放在哪」是后端知识：文件后端须先摘除已打开的连接缓存再删目录，
    ///         内存后端只需丢弃对应条目。留一个必须实现的 trait 方法（而非默认空实现），
    ///         可保证新增后端时无法「忘记」清理逻辑，并强制其接入后端级契约测试。
    async fn drop_project(&self, project_id: &str) -> Result<(), StorageError>;
}

/// 后端注册表：运行期可用的后端实现集合。
///
/// @intent 支持「不同项目并存不同存储策略」——项目用哪种后端由其 `projects.backend`
///         登记值决定，本注册表负责把该值解析为具体实现（见 docs §12.5）。
pub struct StorageRegistry {
    /// 已注册后端：类型 → 实现
    backends: HashMap<BackendKind, Arc<dyn StorageBackend>>,
}

impl StorageRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self {
            backends: HashMap::new(),
        }
    }

    /// 按本构建实际可用的后端装配注册表，并校验配置中的默认后端确实可用。
    ///
    /// @intent 启动即失败优于运行期 500：`STORAGE_BACKEND` 指向本构建未编译的后端时直接报错。
    pub fn from_config(config: &Config) -> Result<Self, StorageError> {
        let mut registry = Self::new();

        // 本构建可用的实现：SQLite 文件后端 + 内存后端（测试 / 临时验证）
        registry.register(Arc::new(SqliteFileBackend::new(config.data_dir.clone())));
        registry.register(Arc::new(InMemBackend::new()));

        // 配置指定的默认后端必须已在注册表中
        let default_kind = BackendKind::parse(&config.storage_backend)?;
        registry.get(default_kind)?;

        Ok(registry)
    }

    /// 注册一个后端实现（同类型重复注册时后注册者覆盖）。
    pub fn register(&mut self, backend: Arc<dyn StorageBackend>) {
        self.backends.insert(backend.kind(), backend);
    }

    /// 取指定类型的后端；未注册返回 `Unsupported`。
    pub fn get(&self, kind: BackendKind) -> Result<Arc<dyn StorageBackend>, StorageError> {
        self.backends
            .get(&kind)
            .cloned()
            .ok_or_else(|| StorageError::Unsupported(kind.as_str().to_string()))
    }

    /// 已注册的后端类型（升序，便于日志输出稳定）。
    pub fn available_kinds(&self) -> Vec<BackendKind> {
        let mut kinds: Vec<BackendKind> = self.backends.keys().copied().collect();
        kinds.sort_by_key(|k| k.as_str());
        kinds
    }
}

impl Default for StorageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// 校验 project_id 可用作目录名，防止越出 DATA_DIR。
///
/// @intent 纵深防御：即便 P4 的 REST 层做了校验，存储层仍拒绝 `..`、路径分隔符等
///         可能把文件写到 DATA_DIR 之外的值。
pub fn validate_project_id(project_id: &str) -> Result<(), StorageError> {
    let bad = |p: &str| Err(StorageError::InvalidProjectId(p.to_string()));

    if project_id.is_empty() || project_id.len() > 64 {
        return bad(project_id);
    }
    if !project_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return bad(project_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(backend: &str) -> Config {
        Config::for_test(0, ".", "admin", backend)
    }

    /// `Arc<dyn StorageBackend>` 不含 Debug，故以匹配方式断言错误类型。
    fn assert_unsupported<T>(result: Result<T, StorageError>, expected: &str) {
        match result {
            Err(StorageError::Unsupported(k)) => assert_eq!(k, expected),
            Err(other) => panic!("应报 Unsupported，实际为 {other:?}"),
            Ok(_) => panic!("应报 Unsupported，但返回成功"),
        }
    }

    #[test]
    fn project_id_accepts_safe_values() {
        assert!(validate_project_id("demo").is_ok());
        assert!(validate_project_id("demo-1_x").is_ok());
        assert!(validate_project_id("A1").is_ok());
    }

    #[test]
    fn project_id_rejects_traversal_and_separators() {
        assert_eq!(
            validate_project_id("../evil"),
            Err(StorageError::InvalidProjectId("../evil".to_string()))
        );
        assert!(validate_project_id("").is_err());
        assert!(validate_project_id("a/b").is_err());
        assert!(validate_project_id("a.b").is_err());
        assert!(validate_project_id("a b").is_err());
        assert!(validate_project_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn backend_kind_parse_roundtrip() {
        for kind in BackendKind::ALL {
            assert_eq!(BackendKind::parse(kind.as_str()), Ok(kind));
        }
        assert_eq!(
            BackendKind::parse("mysql"),
            Err(StorageError::Unsupported("mysql".to_string()))
        );
    }

    #[test]
    fn registry_returns_unsupported_for_missing_kind() {
        let mut registry = StorageRegistry::new();
        registry.register(Arc::new(mem::InMemBackend::new()));

        assert_eq!(
            registry.get(BackendKind::InMem).unwrap().kind(),
            BackendKind::InMem
        );
        assert_unsupported(registry.get(BackendKind::Postgres), "postgres");
    }

    #[test]
    fn registry_available_kinds_is_sorted() {
        let registry = StorageRegistry::from_config(&cfg("sqlite_file")).unwrap();
        let kinds: Vec<&str> = registry
            .available_kinds()
            .iter()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(kinds, vec!["in_mem", "sqlite_file"]);
    }

    #[test]
    fn registry_rejects_unavailable_default_backend() {
        assert!(StorageRegistry::from_config(&cfg("sqlite_file")).is_ok());
        assert!(StorageRegistry::from_config(&cfg("in_mem")).is_ok());
        assert_unsupported(
            StorageRegistry::from_config(&cfg("postgres")),
            "postgres",
        );
        assert_unsupported(StorageRegistry::from_config(&cfg("mysql")), "mysql");
    }
}
