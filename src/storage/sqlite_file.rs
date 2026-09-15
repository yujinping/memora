//! SQLite 文件后端：每项目一个 `data/{project_id}/mem.db`。
//!
//! @author yujinping
//! @intent P2 默认后端。项目隔离靠「一项目一文件」的物理隔离实现，
//!          检索走 SQLite 内置 FTS5（bm25 排序），不引入任何外部服务。
//!
//! 检索策略：FTS5 全词检索（bm25 相关度排序）优先，无结果或语法异常时退化为
//! `LIKE %q%` 子串扫描。子串回退是为中文场景准备的——SQLite 内置 `unicode61`
//! 分词器不做中文分词，整段中文会成为一个 token，导致「深色」查不到
//! 「偏好使用深色主题」；回退后中文片段与英文词内片段均可命中。
//! 该差异完全锁在本文件内，不向调用方泄漏。
#![allow(clippy::doc_lazy_continuation)]

use crate::domain::{Entity, EntityInput, Graph, Observation, ObservationInput, Relation, RelationInput};
use crate::entity::memory::{entities, observations, relations};
use crate::error::StorageError;
use crate::storage::normalize;
use crate::storage::repo::MemoryRepository;
use crate::storage::{BackendKind, ProjectRepos, StorageBackend};
use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ColumnTrait, ConnectionTrait, Database, DatabaseConnection, DatabaseTransaction, DbBackend,
    EntityTrait, QueryFilter, QueryOrder, Statement, TransactionTrait,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// 项目库建表语句（幂等：全部 IF NOT EXISTS）。
///
/// @intent 不引入迁移框架：项目库结构简单且只增不改，`CREATE ... IF NOT EXISTS`
///         已足够；`memory_fts.obs_id` 为 UNINDEXED 列，仅用于把检索行映射回观测，
///         以便删除观测时精确清理索引。
const MEMORY_SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS entities (\
        name TEXT PRIMARY KEY, \
        entity_type TEXT NOT NULL DEFAULT 'unknown', \
        created_at INTEGER NOT NULL\
    )",
    "CREATE TABLE IF NOT EXISTS observations (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        entity_name TEXT NOT NULL, \
        content TEXT NOT NULL, \
        created_at INTEGER NOT NULL\
    )",
    "CREATE INDEX IF NOT EXISTS idx_observations_entity ON observations(entity_name)",
    "CREATE TABLE IF NOT EXISTS relations (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        from_name TEXT NOT NULL, \
        to_name TEXT NOT NULL, \
        relation_type TEXT NOT NULL, \
        created_at INTEGER NOT NULL\
    )",
    "CREATE INDEX IF NOT EXISTS idx_relations_from ON relations(from_name)",
    "CREATE INDEX IF NOT EXISTS idx_relations_to ON relations(to_name)",
    "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(name, content, obs_id UNINDEXED)",
];

/// 检索索引中「实体占位行」的 `obs_id` 取值（真实观测 id 自增，从 1 开始）。
const FTS_ENTITY_SENTINEL: i64 = -1;

/// 当前 Unix 秒。
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 将用户查询转换为 FTS5 短语查询。
///
/// @intent 每个词单独加双引号可中和 FTS5 的全部语法（列过滤、NEAR、前缀 `*` 等），
///         避免用户随手输入的符号导致 SQL 层报错；仅保留含字母 / 数字的词，
///         若一个都没有则返回 `None`（调用方直接走子串回退）。
fn fts_match_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.replace(['"', '*'], ""))
        .filter(|t| t.chars().any(|c| c.is_alphanumeric()))
        .map(|t| format!("\"{t}\""))
        .collect();

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

