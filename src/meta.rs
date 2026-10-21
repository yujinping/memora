//! 元库（`_meta.db`）初始化、迁移与项目登记查询。
//!
//! @author yujinping
//! @intent P2：项目登记从「只有 token / 路径」扩展为「带存储后端标识」，
//!          使不同项目可并存不同持久化策略（见 docs §12.5）。
//!          迁移对既有库保持兼容：缺列时补列并回填默认值。
//!
//! @intent P8（多租户）：`projects` 增 `owner` 列、`users` 增 `role` 列。两者都走
//!         同一套「缺列则补」的加列迁移，因此升级不需要独立的迁移步骤，
//!          也不需要停服——`init_meta_db` 在每次启动时跑，且是幂等的。
//!          存量数据的归属收敛由 [`claim_unowned_projects`] 完成（见其文档）。

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
    /// 归属账号；空串 = 无主（迁移前遗留，仅管理员可见）
    ///
    /// @intent 加 `serde(default)`：`ProjectRecord` 已实现 `Serialize`/`Deserialize`
    ///         并被交接 CLI 等外部调用方消费。老版本字段集反序列化新响应时缺这一列
    ///         不应报错，新版本读老响应时也应得到一个明确的「无主」而不是解析失败。
    #[serde(default)]
    pub owner: String,
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
    /// 归属账号（即创建者）
    ///
    /// @intent 归属在这里是**必填**而非 `Option`：新建一个无主项目在当前模型里
    ///         没有任何正当场景（无主只应作为迁移中间态出现），
    ///         让它是 `String` 就从类型上排除了「忘了传 owner」。
    pub owner: String,
}

/// 建表 + 迁移（幂等：可对全新库与 P1 遗留库重复执行）。
///
/// @intent `users` 表与 `projects` 表同库同生命周期：账号是控制台的凭据来源，
///         而 `_meta.db` 是唯一「一定存在」的库（它由启动流程保证）。把账号放进
///         项目库会让「没有项目时无法登录」——那恰好是首次部署的状态。
pub async fn init_meta_db(db: &DatabaseConnection) -> anyhow::Result<()> {
    // SQL 一律由 DEFAULT_BACKEND 插值，使「默认后端」在全库只有一处字面量；
    // 该常量是内部固定值，不来自用户输入，无注入面。
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS projects (\
        project_id TEXT PRIMARY KEY, \
        token_hash TEXT NOT NULL, \
        db_path TEXT NOT NULL, \
        created_at INTEGER NOT NULL, \
        backend TEXT NOT NULL DEFAULT '{DEFAULT_BACKEND}', \
        owner TEXT NOT NULL DEFAULT ''\
    );"
    );
    db.execute(Statement::from_string(DbBackend::Sqlite, sql))
        .await?;
    ensure_backend_column(db).await?;
    ensure_owner_column(db).await?;
    ensure_users_table(db).await?;
    ensure_role_column(db).await?;
    Ok(())
}

/// 建 `users` 表（P7 控制台账号）。
///
/// @intent 单独一个 `CREATE TABLE IF NOT EXISTS`，与 `projects` 并列而不是塞进上面
///         那条格式串：两张表没有关系，混在一条语句里只会让「加一张表」变成
///         「改一条长 SQL」。
async fn ensure_users_table(db: &DatabaseConnection) -> anyhow::Result<()> {
    db.execute(Statement::from_string(
        DbBackend::Sqlite,
        "CREATE TABLE IF NOT EXISTS users (\
            username TEXT PRIMARY KEY, \
            password_hash TEXT NOT NULL, \
            role TEXT NOT NULL DEFAULT 'admin', \
            created_at INTEGER NOT NULL, \
            updated_at INTEGER NOT NULL\
        )",
    ))
    .await?;
    Ok(())
}

/// 取某表的列名集合。
///
/// @intent 三处加列迁移共用：`PRAGMA table_info` 是 SQLite 唯一不依赖版本差异的
///         列枚举方式（`sqlite_master.sql` 需要自己解析 DDL 文本，一处引号风格
///         变化就会让判断失效）。
async fn columns_of(db: &DatabaseConnection, table: &str) -> anyhow::Result<Vec<String>> {
    let rows = db
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            format!("PRAGMA table_info({table})"),
        ))
        .await?;

    Ok(rows
        .iter()
        .filter_map(|row| row.try_get::<String>("", "name").ok())
        .collect())
}

