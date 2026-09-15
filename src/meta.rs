//! 元库（`_meta.db`）初始化、迁移与项目登记查询。
//!
//! @author yujinping
//! @intent P2：项目登记从「只有 token / 路径」扩展为「带存储后端标识」，
//!          使不同项目可并存不同持久化策略（见 docs §12.5）。
//!          迁移对既有库保持兼容：缺列时补列并回填默认值。

use crate::auth::sha256_hex;
use crate::entity::project::{ActiveModel, Column, Entity as Projects};
use crate::error::AuthError;
use sea_orm::prelude::*;
use sea_orm::ActiveValue::Set;
use sea_orm::{ConnectionTrait, DbBackend, QueryOrder, Statement};
use std::time::{SystemTime, UNIX_EPOCH};

/// 项目登记表默认后端标识（与 `BackendKind::SqliteFile` 一致）。
///
/// @intent 建表默认值与迁移回填值共用，避免两处字面量漂移。
pub const DEFAULT_BACKEND: &str = "sqlite_file";

/// 项目登记项。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectRecord {
    /// 项目标识
    pub project_id: String,
    /// 项目库相对路径（仅作记录，实际位置由存储后端推导）
    pub db_path: String,
    /// 该项目使用的存储后端标识
    pub backend: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
}

/// 新建项目入参。
///
/// @intent 由管理面（`admin::create_project`）构造；token 在此之前已由调用方生成，
///         本结构只负责把「明文 token + 登记元信息」一并交给入库逻辑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    /// 项目标识（须满足 `validate_project_id` 规则）
    pub project_id: String,
    /// 明文 token（入库前散列）
    pub token: String,
    /// 项目库相对路径（记录用）
    pub db_path: String,
    /// 存储后端标识
    pub backend: String,
}

/// 建表 + 迁移（幂等：可对全新库与 P1 遗留库重复执行）。
pub async fn init_meta_db(db: &DatabaseConnection) -> anyhow::Result<()> {
    // SQL 一律由 DEFAULT_BACKEND 插值，使「默认后端」在全库只有一处字面量；
    // 该常量是内部固定值，不来自用户输入，无注入面。
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS projects (\
        project_id TEXT PRIMARY KEY, \
        token_hash TEXT NOT NULL, \
        db_path TEXT NOT NULL, \
        created_at INTEGER NOT NULL, \
        backend TEXT NOT NULL DEFAULT '{DEFAULT_BACKEND}'\
    );"
    );
    db.execute(Statement::from_string(DbBackend::Sqlite, sql))
        .await?;
    ensure_backend_column(db).await?;
    Ok(())
}

/// 为 P1 时期建立的库补上 `backend` 列（SQLite 的 ADD COLUMN 对既有行回填默认值）。
async fn ensure_backend_column(db: &DatabaseConnection) -> anyhow::Result<()> {
    let rows = db
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA table_info(projects)",
        ))
        .await?;

    let has_backend = rows.iter().any(|row| {
        row.try_get::<String>("", "name")
            .map(|name| name == "backend")
            .unwrap_or(false)
    });

    if !has_backend {
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            format!("ALTER TABLE projects ADD COLUMN backend TEXT NOT NULL DEFAULT '{DEFAULT_BACKEND}'"),
        ))
        .await?;
    }
    Ok(())
}

/// 创建项目（明文 token 入库前做 SHA-256）。
///
/// @intent 明文 token 只存在于调用栈与响应体，库内仅留摘要——因此 token 一旦丢失
///         只能重建项目，不存在「查回来」的路径。
pub async fn create_project(db: &DatabaseConnection, new: &NewProject) -> anyhow::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Projects::insert(ActiveModel {
        project_id: Set(new.project_id.clone()),
        token_hash: Set(sha256_hex(&new.token)),
        db_path: Set(new.db_path.clone()),
        created_at: Set(now),
        backend: Set(new.backend.clone()),
    })
    .exec(db)
    .await?;
    Ok(())
}

/// 依据明文 token 解析项目登记项：SHA-256 后匹配 token_hash。
pub async fn resolve_project_by_token(
    db: &DatabaseConnection,
    token: &str,
) -> Result<ProjectRecord, AuthError> {
    let hash = sha256_hex(token);
    let row = Projects::find()
        .filter(Column::TokenHash.eq(hash))
        .one(db)
        .await
        .map_err(|_| AuthError::InvalidToken)?;
    row.map(record_of).ok_or(AuthError::InvalidToken)
}