/// 构造 LIKE 子串模式，转义 `%`、`_` 与转义符本身。
///
/// @intent 逐个字符处理而非链式 `replace`：三个字符的转义目标各不相同
///         （`\` → `\\`、`%` → `\%`、`_` → `\_`），显式循环更能表达意图。
fn like_pattern(query: &str) -> String {
    let mut pattern = String::with_capacity(query.len() + 2);
    pattern.push('%');
    for ch in query.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

/// 建表（幂等，可在每次打开项目库时调用）。
pub async fn init_memory_db(db: &DatabaseConnection) -> Result<(), StorageError> {
    for sql in MEMORY_SCHEMA {
        db.execute(Statement::from_string(DbBackend::Sqlite, *sql))
            .await?;
    }
    Ok(())
}

/// 单项目库的记忆仓库。
pub struct SqliteMemoryRepo {
    /// 项目库连接（sea-orm 连接池）
    db: DatabaseConnection,
}

impl SqliteMemoryRepo {
    /// 提交或回滚，把两类结局收口到一处。
    async fn finish<T>(
        txn: DatabaseTransaction,
        result: Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        match result {
            Ok(value) => {
                txn.commit().await?;
                Ok(value)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    /// 在事务中写入 FTS 索引行。
    async fn index_row(
        txn: &DatabaseTransaction,
        name: &str,
        content: &str,
        obs_id: i64,
    ) -> Result<(), StorageError> {
        txn.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO memory_fts(name, content, obs_id) VALUES (?, ?, ?)",
            [name.into(), content.into(), obs_id.into()],
        ))
        .await?;
        Ok(())
    }

    /// 按名清理 FTS 索引行（实体占位行与观测行同名，可一并清除）。
    async fn unindex_names(
        txn: &DatabaseTransaction,
        names: &[String],
    ) -> Result<(), StorageError> {
        for name in names {
            txn.execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "DELETE FROM memory_fts WHERE name = ?",
                [name.clone().into()],
            ))
            .await?;
        }
        Ok(())
    }

    /// FTS5 检索：全词命中并按 bm25 相关度排序（去重保序）。
    async fn fts_names(db: &DatabaseConnection, query: &str) -> Result<Vec<String>, StorageError> {
        let Some(match_expr) = fts_match_query(query) else {
            return Ok(Vec::new());
        };

        let rows = db
            .query_all(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT name, bm25(memory_fts) AS score FROM memory_fts \
                 WHERE memory_fts MATCH ? ORDER BY score",
                [match_expr.into()],
            ))
            .await?;

        let mut names: Vec<String> = Vec::new();
        for row in rows {
            let name: String = row.try_get("", "name")?;
            if !names.contains(&name) {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// 子串回退检索：实体名 / 类型 / 观测内容任一命中即算命中。
    async fn like_names(db: &DatabaseConnection, query: &str) -> Result<Vec<String>, StorageError> {
        let pattern = like_pattern(query);
        let rows = db
            .query_all(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT n FROM (\
                    SELECT name AS n FROM entities \
                        WHERE name LIKE ? ESCAPE '\\' OR entity_type LIKE ? ESCAPE '\\' \
                    UNION \
                    SELECT entity_name AS n FROM observations WHERE content LIKE ? ESCAPE '\\'\
                 ) ORDER BY n",
                [pattern.clone().into(), pattern.clone().into(), pattern.into()],
            ))
            .await?;

        let mut names = Vec::new();
        for row in rows {
            names.push(row.try_get::<String>("", "n")?);
        }
        Ok(names)
    }

    /// 批量取实体明细（含观测），按 name 升序；不存在的名字跳过。
    async fn fetch_entities(&self, names: &[String]) -> Result<Vec<Entity>, StorageError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }

        let entity_rows = entities::Entity::find()
            .filter(entities::Column::Name.is_in(names.to_vec()))
            .order_by_asc(entities::Column::Name)
            .all(&self.db)
            .await?;
        if entity_rows.is_empty() {
            return Ok(Vec::new());
        }

        let observation_rows = observations::Entity::find()
            .filter(observations::Column::EntityName.is_in(names.to_vec()))
            .order_by_asc(observations::Column::Id)
            .all(&self.db)
            .await?;

        let mut grouped: HashMap<String, Vec<Observation>> = HashMap::new();
        for o in observation_rows {
            grouped.entry(o.entity_name.clone()).or_default().push(Observation {
                id: o.id,
                entity_name: o.entity_name,
                content: o.content,
                created_at: o.created_at,
            });
        }

        Ok(entity_rows
            .into_iter()
            .map(|e| Entity {
                observations: grouped.remove(&e.name).unwrap_or_default(),
                name: e.name,
                entity_type: e.entity_type,
                created_at: e.created_at,
            })
            .collect())
    }

    /// 按给定名字顺序排列实体（保持检索相关度序）。
    async fn fetch_ordered(&self, ordered_names: &[String]) -> Result<Vec<Entity>, StorageError> {
        let mut loaded = self.fetch_entities(ordered_names).await?;
        let mut index: HashMap<String, Entity> = loaded
            .drain(..)
            .map(|e| (e.name.clone(), e))
            .collect();
        Ok(ordered_names
            .iter()
            .filter_map(|n| index.remove(n))
            .collect())
    }

    // ------------------------------------------------------------ 事务内写入

    async fn tx_create_entities(
        txn: &DatabaseTransaction,
        normalized: Vec<(String, String)>,
    ) -> Result<(), StorageError> {
        let ts = now();
        for (name, entity_type) in normalized {
            let exists = entities::Entity::find()
                .filter(entities::Column::Name.eq(name.as_str()))
                .one(txn)
                .await?
                .is_some();
            if exists {
                continue;
            }
            entities::Entity::insert(entities::ActiveModel {
                name: Set(name.clone()),
                entity_type: Set(entity_type.clone()),
                created_at: Set(ts),
            })
            .exec(txn)
            .await?;

            // 占位索引行：让「只建了实体、暂未写观测」的名字也可被检索
            Self::index_row(txn, &name, &entity_type, FTS_ENTITY_SENTINEL).await?;
        }
        Ok(())
    }

    async fn tx_create_relations(
        txn: &DatabaseTransaction,
        normalized: Vec<normalize::RelationKey>,
    ) -> Result<(), StorageError> {
        let ts = now();
        for (from_name, to_name, relation_type) in normalized {
            let exists = relations::Entity::find()
                .filter(relations::Column::FromName.eq(from_name.as_str()))
                .filter(relations::Column::ToName.eq(to_name.as_str()))
                .filter(relations::Column::RelationType.eq(relation_type.as_str()))
                .one(txn)
                .await?
                .is_some();
            if exists {
                continue;
            }
            relations::Entity::insert(relations::ActiveModel {
                from_name: Set(from_name),
                to_name: Set(to_name),
                relation_type: Set(relation_type),
                created_at: Set(ts),
                ..Default::default()
            })
            .exec(txn)
            .await?;
        }
        Ok(())
    }

    async fn tx_add_observations(
        txn: &DatabaseTransaction,
        normalized: Vec<(String, Vec<String>)>,
    ) -> Result<(), StorageError> {
        for (entity_name, _) in &normalized {
            let exists = entities::Entity::find()
                .filter(entities::Column::Name.eq(entity_name.as_str()))
                .one(txn)
                .await?
                .is_some();
            if !exists {
                return Err(StorageError::EntityNotFound(entity_name.clone()));
            }
        }

        let ts = now();
        for (entity_name, contents) in normalized {
            for content in contents {
                let result = observations::Entity::insert(observations::ActiveModel {
                    entity_name: Set(entity_name.clone()),
                    content: Set(content.clone()),
                    created_at: Set(ts),
                    ..Default::default()
                })
                .exec(txn)
                .await?;

                Self::index_row(txn, &entity_name, &content, result.last_insert_id).await?;
            }
        }
        Ok(())
    }

    async fn tx_delete_entities(
        txn: &DatabaseTransaction,
        names: &[String],
    ) -> Result<(), StorageError> {
        Self::unindex_names(txn, names).await?;

        observations::Entity::delete_many()
            .filter(observations::Column::EntityName.is_in(names.to_vec()))
            .exec(txn)
            .await?;
        relations::Entity::delete_many()
            .filter(relations::Column::FromName.is_in(names.to_vec()))
            .exec(txn)
            .await?;
        relations::Entity::delete_many()
            .filter(relations::Column::ToName.is_in(names.to_vec()))
            .exec(txn)
            .await?;
        entities::Entity::delete_many()
            .filter(entities::Column::Name.is_in(names.to_vec()))
            .exec(txn)
            .await?;
        Ok(())
    }

    async fn tx_delete_observations(
        txn: &DatabaseTransaction,
        ids: &[i64],
    ) -> Result<(), StorageError> {
        for id in ids {
            txn.execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "DELETE FROM memory_fts WHERE obs_id = ?",
                [(*id).into()],
            ))
            .await?;
        }
        observations::Entity::delete_many()
            .filter(observations::Column::Id.is_in(ids.to_vec()))
            .exec(txn)
            .await?;
        Ok(())
    }

    async fn tx_delete_relations(
        txn: &DatabaseTransaction,
        ids: &[i64],
    ) -> Result<(), StorageError> {
        relations::Entity::delete_many()
            .filter(relations::Column::Id.is_in(ids.to_vec()))
            .exec(txn)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl MemoryRepository for SqliteMemoryRepo {
    async fn create_entities(&self, entity_inputs: Vec<EntityInput>) -> Result<(), StorageError> {
        let normalized = normalize::entities(&entity_inputs)?;
        if normalized.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_create_entities(&txn, normalized).await;
        Self::finish(txn, result).await
    }

    async fn create_relations(&self, relation_inputs: Vec<RelationInput>) -> Result<(), StorageError> {
        let normalized = normalize::relations(&relation_inputs)?;
        if normalized.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_create_relations(&txn, normalized).await;
        Self::finish(txn, result).await
    }

    async fn add_observations(
        &self,
        observation_inputs: Vec<ObservationInput>,
    ) -> Result<(), StorageError> {
        let normalized = normalize::observations(&observation_inputs)?;
        if normalized.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_add_observations(&txn, normalized).await;
        Self::finish(txn, result).await
    }

    async fn delete_entities(&self, names: &[String]) -> Result<(), StorageError> {
        let names: Vec<String> = names.iter().map(|n| n.trim().to_string()).collect();
        if names.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_delete_entities(&txn, &names).await;
        Self::finish(txn, result).await
    }

    async fn delete_observations(&self, ids: &[i64]) -> Result<(), StorageError> {
        if ids.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_delete_observations(&txn, ids).await;
        Self::finish(txn, result).await
    }

    async fn delete_relations(&self, ids: &[i64]) -> Result<(), StorageError> {
        if ids.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_delete_relations(&txn, ids).await;
        Self::finish(txn, result).await
    }

    async fn read_graph(&self) -> Result<Graph, StorageError> {
        let entity_rows = entities::Entity::find()
            .order_by_asc(entities::Column::Name)
            .all(&self.db)
            .await?;
        let observation_rows = observations::Entity::find()
            .order_by_asc(observations::Column::Id)
            .all(&self.db)
            .await?;
        let relation_rows = relations::Entity::find()
            .order_by_asc(relations::Column::Id)
            .all(&self.db)
            .await?;

        let mut grouped: HashMap<String, Vec<Observation>> = HashMap::new();
        for o in observation_rows {
            grouped.entry(o.entity_name.clone()).or_default().push(Observation {
                id: o.id,
                entity_name: o.entity_name,
                content: o.content,
                created_at: o.created_at,
            });
        }

        Ok(Graph {
            entities: entity_rows
                .into_iter()
                .map(|e| Entity {
                    observations: grouped.remove(&e.name).unwrap_or_default(),
                    name: e.name,
                    entity_type: e.entity_type,
                    created_at: e.created_at,
                })
                .collect(),
            relations: relation_rows
                .into_iter()
                .map(|r| Relation {
                    id: r.id,
                    from_name: r.from_name,
                    to_name: r.to_name,
                    relation_type: r.relation_type,
                    created_at: r.created_at,
                })
                .collect(),
        })
    }

    async fn search_nodes(&self, query: &str) -> Result<Vec<Entity>, StorageError> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }

        let hit_names = match Self::fts_names(&self.db, q).await {
            Ok(names) if !names.is_empty() => names,
            Ok(_) => Self::like_names(&self.db, q).await?,
            Err(err) => {
                tracing::warn!(query = q, error = %err, "FTS 检索不可用，退化为子串匹配");
                Self::like_names(&self.db, q).await?
            }
        };

        self.fetch_ordered(&hit_names).await
    }

    async fn open_nodes(&self, names: &[String]) -> Result<Vec<Entity>, StorageError> {
        let names: Vec<String> = {
            let mut v: Vec<String> = names.iter().map(|n| n.trim().to_string()).collect();
            v.sort();
            v.dedup();
            v
        };
        self.fetch_entities(&names).await
    }
}