/// 为 P1 时期建立的库补上 `backend` 列（SQLite 的 ADD COLUMN 对既有行回填默认值）。
async fn ensure_backend_column(db: &DatabaseConnection) -> anyhow::Result<()> {
    if !columns_of(db, "projects").await?.iter().any(|c| c == "backend") {
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            format!(
                "ALTER TABLE projects ADD COLUMN backend TEXT NOT NULL DEFAULT '{DEFAULT_BACKEND}'"
            ),
        ))
        .await?;
    }
    Ok(())
}

/// 为 P7 及更早的库补上 `projects.owner` 列（P8 多租户）。
///
/// @intent 回填 `''` 而不是回填某个用户名：迁移的这一刻**未必有账号**
///         （只用 `ADMIN_TOKEN` 的部署从不建控制台账号），SQLite 的 `ADD COLUMN`
///         也无法做「跨表取最大值」这种子查询默认值。因此这里只负责把列补上、
///         把存量标成「无主」，真正的认领交给 [`claim_unowned_projects`]——
///         它在账号存在时执行，且可重复调用。
async fn ensure_owner_column(db: &DatabaseConnection) -> anyhow::Result<()> {
    if !columns_of(db, "projects").await?.iter().any(|c| c == "owner") {
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            "ALTER TABLE projects ADD COLUMN owner TEXT NOT NULL DEFAULT ''",
        ))
        .await?;
    }
    Ok(())
}

/// 为 P7 时期建立的 `users` 表补上 `role` 列（P8 多租户）。
///
/// @intent 回填 `admin`：P7 时期的语义就是「凡能登录者皆管理员」，回填成别的值
///         会让线上唯一账号在升级瞬间失去管理权（且它连管理员都还不是，
///         无法自行修回）。这条回填是把旧语义**忠实**地带进新模型。
async fn ensure_role_column(db: &DatabaseConnection) -> anyhow::Result<()> {
    if !columns_of(db, "users").await?.iter().any(|c| c == "role") {
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            "ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'admin'",
        ))
        .await?;
    }
    Ok(())
}