/// 全部项目登记项（管理接口用），按 project_id 升序。
pub async fn list_projects(db: &DatabaseConnection) -> anyhow::Result<Vec<ProjectRecord>> {
    let rows = Projects::find()
        .order_by_asc(Column::ProjectId)
        .all(db)
        .await?;
    Ok(rows.into_iter().map(record_of).collect())
}

/// 按 project_id 查单个项目登记项；不存在返回 `None`。
///
/// @intent 管理面（P4）需要区分「项目不存在（404）」与「服务端失败（500）」，
///         故查询与错误必须可分离——`list_projects` 无法表达「不存在」这一事实。
pub async fn find_project(
    db: &DatabaseConnection,
    project_id: &str,
) -> anyhow::Result<Option<ProjectRecord>> {
    let row = Projects::find()
        .filter(Column::ProjectId.eq(project_id))
        .one(db)
        .await?;
    Ok(row.map(record_of))
}

/// 删除项目登记行，返回是否确有该行。
///
/// @intent 返回值而非 `Result<()>`：删除是幂等的，调用方需要知道「本来就没有」与
///         「确实删掉了」的区别（前者在管理面应报 404）。只删登记行，不动项目库文件——
///         物理数据的清理属存储后端的职责（`StorageBackend::drop_project`）。
pub async fn delete_project(db: &DatabaseConnection, project_id: &str) -> anyhow::Result<bool> {
    let result = Projects::delete_by_id(project_id.to_string()).exec(db).await?;
    Ok(result.rows_affected > 0)
}