/// SQLite 文件后端：按 project_id 换算出项目库路径。
///
/// @intent `projects.db_path`（元库登记值）仅作记录，项目库位置以本后端推导结果为准，
///         避免出现「元库路径」与「实际文件」两个真相源。
pub struct SqliteFileBackend {
    /// 数据根目录（来自 `DATA_DIR`）
    data_dir: PathBuf,
    /// 已打开的项目库连接缓存，避免每请求重连
    cache: Mutex<HashMap<String, DatabaseConnection>>,
    /// 每个项目一把「建池闸门」，保证同一项目的连接池只被创建一次。
    ///
    /// @intent 这是 P4.2 压测暴露的真实缺陷的修复：`repositories_for` 是
    ///         「查缓存 → 建池 → 回填」，两步之间必然存在 await 点，于是并发首次访问
    ///         会各自建出一个指向**同一文件**的连接池。而建池时要执行
    ///         `PRAGMA journal_mode = WAL`，该 PRAGMA 需要**独占锁且不受
    ///         `busy_timeout` 保护**（SQLite 在独占锁场景遇 BUSY 立即返回以避免死锁），
    ///         因此并发建池必然有一方以 `database is locked`(SQLITE_BUSY) 失败。
    ///         注意 sqlx 默认的 5 秒 busy_timeout 对此无效——它不是等待超时问题。
    ///
    /// @intent 闸门条目按项目保留而非用后即删：删除会让等待者与新来者落到**不同的**
    ///         Mutex 上，重新出现并发建池。条目数等于活跃项目数，内存开销可忽略，
    ///         并在 [`SqliteFileBackend::drop_project`] 中随项目生命周期清理。
    openings: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl SqliteFileBackend {
    /// 构造后端。
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            cache: Mutex::new(HashMap::new()),
            openings: Mutex::new(HashMap::new()),
        }
    }

    /// 取（或按需创建）某项目的建池闸门。
    async fn gate_for(&self, project_id: &str) -> Arc<Mutex<()>> {
        self.openings
            .lock()
            .await
            .entry(project_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 建立并初始化项目库连接池（调用方必须已持有该项目的建池闸门）。
    async fn create_pool(&self, project_id: &str) -> Result<DatabaseConnection, StorageError> {
        let path = self.db_path(project_id);
        let parent = path.parent().unwrap_or(&self.data_dir).to_path_buf();
        std::fs::create_dir_all(&parent)?;

        let url = format!("sqlite://{}?mode=rwc", path.display());
        let db = Database::connect(&url).await?;

        // WAL + NORMAL：单机读多写少场景下的常规取舍，兼顾吞吐与掉电安全。
        // `journal_mode = WAL` 需要独占锁，故本函数必须被闸门串行化保护（见 `openings`）。
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA journal_mode = WAL",
        ))
        .await?;
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA synchronous = NORMAL",
        ))
        .await?;
        init_memory_db(&db).await?;
        Ok(db)
    }

    /// 组装仓库集合（两处返回点共用，避免字段漂移）。
    fn repos_with(&self, project_id: &str, db: DatabaseConnection) -> ProjectRepos {
        ProjectRepos {
            project_id: project_id.to_string(),
            backend: BackendKind::SqliteFile,
            memory: Arc::new(SqliteMemoryRepo { db }),
        }
    }

    /// 项目库文件路径：`{data_dir}/{project_id}/mem.db`。
    pub fn db_path(&self, project_id: &str) -> PathBuf {
        self.data_dir.join(project_id).join("mem.db")
    }
}