/// 把「无主项目」认领给首个管理员，返回被认领的条数。
///
/// @intent 为什么需要这一步：`ensure_owner_column` 只能把存量留成空串，而空串在
///         权限模型里意味着「仅管理员可见」。若不认领，升级后管理员虽仍能看到
///         全部项目，但这些项目永远不会出现在任何非管理员视图里——看起来就像
///         「升级把数据弄丢了」。认领把它们明确地归给现有的管理员账号。
///
/// @intent 选「最早创建的 admin」而不是「当前登录者」：本函数在启动时执行，
///         此刻没有「当前登录者」这个概念。最早的管理员在几乎所有部署里就是
///         配置里播种的那个账号，与用户的直觉一致，且结果与执行时机无关
///         ——这一点很重要，认领的可重入性依赖于它。
///
/// @intent 没有任何管理员时**什么都不做**并返回 0：此时无主项目对管理员可见
///         （`ADMIN_TOKEN` 通路恒为管理员），系统仍然自洽；等第一个账号被建出来
///         时，下一次启动会补做认领。不在这里报错，是因为「只跑 MCP + 脚本、
///         从不建控制台账号」是一个被支持的部署形态。
pub async fn claim_unowned_projects(db: &DatabaseConnection) -> anyhow::Result<usize> {
    let Some(admin) = crate::users::first_admin(db).await? else {
        return Ok(0);
    };

    let affected = Projects::update_many()
        .col_expr(Column::Owner, Expr::value(admin))
        .filter(Column::Owner.eq(""))
        .exec(db)
        .await?
        .rows_affected;

    Ok(affected as usize)
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
        owner: Set(new.owner.clone()),
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
///
/// @intent 这是**全量**视图，只有管理员能走这个入口。租户请用
///         [`list_projects_of`]——把过滤责任留给调用方是这里最容易出事的地方：
///         一旦某个 handler 忘了传 owner，越权就变成了默认行为。
///         故 `admin` 模块统一经 `visible_projects` 取数，不直接调用本函数。
pub async fn list_projects(db: &DatabaseConnection) -> anyhow::Result<Vec<ProjectRecord>> {
    let rows = Projects::find()
        .order_by_asc(Column::ProjectId)
        .all(db)
        .await?;
    Ok(rows.into_iter().map(record_of).collect())
}

/// 某账号名下的项目，按 project_id 升序。
///
/// @intent 过滤下推到 SQL 而非「取全量后在内存里筛」：后者在管理员视角下等价，
///         但一旦有人复用它做租户视图，就变成「先把别人的数据读进内存再丢掉」
///         ——功能上不错，却让「越权读取」从「代码里没有这个查询」退化成
///         「查询了但不展示」，那是两种可信度完全不同的保证。
pub async fn list_projects_of(
    db: &DatabaseConnection,
    owner: &str,
) -> anyhow::Result<Vec<ProjectRecord>> {
    let rows = Projects::find()
        .filter(Column::Owner.eq(owner))
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

/// 更新项目 token 的摘要，返回项目是否存在。
///
/// @intent 「轮换 token」是控制台里「token 丢了 / 疑似泄漏」的**唯一出路**：
///         库内只有摘要，读不回来，只能换一把新的。返回 `bool` 让 handler 区分
///         「项目不存在（404）」与「已轮换（200）」——静默成功会让界面上显示一个
///         实际没生效的新 token。
///
/// @intent 只改 `token_hash`，不动 `db_path` 与 `backend`：轮换凭据不该影响
///         数据的存放位置，否则一次「换 token」会连带把项目迁移到别处。
pub async fn update_token(
    db: &DatabaseConnection,
    project_id: &str,
    token: &str,
) -> anyhow::Result<bool> {
    let affected = Projects::update_many()
        .col_expr(Column::TokenHash, Expr::value(sha256_hex(token)))
        .filter(Column::ProjectId.eq(project_id))
        .exec(db)
        .await?
        .rows_affected;
    Ok(affected > 0)
}

/// 删除项目登记行，返回是否确有该行。
///
/// @intent 返回值而非 `Result<()>`：删除是幂等的，调用方需要知道「本来就没有」与
///         「确实删掉了」的区别（前者在管理面应报 404）。只删登记行，不动项目库文件——
///         物理数据的清理属存储后端的职责（`StorageBackend::drop_project`）。
pub async fn delete_project(db: &DatabaseConnection, project_id: &str) -> anyhow::Result<bool> {
    let result = Projects::delete_by_id(project_id.to_string())
        .exec(db)
        .await?;
    Ok(result.rows_affected > 0)
}

/// 实体模型 → 领域登记项。
fn record_of(model: crate::entity::project::Model) -> ProjectRecord {
    ProjectRecord {
        project_id: model.project_id,
        db_path: model.db_path,
        backend: model.backend,
        created_at: model.created_at,
        owner: model.owner,
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
        let path =
            std::env::temp_dir().join(format!("memora_p2_meta_{}_{}.db", std::process::id(), n));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        Database::connect(&url).await.unwrap()
    }

    fn new_project(id: &str, token: &str, backend: &str) -> NewProject {
        NewProject {
            project_id: id.to_string(),
            token: token.to_string(),
            db_path: format!("data/{id}/mem.db"),
            backend: backend.to_string(),
            owner: "alice".to_string(),
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
            resolve_project_by_token(&db, "tk")
                .await
                .unwrap()
                .project_id,
            "keep"
        );
    }

    // ------------------------------------------------- P8 多租户迁移与归属

    /// 指定归属的项目构造函数。
    fn owned(id: &str, token: &str, owner: &str) -> NewProject {
        NewProject {
            owner: owner.to_string(),
            ..new_project(id, token, DEFAULT_BACKEND)
        }
    }

    /// P7 遗留库（projects 无 owner、users 无 role）升级后：
    /// 两张表都补列、role 回填 admin、owner 先留空再由认领填给管理员。
    #[tokio::test]
    async fn legacy_database_gains_ownership_columns_and_role() {
        let db = temp_meta().await;

        // 模拟 P7 时期的 projects（无 owner）
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            "CREATE TABLE projects (\
                project_id TEXT PRIMARY KEY, \
                token_hash TEXT NOT NULL, \
                db_path TEXT NOT NULL, \
                created_at INTEGER NOT NULL, \
                backend TEXT NOT NULL DEFAULT 'sqlite_file'\
            )",
        ))
        .await
        .unwrap();
        // 与 P7 时期的 users（无 role）
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            "CREATE TABLE users (\
                username TEXT PRIMARY KEY, \
                password_hash TEXT NOT NULL, \
                created_at INTEGER NOT NULL, \
                updated_at INTEGER NOT NULL\
            )",
        ))
        .await
        .unwrap();
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            "INSERT INTO users VALUES('root', 'x', 1700000000, 1700000000)",
        ))
        .await
        .unwrap();
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            format!(
                "INSERT INTO projects(project_id, token_hash, db_path, created_at, backend) \
                 VALUES('legacy', '{}', 'data/legacy/mem.db', 1700000000, 'sqlite_file')",
                sha256_hex("legacy-token")
            ),
        ))
        .await
        .unwrap();

        init_meta_db(&db).await.unwrap();

        assert!(columns_of(&db, "projects")
            .await
            .unwrap()
            .contains(&"owner".to_string()));
        assert!(columns_of(&db, "users")
            .await
            .unwrap()
            .contains(&"role".to_string()));

        // role 必须回填成 admin：P7 的语义就是「凡能登录者皆管理员」
        let role: String = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT role FROM users WHERE username='root'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "role")
            .unwrap();
        assert_eq!(role, "admin", "存量账号必须仍是管理员");

        // owner 先为空（认领尚未发生）
        let before = find_project(&db, "legacy").await.unwrap().unwrap();
        assert_eq!(before.owner, "", "加列阶段只标无主，不猜归属");

        // 认领后归属该管理员
        assert_eq!(claim_unowned_projects(&db).await.unwrap(), 1);
        let after = find_project(&db, "legacy").await.unwrap().unwrap();
        assert_eq!(after.owner, "root");
        assert_eq!(after.db_path, "data/legacy/mem.db", "既有数据不得被破坏");

        // 幂等：第二次没有无主项目可认领
        assert_eq!(claim_unowned_projects(&db).await.unwrap(), 0);
        init_meta_db(&db).await.unwrap();
    }

    /// 没有任何账号时（只用 ADMIN_TOKEN 的部署），认领是空操作而非报错。
    ///
    /// @intent 这条守的是「不建控制台账号也能用」这一被支持的部署形态：
    ///         若认领在没有管理员时报错，服务会在启动阶段直接失败。
    #[tokio::test]
    async fn claiming_without_any_admin_is_a_no_op() {
        let db = temp_meta().await;
        init_meta_db(&db).await.unwrap();
        create_project(&db, &new_project("orphan", "t", DEFAULT_BACKEND))
            .await
            .unwrap();

        // 手工清空归属，模拟「迁移后还没建账号」的状态
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            "UPDATE projects SET owner = ''",
        ))
        .await
        .unwrap();

        assert_eq!(claim_unowned_projects(&db).await.unwrap(), 0);
        assert_eq!(
            find_project(&db, "orphan").await.unwrap().unwrap().owner,
            ""
        );
    }

    /// 认领只动无主项目，已有归属的项目不得被改写。
    #[tokio::test]
    async fn claiming_never_rewrites_an_existing_owner() {
        let db = temp_meta().await;
        init_meta_db(&db).await.unwrap();
        crate::users::create_with_role(&db, "root", "hash-for-test-only", crate::users::Role::Admin)
            .await
            .unwrap();
        create_project(&db, &owned("bobs", "tb", "bob"))
            .await
            .unwrap();
        create_project(&db, &owned("orphan", "to", ""))
            .await
            .unwrap();

        assert_eq!(claim_unowned_projects(&db).await.unwrap(), 1);

        assert_eq!(
            find_project(&db, "bobs").await.unwrap().unwrap().owner,
            "bob",
            "已有归属的项目不得被认领改写"
        );
        assert_eq!(
            find_project(&db, "orphan").await.unwrap().unwrap().owner,
            "root"
        );
    }

    /// 按归属列出项目：只返回自己的，且仍是升序。
    #[tokio::test]
    async fn list_projects_of_filters_by_owner() {
        let db = temp_meta().await;
        init_meta_db(&db).await.unwrap();
        for (id, owner) in [("a-alice", "alice"), ("b-bob", "bob"), ("c-alice", "alice")] {
            create_project(&db, &owned(id, &format!("t-{id}"), owner))
                .await
                .unwrap();
        }

        let alice: Vec<String> = list_projects_of(&db, "alice")
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.project_id)
            .collect();
        assert_eq!(alice, vec!["a-alice".to_string(), "c-alice".to_string()]);

        assert!(
            list_projects_of(&db, "nobody").await.unwrap().is_empty(),
            "没有项目的账号应得到空列表而非全量"
        );

        // 全量视图仍是三条，证明过滤发生在查询里而非写坏了数据
        assert_eq!(list_projects(&db).await.unwrap().len(), 3);
    }
}