/// 实体模型 → 领域登记项。
fn record_of(model: crate::entity::project::Model) -> ProjectRecord {
    ProjectRecord {
        project_id: model.project_id,
        db_path: model.db_path,
        backend: model.backend,
        created_at: model.created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Database;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 临时元库。
    async fn temp_meta() -> DatabaseConnection {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "memora_p2_meta_{}_{}.db",
            std::process::id(),
            n
        ));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        Database::connect(&url).await.unwrap()
    }

    fn new_project(id: &str, token: &str, backend: &str) -> NewProject {
        NewProject {
            project_id: id.to_string(),
            token: token.to_string(),
            db_path: format!("data/{id}/mem.db"),
            backend: backend.to_string(),
        }
    }

    /// 默认后端常量必须与 `BackendKind` 的解析结果一致——否则建表默认值会与运行期
    /// 后端注册表脱节，表现为「新项目登记了一个不存在的后端」。
    #[test]
    fn default_backend_matches_backend_kind() {
        assert_eq!(
            crate::storage::BackendKind::parse(DEFAULT_BACKEND),
            Ok(crate::storage::BackendKind::SqliteFile)
        );
    }

    /// 列名集合（用于断言迁移结果）。
    async fn columns(db: &DatabaseConnection) -> Vec<String> {
        let rows = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA table_info(projects)",
            ))
            .await
            .unwrap();
        rows.iter()
            .map(|r| r.try_get::<String>("", "name").unwrap())
            .collect()
    }

    /// P1 遗留库（无 backend 列）升级后应补列并回填默认值，且可重复执行。
    #[tokio::test]
    async fn init_meta_db_migrates_legacy_table() {
        let db = temp_meta().await;
        // 模拟 P1 时期的表结构与既有数据
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            "CREATE TABLE projects (\
                project_id TEXT PRIMARY KEY, \
                token_hash TEXT NOT NULL, \
                db_path TEXT NOT NULL, \
                created_at INTEGER NOT NULL\
            )",
        ))
        .await
        .unwrap();
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            format!(
                "INSERT INTO projects(project_id, token_hash, db_path, created_at) \
                 VALUES('legacy', '{}', 'data/legacy/mem.db', 1700000000)",
                sha256_hex("legacy-token")
            ),
        ))
        .await
        .unwrap();
        assert!(!columns(&db).await.contains(&"backend".to_string()));

        init_meta_db(&db).await.unwrap();

        assert!(columns(&db).await.contains(&"backend".to_string()));
        let record = resolve_project_by_token(&db, "legacy-token").await.unwrap();
        assert_eq!(record.backend, DEFAULT_BACKEND, "既有行应回填默认后端");
        assert_eq!(record.db_path, "data/legacy/mem.db", "既有数据不得被破坏");

        // 幂等：再次执行不应报「duplicate column」之类的错误
        init_meta_db(&db).await.unwrap();
    }

    /// 新库应一次建成，且登记的后端可被读回。
    #[tokio::test]
    async fn create_project_records_backend_and_resolves_by_token() {
        let db = temp_meta().await;
        init_meta_db(&db).await.unwrap();

        create_project(&db, &new_project("demo", "s3cret", "in_mem"))
            .await
            .unwrap();

        let record = resolve_project_by_token(&db, "s3cret").await.unwrap();
        assert_eq!(record.project_id, "demo");
        assert_eq!(record.backend, "in_mem");
        assert!(record.created_at > 0);
    }

    /// 未知 token 必须报 InvalidToken，且不泄漏「项目是否存在」的信息。
    #[tokio::test]
    async fn resolve_unknown_token_is_invalid() {
        let db = temp_meta().await;
        init_meta_db(&db).await.unwrap();
        create_project(&db, &new_project("demo", "s3cret", DEFAULT_BACKEND))
            .await
            .unwrap();

        assert_eq!(
            resolve_project_by_token(&db, "wrong").await,
            Err(AuthError::InvalidToken)
        );
    }

    /// 项目列表按 project_id 升序。
    #[tokio::test]
    async fn list_projects_is_sorted() {
        let db = temp_meta().await;
        init_meta_db(&db).await.unwrap();
        create_project(&db, &new_project("zeta", "tz", DEFAULT_BACKEND))
            .await
            .unwrap();
        create_project(&db, &new_project("alpha", "ta", DEFAULT_BACKEND))
            .await
            .unwrap();

        let list = list_projects(&db).await.unwrap();
        let ids: Vec<&str> = list.iter().map(|p| p.project_id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
    }

    /// 单项目查询：命中返回登记项，未命中必须返回 `None`（而非报错）。
    #[tokio::test]
    async fn find_project_distinguishes_missing_from_error() {
        let db = temp_meta().await;
        init_meta_db(&db).await.unwrap();
        create_project(&db, &new_project("demo", "s3cret", "in_mem"))
            .await
            .unwrap();

        let found = find_project(&db, "demo").await.unwrap().expect("应命中");
        assert_eq!(found.backend, "in_mem");
        assert_eq!(found.db_path, "data/demo/mem.db");

        assert!(
            find_project(&db, "ghost").await.unwrap().is_none(),
            "未命中应为 None"
        );
    }

    /// 删除登记行：首次为 true，重复删除为 false（幂等且不报错）。
    #[tokio::test]
    async fn delete_project_reports_whether_a_row_existed() {
        let db = temp_meta().await;
        init_meta_db(&db).await.unwrap();
        create_project(&db, &new_project("demo", "s3cret", DEFAULT_BACKEND))
            .await
            .unwrap();

        assert!(delete_project(&db, "demo").await.unwrap());
        assert!(find_project(&db, "demo").await.unwrap().is_none());
        assert!(
            resolve_project_by_token(&db, "s3cret").await.is_err(),
            "登记行删除后原 token 必须立即失效"
        );

        assert!(
            !delete_project(&db, "demo").await.unwrap(),
            "重复删除应返回 false"
        );
        assert!(!delete_project(&db, "never-existed").await.unwrap());
    }

    /// 删除只影响目标项目，不得波及其他项目。
    #[tokio::test]
    async fn delete_project_leaves_other_projects_untouched() {
        let db = temp_meta().await;
        init_meta_db(&db).await.unwrap();
        for (id, token) in [("keep", "tk"), ("drop", "td")] {
            create_project(&db, &new_project(id, token, DEFAULT_BACKEND))
                .await
                .unwrap();
        }

        assert!(delete_project(&db, "drop").await.unwrap());

        let ids: Vec<String> = list_projects(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.project_id)
            .collect();
        assert_eq!(ids, vec!["keep".to_string()]);
        assert_eq!(
            resolve_project_by_token(&db, "tk").await.unwrap().project_id,
            "keep"
        );
    }
}