#[async_trait]
impl StorageBackend for SqliteFileBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::SqliteFile
    }

    async fn repositories_for(&self, project_id: &str) -> Result<ProjectRepos, StorageError> {
        crate::storage::validate_project_id(project_id)?;

        // 快路径：绝大多数请求在此命中，无需触碰闸门
        if let Some(db) = self.cache.lock().await.get(project_id).cloned() {
            return Ok(self.repos_with(project_id, db));
        }

        // 慢路径：按项目串行化建池（详见 `openings` 字段说明）
        let gate = self.gate_for(project_id).await;
        let _guard = gate.lock().await;

        // 双重检查：等待期间可能已有其他任务建好
        if let Some(db) = self.cache.lock().await.get(project_id).cloned() {
            return Ok(self.repos_with(project_id, db));
        }

        let db = self.create_pool(project_id).await?;
        self.cache
            .lock()
            .await
            .insert(project_id.to_string(), db.clone());

        Ok(self.repos_with(project_id, db))
    }

    async fn drop_project(&self, project_id: &str) -> Result<(), StorageError> {
        crate::storage::validate_project_id(project_id)?;

        // 先摘除连接缓存再删文件：留着已打开的连接会让进程继续持有旧 inode（Unix 下
        // 删除只是 unlink），表现为「项目已删除但仍有写入落在不可见的文件上」。
        self.cache.lock().await.remove(project_id);

        // 整个项目目录一并删除：`mem.db` 之外还有 WAL 模式产生的 `-wal` / `-shm`，
        // 以及未来可能加入的附属文件；只删 mem.db 会留下可恢复的数据残片。
        let dir = self.data_dir.join(project_id);
        // 闸门条目随项目生命周期清理，避免 openings 无限增长
        self.openings.lock().await.remove(project_id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            // 从未落盘（或已被删过）⇒ 幂等成功
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EntityInput;
    use crate::storage::contract;
    use crate::storage::StorageBackend;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 构造隔离的临时数据目录。
    fn temp_data_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("memora_p2_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 直接构造具体仓库（持有连接，便于断言内部检索路径与索引状态）。
    async fn temp_repo(project: &str) -> SqliteMemoryRepo {
        let path = temp_data_dir().join(project).join("mem.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        init_memory_db(&db).await.unwrap();
        SqliteMemoryRepo { db }
    }

    /// SQLite 文件后端必须与内存后端行为等价。
    #[tokio::test]
    async fn sqlite_file_satisfies_repository_contract() {
        let backend = SqliteFileBackend::new(temp_data_dir());
        let repos = backend.repositories_for("contract").await.unwrap();
        contract::run_all(&*repos.memory).await;
    }

    /// 后端级契约（项目生命周期）同样必须与内存后端等价。
    #[tokio::test]
    async fn sqlite_file_satisfies_backend_contract() {
        let backend = SqliteFileBackend::new(temp_data_dir());
        contract::run_backend_contract(&backend).await;
    }

    /// 并发写入必须全部成功，且一条都不能丢。
    ///
    /// @intent 这条用例来自 P4.2 的真实压测：SQLite 是**单写者**模型，连接池一旦有
    ///         多条连接，并发写就会以 `database is locked`（SQLITE_BUSY）失败——
    ///         而且是在事务内升级锁失败，`busy_timeout` 也救不了（SQLite 在事务中
    ///         遇 BUSY 会立即返回以避免死锁）。此前的验证全是串行的，因此从未暴露。
    ///
    /// @intent 必须用 multi_thread：默认的 current_thread runtime 下 `tokio::spawn`
    ///         只会协作式轮流执行，SQLite 调用之间没有 await 点，实际根本不会并发，
    ///         用例会假绿。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_all_succeed_without_loss() {
        const WRITERS: usize = 16;

        let backend = SqliteFileBackend::new(temp_data_dir());
        let repos = backend.repositories_for("concurrent").await.unwrap();
        let repo = repos.memory.clone();

        let mut handles = Vec::with_capacity(WRITERS);
        for i in 0..WRITERS {
            let repo = repo.clone();
            handles.push(tokio::spawn(async move {
                repo.create_entities(vec![EntityInput::new(format!("entity-{i}"), "concept")])
                    .await
            }));
        }
        for (i, handle) in handles.into_iter().enumerate() {
            handle
                .await
                .expect("任务不得 panic")
                .unwrap_or_else(|err| panic!("第 {i} 个并发写入失败: {err}"));
        }

        let graph = repo.read_graph().await.unwrap();
        assert_eq!(
            graph.entity_count(),
            WRITERS,
            "并发写入不得丢失任何实体"
        );
    }

    /// 并发首次取用同一项目，必须只建立一个连接池。
    ///
    /// @intent 对应压测暴露的第二个缺陷：`repositories_for` 是「查缓存 → 未命中则建池
    ///         → 回填」，两步之间存在 await 点。并发首次访问会各自建出一个指向**同一
    ///         文件**的连接池，回填后只有一个能被复用，其余池被丢弃却仍在使用中——
    ///         此后这两个池并发写同一文件，立刻 `database is locked`。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_access_creates_a_single_pool() {
        const CALLERS: usize = 8;

        let backend = std::sync::Arc::new(SqliteFileBackend::new(temp_data_dir()));
        let mut handles = Vec::with_capacity(CALLERS);
        for i in 0..CALLERS {
            let backend = backend.clone();
            handles.push(tokio::spawn(async move {
                let repos = backend.repositories_for("same").await.unwrap();
                repos
                    .memory
                    .create_entities(vec![EntityInput::new(format!("c{i}"), "concept")])
                    .await
            }));
        }
        for (i, handle) in handles.into_iter().enumerate() {
            handle
                .await
                .expect("任务不得 panic")
                .unwrap_or_else(|err| panic!("第 {i} 个并发请求失败: {err}"));
        }

        let repos = backend.repositories_for("same").await.unwrap();
        assert_eq!(
            repos.memory.read_graph().await.unwrap().entity_count(),
            CALLERS,
            "并发首次取用不得丢数据"
        );
    }

    /// 并发「读 + 写」混合同样不得失败。
    ///
    /// @intent 纯写测试只能覆盖写-写竞争；读写混合还会暴露「读事务持有 SHARED 锁
    ///         导致写者无法升级」这一类问题，而 MCP 客户端的真实调用模式正是混合的。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_mixed_read_write_all_succeed() {
        let backend = SqliteFileBackend::new(temp_data_dir());
        let repos = backend.repositories_for("mixed").await.unwrap();
        let repo = repos.memory.clone();

        repo.create_entities(vec![EntityInput::new("seed", "concept")])
            .await
            .unwrap();

        let mut handles = Vec::new();
        for i in 0..8 {
            let writer = repo.clone();
            handles.push(tokio::spawn(async move {
                writer
                    .create_entities(vec![EntityInput::new(format!("w{i}"), "concept")])
                    .await
            }));
            let reader = repo.clone();
            handles.push(tokio::spawn(async move {
                reader.read_graph().await.map(|_| ())
            }));
        }
        for handle in handles {
            handle
                .await
                .expect("任务不得 panic")
                .expect("读写混合不得失败");
        }

        assert_eq!(repo.read_graph().await.unwrap().entity_count(), 9);
    }

    /// 丢弃项目必须真正删除文件，且连目录一起清掉——WAL 模式下还有 `-wal` / `-shm`
    /// 附属文件，漏删即等于「声称删除但数据仍在」。
    #[tokio::test]
    async fn drop_project_removes_db_file_and_wal_siblings() {
        let backend = SqliteFileBackend::new(temp_data_dir());
        let repos = backend.repositories_for("gone").await.unwrap();
        repos
            .memory
            .create_entities(vec![EntityInput::new("doomed", "person")])
            .await
            .unwrap();
        drop(repos);

        let file = backend.db_path("gone");
        let dir = file.parent().unwrap().to_path_buf();
        assert!(file.exists(), "前置：项目库应已落盘");
        assert!(dir.exists(), "前置：项目目录应已创建");

        backend.drop_project("gone").await.unwrap();

        assert!(!file.exists(), "mem.db 必须被删除");
        assert!(!dir.exists(), "项目目录（含 -wal / -shm）必须被一并删除");
    }

    /// 丢弃不影响其他项目的文件。
    #[tokio::test]
    async fn drop_project_leaves_other_project_files_intact() {
        let backend = SqliteFileBackend::new(temp_data_dir());
        for id in ["keep", "gone"] {
            backend
                .repositories_for(id)
                .await
                .unwrap()
                .memory
                .create_entities(vec![EntityInput::new("x", "person")])
                .await
                .unwrap();
        }

        backend.drop_project("gone").await.unwrap();

        assert!(backend.db_path("keep").exists(), "其他项目的库文件必须保留");
        let graph = backend
            .repositories_for("keep")
            .await
            .unwrap()
            .memory
            .read_graph()
            .await
            .unwrap();
        assert_eq!(graph.entity_count(), 1, "其他项目的数据必须完好");
    }

    /// 每项目一个独立文件，互不可见。
    #[tokio::test]
    async fn sqlite_file_isolates_projects() {
        let backend = SqliteFileBackend::new(temp_data_dir());
        let a = backend.repositories_for("proj-a").await.unwrap();
        let b = backend.repositories_for("proj-b").await.unwrap();

        a.memory
            .create_entities(vec![EntityInput::new("only-a", "person")])
            .await
            .unwrap();

        assert_eq!(a.memory.read_graph().await.unwrap().entity_count(), 1);
        assert_eq!(b.memory.read_graph().await.unwrap().entity_count(), 0);

        assert!(backend.db_path("proj-a").exists());
        assert!(backend.db_path("proj-b").exists());
        assert_ne!(backend.db_path("proj-a"), backend.db_path("proj-b"));
    }

    /// 数据落盘后可由新实例读回（跨进程 / 跨会话恢复的前提）。
    #[tokio::test]
    async fn sqlite_file_persists_across_backend_instances() {
        let dir = temp_data_dir();
        let first = SqliteFileBackend::new(dir.clone());
        first
            .repositories_for("keep")
            .await
            .unwrap()
            .memory
            .create_entities(vec![EntityInput::new("persisted", "concept")])
            .await
            .unwrap();

        let second = SqliteFileBackend::new(dir);
        let graph = second
            .repositories_for("keep")
            .await
            .unwrap()
            .memory
            .read_graph()
            .await
            .unwrap();
        assert_eq!(graph.entity_count(), 1);
        assert_eq!(graph.entities[0].name, "persisted");
    }

    /// 建表语句必须幂等（重复打开同一项目库不应报错）。
    #[tokio::test]
    async fn init_memory_db_is_idempotent() {
        let dir = temp_data_dir();
        let backend = SqliteFileBackend::new(dir);
        let path = backend.db_path("twice");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();

        init_memory_db(&db).await.unwrap();
        init_memory_db(&db).await.unwrap();
        assert_eq!(backend.kind(), BackendKind::SqliteFile);
    }

    /// 非法 project_id 必须被拒绝，防止越出 DATA_DIR 写文件。
    #[tokio::test]
    async fn sqlite_file_rejects_unsafe_project_id() {
        let backend = SqliteFileBackend::new(temp_data_dir());
        assert!(backend.repositories_for("../evil").await.is_err());
        assert!(backend.repositories_for("a/b").await.is_err());
    }

    /// bm25 相关度排序：词频更高的观测应排在前面。
    #[tokio::test]
    async fn search_ranks_more_relevant_first() {
        let repo = temp_repo("rank").await;

        repo.create_entities(vec![
            EntityInput::new("many", "note"),
            EntityInput::new("few", "note"),
        ])
        .await
        .unwrap();
        repo.add_observations(vec![
            ObservationInput::single("many", "alpha alpha alpha alpha"),
            ObservationInput::single("few", "alpha"),
        ])
        .await
        .unwrap();

        let hits = repo.search_nodes("alpha").await.unwrap();
        assert_eq!(
            hits.iter().map(|e| e.name.clone()).next(),
            Some("many".to_string()),
            "词频更高的实体应排在检索结果首位"
        );
    }

    /// 中文与英文词内片段依赖子串回退命中（FTS5 全词匹配在此必然落空）。
    #[tokio::test]
    async fn search_falls_back_to_substring_when_fts_misses() {
        let repo = temp_repo("cjk").await;

        repo.create_entities(vec![EntityInput::new("note", "memory")])
            .await
            .unwrap();
        repo.add_observations(vec![ObservationInput::single(
            "note",
            "偏好使用深色主题与 sqlite 存储",
        )])
        .await
        .unwrap();

        // FTS5 只做全词匹配：这两个查询在索引里都不是完整 token
        // （"深色" 被 unicode61 并入整段中文，"qlit" 只是 "sqlite" 的词内片段）
        assert!(
            SqliteMemoryRepo::fts_names(&repo.db, "深色")
                .await
                .unwrap()
                .is_empty(),
            "FTS5 应无法命中中文片段（验证回退路径确有必要）"
        );
        assert!(
            SqliteMemoryRepo::fts_names(&repo.db, "qlit")
                .await
                .unwrap()
                .is_empty(),
            "FTS5 应无法命中英文词内片段"
        );

        assert_eq!(repo.search_nodes("深色").await.unwrap()[0].name, "note");
        assert_eq!(repo.search_nodes("qlit").await.unwrap()[0].name, "note");
    }

    /// 恶意 / 无意义查询串不得导致 SQL 层报错。
    #[tokio::test]
    async fn hostile_query_strings_do_not_error() {
        let repo = temp_repo("hostile").await;
        repo.create_entities(vec![EntityInput::new("safe", "note")])
            .await
            .unwrap();

        for q in [
            "\"", "*", "(", ")", "AND", "OR", "NOT", "NEAR", "^", ":", "a\"b", "'", "--", "50%",
            "a_b", "\\", "记忆 OR 1=1",
        ] {
            assert!(repo.search_nodes(q).await.is_ok(), "查询 {q:?} 不应报错");
        }
    }

    #[test]
    fn fts_query_is_neutralised_and_quoted() {
        assert_eq!(fts_match_query("acme"), Some("\"acme\"".to_string()));
        assert_eq!(
            fts_match_query("works at"),
            Some("\"works\" \"at\"".to_string())
        );
        // 双引号与星号被剥离，避免语法错误与意外的前缀匹配
        assert_eq!(fts_match_query("a\"b*"), Some("\"ab\"".to_string()));
        // 纯粹符号 / 空白 → 无有效词，交给子串回退
        assert_eq!(fts_match_query("\""), None);
        assert_eq!(fts_match_query("*"), None);
        assert_eq!(fts_match_query("   "), None);
        assert_eq!(fts_match_query("-+^:()"), None);
    }

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("abc"), "%abc%");
        assert_eq!(like_pattern("50%"), "%50\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        assert_eq!(like_pattern("c:\\x"), "%c:\\\\x%");
    }
}
