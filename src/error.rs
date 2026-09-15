//! 错误类型：鉴权错误与存储错误。
//!
//! @author yujinping
//! @intent P2：将「存储后端失败」与「鉴权失败」分离。存储层对外只暴露 `StorageError`，
//!         底层 sea-orm / SQLite 的错误被收敛为 `StorageError::Backend`，
//!         使业务层不依赖任何具体驱动（详见 docs §12.4）。

/// 鉴权失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// 请求缺少 / 格式错误的 Authorization 头。
    MissingToken,
    /// token 无效或越权。
    InvalidToken,
}

/// 存储层统一错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    /// 入参非法（空实体名、空观测内容等）。
    InvalidInput(String),
    /// 目标实体不存在（追加观测时实体必须已存在）。
    EntityNotFound(String),
    /// project_id 非法（空、含路径分隔符或 `..`，可能越出 DATA_DIR）。
    InvalidProjectId(String),
    /// 后端类型不存在或本构建未编译进该后端。
    Unsupported(String),
    /// 底层存储错误（sea-orm / SQLite），仅用于诊断，不做分支判断。
    Backend(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::InvalidInput(m) => write!(f, "invalid input: {m}"),
            StorageError::EntityNotFound(n) => write!(f, "entity not found: {n}"),
            StorageError::InvalidProjectId(p) => write!(f, "invalid project id: {p}"),
            StorageError::Unsupported(k) => write!(f, "unsupported storage backend: {k}"),
            StorageError::Backend(m) => write!(f, "storage backend error: {m}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<sea_orm::DbErr> for StorageError {
    fn from(e: sea_orm::DbErr) -> Self {
        StorageError::Backend(e.to_string())
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Backend(e.to_string())
    }
}

/// 项目解析失败：区分「凭据问题（401）」与「服务端问题（500）」。
///
/// @intent 中间件必须在两种失败之间给出不同状态码：token 无效是客户端问题，
///         后端未注册 / 建库失败是服务端问题，混为一谈会掩盖故障。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectResolveError {
    /// token 缺失或无效。
    Auth(AuthError),
    /// 存储后端不可用。
    Storage(StorageError),
}

impl std::fmt::Display for ProjectResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectResolveError::Auth(e) => write!(f, "auth failed: {e:?}"),
            ProjectResolveError::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProjectResolveError {}

impl From<AuthError> for ProjectResolveError {
    fn from(e: AuthError) -> Self {
        ProjectResolveError::Auth(e)
    }
}

impl From<StorageError> for ProjectResolveError {
    fn from(e: StorageError) -> Self {
        ProjectResolveError::Storage(e)
    }
}
