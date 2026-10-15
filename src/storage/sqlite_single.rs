//! SQLite 单库后端：所有项目共用 `data/mem-single.db`，租户隔离全靠 `project_id` 列过滤。
//!
//! @author yujinping
//! @intent P6：与 [`super::sqlite_file`] 的「一项目一文件」并列的第二个持久化形态。
//!          适配「项目数多但单项目体量小」的部署：免掉每项目一个连接池的文件句柄开销，
//!          也让「项目即目录」的文件布局不再是硬约束。
//!
//! 隔离模型（docs/memory-system-design.md §12.3）：
//! - `entities` 主键扩为 `(project_id, name)`——实体名只在项目内唯一；
//! - `observations` / `relations` 的自增 id 是**全库**唯一（非项目内），跨项目引用
//!   他人的 id 必须被租户条件挡住；
//! - `handoffs` 主键扩为 `(project_id, task_id, revision)`——不同项目允许有同名任务
//!   （含各自的 `current@<client>` 槽位）；
//! - FTS5 检索索引带 `project_id` 列，全部查询带租户条件。
//!
//! 检索策略与文件后端一致：FTS5 全词检索（bm25）优先，无结果或语法异常时退化为
//! `LIKE %q%` 子串扫描（中文场景依赖此回退，见 [`super::sqlite_file`] 的模块注释）。
#![allow(clippy::doc_lazy_continuation)]

use crate::domain::{
    AckOutcome, Entity, EntityInput, Graph, Handoff, HandoffInput, HandoffResolution,
    HandoffSummary, Observation, ObservationInput, Relation, RelationInput, SessionInput, Turn,
};
use crate::entity::single::{conversations, entities, handoffs, observations, relations};
use crate::error::StorageError;
use crate::storage::normalize;
use crate::storage::repo::{ConversationRepository, HandoffRepository, MemoryRepository};
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

/// 单库文件名（位于 DATA_DIR 下，与项目目录同层但带 `.` 后缀不会与目录混淆）。
const SINGLE_DB_FILE: &str = "mem-single.db";

/// 单库建表语句（幂等：全部 IF NOT EXISTS）。
///
/// @intent 表结构与文件后端同构，差异只有三处：① 每表多一列 `project_id`；
///         ② `entities` / `handoffs` 的主键扩为含租户的复合主键；
///         ③ `memory_fts` 的 `project_id` 列标 UNINDEXED——它只做等值过滤、
///         不参与全文匹配，UNINDEXED 可省去分词开销。
///         `source` 列自本版起就存在，故不需要文件后端那套补列迁移。
const SINGLE_SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS entities (\
        project_id TEXT NOT NULL, \
        name TEXT NOT NULL, \
        entity_type TEXT NOT NULL DEFAULT 'unknown', \
        created_at INTEGER NOT NULL, \
        source TEXT NOT NULL DEFAULT '', \
        PRIMARY KEY (project_id, name)\
    )",
    "CREATE TABLE IF NOT EXISTS observations (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        project_id TEXT NOT NULL, \
        entity_name TEXT NOT NULL, \
        content TEXT NOT NULL, \
        created_at INTEGER NOT NULL, \
        source TEXT NOT NULL DEFAULT ''\
    )",
    "CREATE INDEX IF NOT EXISTS idx_observations_tenant_entity \
        ON observations(project_id, entity_name)",
    "CREATE TABLE IF NOT EXISTS relations (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        project_id TEXT NOT NULL, \
        from_name TEXT NOT NULL, \
        to_name TEXT NOT NULL, \
        relation_type TEXT NOT NULL, \
        created_at INTEGER NOT NULL, \
        source TEXT NOT NULL DEFAULT ''\
    )",
    "CREATE INDEX IF NOT EXISTS idx_relations_tenant_from ON relations(project_id, from_name)",
    "CREATE INDEX IF NOT EXISTS idx_relations_tenant_to ON relations(project_id, to_name)",
    "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts \
        USING fts5(project_id UNINDEXED, name, content, obs_id UNINDEXED)",
    // --- P4.3 交接：单库形态下主键扩为 (project_id, task_id, revision) ---
    "CREATE TABLE IF NOT EXISTS handoffs (\
        project_id TEXT NOT NULL, \
        task_id TEXT NOT NULL, \
        revision INTEGER NOT NULL, \
        goal TEXT NOT NULL, \
        state TEXT NOT NULL, \
        next_steps TEXT NOT NULL, \
        files TEXT NOT NULL, \
        decisions TEXT NOT NULL, \
        open_questions TEXT NOT NULL, \
        client TEXT NOT NULL DEFAULT '', \
        session_id TEXT NOT NULL DEFAULT '', \
        created_at INTEGER NOT NULL, \
        acked_at INTEGER, \
        acked_by TEXT NOT NULL DEFAULT '', \
        PRIMARY KEY (project_id, task_id, revision)\
    )",
    // 「每任务只取最新修订」是 list / resolve 的共同形态，故索引按 (project_id, task_id, revision DESC)
    "CREATE INDEX IF NOT EXISTS idx_handoffs_tenant_task \
        ON handoffs(project_id, task_id, revision DESC)",
    // --- P5.1 会话：逐轮 transcript 追加表，主键含租户维度 ---
    "CREATE TABLE IF NOT EXISTS conversations (\
        project_id TEXT NOT NULL, \
        session_id TEXT NOT NULL, \
        turn_index INTEGER NOT NULL, \
        role TEXT NOT NULL, \
        content TEXT NOT NULL, \
        created_at INTEGER NOT NULL, \
        PRIMARY KEY (project_id, session_id, turn_index)\
    )",
    "CREATE INDEX IF NOT EXISTS idx_conversations_tenant_session \
        ON conversations(project_id, session_id, created_at)",
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

/// 将用户查询转换为 FTS5 短语查询（与文件后端同一套中和规则）。
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

/// 建表（幂等，可在每次打开共享库时调用）。
pub async fn init_single_db(db: &DatabaseConnection) -> Result<(), StorageError> {
    for sql in SINGLE_SCHEMA {
        db.execute(Statement::from_string(DbBackend::Sqlite, *sql))
            .await?;
    }
    Ok(())
}

/// 单租户视图下的记忆仓库：所有查询自动附带 `project_id` 条件。
///
/// @intent 仓库对象持有所属项目标识，「租户过滤」收敛在**每个方法的第一层**——
///         上层（MCP / REST）完全无感，这正是 §12.5「租户过滤仅存在于 impl 内」的落点。
pub struct SqliteSingleMemoryRepo {
    /// 所属项目（租户）
    project_id: String,
    /// 共享库连接（sea-orm 连接池，全后端共用一份）
    db: DatabaseConnection,
}

impl SqliteSingleMemoryRepo {
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

    /// 在事务中写入 FTS 索引行（带租户列）。
    async fn index_row(
        txn: &DatabaseTransaction,
        project_id: &str,
        name: &str,
        content: &str,
        obs_id: i64,
    ) -> Result<(), StorageError> {
        txn.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO memory_fts(project_id, name, content, obs_id) VALUES (?, ?, ?, ?)",
            [
                project_id.into(),
                name.into(),
                content.into(),
                obs_id.into(),
            ],
        ))
        .await?;
        Ok(())
    }

    /// 按名清理本租户的 FTS 索引行。
    ///
    /// @intent 实体名只在项目内唯一，跨项目同名实体在索引里并存——
    ///         清理必须同时带 `project_id`，否则会删掉别人项目里同名实体的索引。
    async fn unindex_names(
        txn: &DatabaseTransaction,
        project_id: &str,
        names: &[String],
    ) -> Result<(), StorageError> {
        for name in names {
            txn.execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "DELETE FROM memory_fts WHERE project_id = ? AND name = ?",
                [project_id.into(), name.clone().into()],
            ))
            .await?;
        }
        Ok(())
    }

    /// FTS5 检索：本租户内全词命中并按 bm25 相关度排序（去重保序）。
    async fn fts_names(
        db: &DatabaseConnection,
        project_id: &str,
        query: &str,
    ) -> Result<Vec<String>, StorageError> {
        let Some(match_expr) = fts_match_query(query) else {
            return Ok(Vec::new());
        };

        let rows = db
            .query_all(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT name, bm25(memory_fts) AS score FROM memory_fts \
                 WHERE memory_fts MATCH ? AND project_id = ? ORDER BY score",
                [match_expr.into(), project_id.into()],
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

    /// 子串回退检索：本租户内实体名 / 类型 / 观测内容任一命中即算命中。
    async fn like_names(
        db: &DatabaseConnection,
        project_id: &str,
        query: &str,
    ) -> Result<Vec<String>, StorageError> {
        let pattern = like_pattern(query);
        let rows = db
            .query_all(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT n FROM (\
                    SELECT name AS n FROM entities \
                        WHERE project_id = ? \
                          AND (name LIKE ? ESCAPE '\\' OR entity_type LIKE ? ESCAPE '\\') \
                    UNION \
                    SELECT entity_name AS n FROM observations \
                        WHERE project_id = ? AND content LIKE ? ESCAPE '\\'\
                 ) ORDER BY n",
                [
                    project_id.into(),
                    pattern.clone().into(),
                    pattern.clone().into(),
                    project_id.into(),
                    pattern.into(),
                ],
            ))
            .await?;

        let mut names = Vec::new();
        for row in rows {
            names.push(row.try_get::<String>("", "n")?);
        }
        Ok(names)
    }

    /// 批量取本租户的实体明细（含观测），按 name 升序；不存在的名字跳过。
    async fn fetch_entities(&self, names: &[String]) -> Result<Vec<Entity>, StorageError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }

        let entity_rows = entities::Entity::find()
            .filter(entities::Column::ProjectId.eq(self.project_id.as_str()))
            .filter(entities::Column::Name.is_in(names.to_vec()))
            .order_by_asc(entities::Column::Name)
            .all(&self.db)
            .await?;
        if entity_rows.is_empty() {
            return Ok(Vec::new());
        }

        let observation_rows = observations::Entity::find()
            .filter(observations::Column::ProjectId.eq(self.project_id.as_str()))
            .filter(observations::Column::EntityName.is_in(names.to_vec()))
            .order_by_asc(observations::Column::Id)
            .all(&self.db)
            .await?;

        let mut grouped: HashMap<String, Vec<Observation>> = HashMap::new();
        for o in observation_rows {
            grouped
                .entry(o.entity_name.clone())
                .or_default()
                .push(Observation {
                    id: o.id,
                    entity_name: o.entity_name,
                    content: o.content,
                    created_at: o.created_at,
                    source: o.source,
                });
        }

        Ok(entity_rows
            .into_iter()
            .map(|e| Entity {
                observations: grouped.remove(&e.name).unwrap_or_default(),
                name: e.name,
                entity_type: e.entity_type,
                created_at: e.created_at,
                source: e.source,
            })
            .collect())
    }

    /// 按给定名字顺序排列实体（保持检索相关度序）。
    async fn fetch_ordered(&self, ordered_names: &[String]) -> Result<Vec<Entity>, StorageError> {
        let mut loaded = self.fetch_entities(ordered_names).await?;
        let mut index: HashMap<String, Entity> =
            loaded.drain(..).map(|e| (e.name.clone(), e)).collect();
        Ok(ordered_names
            .iter()
            .filter_map(|n| index.remove(n))
            .collect())
    }

    // ------------------------------------------------------------ 事务内写入

    async fn tx_create_entities(
        txn: &DatabaseTransaction,
        project_id: &str,
        normalized: Vec<(String, String, String)>,
    ) -> Result<(), StorageError> {
        let ts = now();
        for (name, entity_type, source) in normalized {
            let exists = entities::Entity::find()
                .filter(entities::Column::ProjectId.eq(project_id))
                .filter(entities::Column::Name.eq(name.as_str()))
                .one(txn)
                .await?
                .is_some();
            if exists {
                continue;
            }
            entities::Entity::insert(entities::ActiveModel {
                project_id: Set(project_id.to_string()),
                name: Set(name.clone()),
                entity_type: Set(entity_type.clone()),
                created_at: Set(ts),
                source: Set(source),
            })
            .exec(txn)
            .await?;

            // 占位索引行：让「只建了实体、暂未写观测」的名字也可被检索
            Self::index_row(txn, project_id, &name, &entity_type, FTS_ENTITY_SENTINEL).await?;
        }
        Ok(())
    }

    async fn tx_create_relations(
        txn: &DatabaseTransaction,
        project_id: &str,
        normalized: Vec<normalize::RelationKey>,
    ) -> Result<(), StorageError> {
        let ts = now();
        for (from_name, to_name, relation_type, source) in normalized {
            let exists = relations::Entity::find()
                .filter(relations::Column::ProjectId.eq(project_id))
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
                project_id: Set(project_id.to_string()),
                from_name: Set(from_name),
                to_name: Set(to_name),
                relation_type: Set(relation_type),
                created_at: Set(ts),
                source: Set(source),
                ..Default::default()
            })
            .exec(txn)
            .await?;
        }
        Ok(())
    }

    async fn tx_add_observations(
        txn: &DatabaseTransaction,
        project_id: &str,
        normalized: Vec<(String, Vec<String>, String)>,
    ) -> Result<(), StorageError> {
        for (entity_name, _, _) in &normalized {
            let exists = entities::Entity::find()
                .filter(entities::Column::ProjectId.eq(project_id))
                .filter(entities::Column::Name.eq(entity_name.as_str()))
                .one(txn)
                .await?
                .is_some();
            if !exists {
                return Err(StorageError::EntityNotFound(entity_name.clone()));
            }
        }

        let ts = now();
        for (entity_name, contents, source) in normalized {
            for content in contents {
                let result = observations::Entity::insert(observations::ActiveModel {
                    project_id: Set(project_id.to_string()),
                    entity_name: Set(entity_name.clone()),
                    content: Set(content.clone()),
                    created_at: Set(ts),
                    source: Set(source.clone()),
                    ..Default::default()
                })
                .exec(txn)
                .await?;

                Self::index_row(
                    txn,
                    project_id,
                    &entity_name,
                    &content,
                    result.last_insert_id,
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn tx_delete_entities(
        txn: &DatabaseTransaction,
        project_id: &str,
        names: &[String],
    ) -> Result<(), StorageError> {
        Self::unindex_names(txn, project_id, names).await?;

        observations::Entity::delete_many()
            .filter(observations::Column::ProjectId.eq(project_id))
            .filter(observations::Column::EntityName.is_in(names.to_vec()))
            .exec(txn)
            .await?;
        relations::Entity::delete_many()
            .filter(relations::Column::ProjectId.eq(project_id))
            .filter(relations::Column::FromName.is_in(names.to_vec()))
            .exec(txn)
            .await?;
        relations::Entity::delete_many()
            .filter(relations::Column::ProjectId.eq(project_id))
            .filter(relations::Column::ToName.is_in(names.to_vec()))
            .exec(txn)
            .await?;
        entities::Entity::delete_many()
            .filter(entities::Column::ProjectId.eq(project_id))
            .filter(entities::Column::Name.is_in(names.to_vec()))
            .exec(txn)
            .await?;
        Ok(())
    }

    async fn tx_delete_observations(
        txn: &DatabaseTransaction,
        project_id: &str,
        ids: &[i64],
    ) -> Result<(), StorageError> {
        for id in ids {
            // obs_id 全库唯一，但删除仍带租户条件：跨项目引用他人 id 必须是无操作的 no-op，
            // 而不是「恰好删到同名观测」的事故温床（纵深防御）
            txn.execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "DELETE FROM memory_fts WHERE obs_id = ? AND project_id = ?",
                [(*id).into(), project_id.into()],
            ))
            .await?;
        }
        observations::Entity::delete_many()
            .filter(observations::Column::ProjectId.eq(project_id))
            .filter(observations::Column::Id.is_in(ids.to_vec()))
            .exec(txn)
            .await?;
        Ok(())
    }

    async fn tx_delete_relations(
        txn: &DatabaseTransaction,
        project_id: &str,
        ids: &[i64],
    ) -> Result<(), StorageError> {
        relations::Entity::delete_many()
            .filter(relations::Column::ProjectId.eq(project_id))
            .filter(relations::Column::Id.is_in(ids.to_vec()))
            .exec(txn)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl MemoryRepository for SqliteSingleMemoryRepo {
    async fn create_entities(&self, entity_inputs: Vec<EntityInput>) -> Result<(), StorageError> {
        let normalized = normalize::entities(&entity_inputs)?;
        if normalized.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_create_entities(&txn, &self.project_id, normalized).await;
        Self::finish(txn, result).await
    }

    async fn create_relations(
        &self,
        relation_inputs: Vec<RelationInput>,
    ) -> Result<(), StorageError> {
        let normalized = normalize::relations(&relation_inputs)?;
        if normalized.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_create_relations(&txn, &self.project_id, normalized).await;
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
        let result = Self::tx_add_observations(&txn, &self.project_id, normalized).await;
        Self::finish(txn, result).await
    }

    async fn delete_entities(&self, names: &[String]) -> Result<(), StorageError> {
        let names: Vec<String> = names.iter().map(|n| n.trim().to_string()).collect();
        if names.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_delete_entities(&txn, &self.project_id, &names).await;
        Self::finish(txn, result).await
    }

    async fn delete_observations(&self, ids: &[i64]) -> Result<(), StorageError> {
        if ids.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_delete_observations(&txn, &self.project_id, ids).await;
        Self::finish(txn, result).await
    }

    async fn delete_relations(&self, ids: &[i64]) -> Result<(), StorageError> {
        if ids.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin().await?;
        let result = Self::tx_delete_relations(&txn, &self.project_id, ids).await;
        Self::finish(txn, result).await
    }

    async fn read_graph(&self) -> Result<Graph, StorageError> {
        let entity_rows = entities::Entity::find()
            .filter(entities::Column::ProjectId.eq(self.project_id.as_str()))
            .order_by_asc(entities::Column::Name)
            .all(&self.db)
            .await?;
        let observation_rows = observations::Entity::find()
            .filter(observations::Column::ProjectId.eq(self.project_id.as_str()))
            .order_by_asc(observations::Column::Id)
            .all(&self.db)
            .await?;
        let relation_rows = relations::Entity::find()
            .filter(relations::Column::ProjectId.eq(self.project_id.as_str()))
            .order_by_asc(relations::Column::Id)
            .all(&self.db)
            .await?;

        let mut grouped: HashMap<String, Vec<Observation>> = HashMap::new();
        for o in observation_rows {
            grouped
                .entry(o.entity_name.clone())
                .or_default()
                .push(Observation {
                    id: o.id,
                    entity_name: o.entity_name,
                    content: o.content,
                    created_at: o.created_at,
                    source: o.source,
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
                    source: e.source,
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
                    source: r.source,
                })
                .collect(),
        })
    }

    async fn search_nodes(&self, query: &str) -> Result<Vec<Entity>, StorageError> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }

        let hit_names = match Self::fts_names(&self.db, &self.project_id, q).await {
            Ok(names) if !names.is_empty() => names,
            Ok(_) => Self::like_names(&self.db, &self.project_id, q).await?,
            Err(err) => {
                tracing::warn!(query = q, error = %err, "FTS 检索不可用，退化为子串匹配");
                Self::like_names(&self.db, &self.project_id, q).await?
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

/// 单租户视图下的交接仓库（与记忆仓库共享同一个连接池）。
///
/// @intent 与文件后端同理：同库同连接但**不共用仓库对象**——两种写语义分开成对象，
///         「哪张表归谁」保持清晰；租户条件在**每个方法**里显式可见，便于审读。
pub struct SqliteSingleHandoffRepo {
    /// 所属项目（租户）
    project_id: String,
    /// 共享库连接（与记忆仓库同一份）
    db: DatabaseConnection,
}

impl SqliteSingleHandoffRepo {
    /// 行模型 → 领域模型（JSON 列在此还原成数组）。
    fn to_handoff(m: handoffs::Model) -> Result<Handoff, StorageError> {
        Ok(Handoff {
            task_id: m.task_id,
            revision: m.revision,
            goal: m.goal,
            state: m.state,
            next_steps: normalize::handoff_array_from_json(&m.next_steps)?,
            files: normalize::handoff_array_from_json(&m.files)?,
            decisions: normalize::handoff_array_from_json(&m.decisions)?,
            open_questions: normalize::handoff_array_from_json(&m.open_questions)?,
            client: m.client,
            session_id: m.session_id,
            created_at: m.created_at,
            acked_at: m.acked_at,
            acked_by: m.acked_by,
        })
    }

    /// 取本租户某任务的最新修订（连接与事务上都可用，故对 `ConnectionTrait` 泛型）。
    async fn load_latest_on<C: ConnectionTrait>(
        db: &C,
        project_id: &str,
        task_id: &str,
    ) -> Result<Option<Handoff>, StorageError> {
        let model = handoffs::Entity::find()
            .filter(handoffs::Column::ProjectId.eq(project_id))
            .filter(handoffs::Column::TaskId.eq(task_id))
            .order_by_desc(handoffs::Column::Revision)
            .one(db)
            .await?;
        model.map(Self::to_handoff).transpose()
    }

    /// 取本租户某任务的最新修订（本仓库连接）。
    async fn load_latest(&self, task_id: &str) -> Result<Option<Handoff>, StorageError> {
        Self::load_latest_on(&self.db, &self.project_id, task_id).await
    }

    /// 在事务中追加一条修订并读回。
    ///
    /// @intent 修订号用**单条 `INSERT ... SELECT COALESCE(MAX(revision),0)+1`** 分配
    ///         （与文件后端同构），WHERE 带 `project_id`：同名任务在不同项目里
    ///         各自从 1 计数，互不干扰。SQLite 的写事务天然串行，单语句即原子。
    async fn append_revision(
        txn: &DatabaseTransaction,
        project_id: &str,
        n: &normalize::NormalizedHandoff,
    ) -> Result<Handoff, StorageError> {
        let sql = "INSERT INTO handoffs (\
                project_id, task_id, revision, goal, state, next_steps, files, decisions, \
                open_questions, client, session_id, created_at, acked_at, acked_by\
            ) SELECT ?, ?, COALESCE(MAX(revision), 0) + 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, '' \
            FROM handoffs WHERE project_id = ? AND task_id = ?";
        txn.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            [
                project_id.into(),
                n.task_id.clone().into(),
                n.goal.clone().into(),
                n.state.clone().into(),
                normalize::handoff_array_to_json(&n.next_steps).into(),
                normalize::handoff_array_to_json(&n.files).into(),
                normalize::handoff_array_to_json(&n.decisions).into(),
                normalize::handoff_array_to_json(&n.open_questions).into(),
                n.client.clone().into(),
                n.session_id.clone().into(),
                now().into(),
                project_id.into(),
                n.task_id.clone().into(),
            ],
        ))
        .await?;

        Self::load_latest_on(txn, project_id, &n.task_id)
            .await?
            .ok_or_else(|| {
                StorageError::Backend(
                    "handoff insert succeeded but read-back found no revision".to_string(),
                )
            })
    }

    /// 本租户每任务最新修订的摘要列表。
    ///
    /// @intent 与文件后端同构：SQL 里 `ORDER BY ... LIMIT`，排序口径仍以共用的
    ///         [`normalize::order_handoff_summaries`] 为唯一真相（H11 只在两边都过它时成立）。
    ///         子查询与外层都带 `project_id`——单库里同名任务跨项目并存，缺一处就会串。
    async fn list_latest(
        &self,
        prefix: Option<&str>,
        unacked_only: bool,
        limit: usize,
    ) -> Result<Vec<HandoffSummary>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut sql = String::from(
            "SELECT h.* FROM handoffs h \
             JOIN (SELECT task_id, MAX(revision) AS revision FROM handoffs \
                   WHERE project_id = ? GROUP BY task_id) m \
               ON h.task_id = m.task_id AND h.revision = m.revision \
             WHERE h.project_id = ?",
        );
        let mut values: Vec<sea_orm::Value> = vec![self.project_id.clone().into()];
        sql.push_str("");
        values.push(self.project_id.clone().into());
        if unacked_only {
            sql.push_str(" AND h.acked_at IS NULL");
        }
        if let Some(p) = prefix {
            // 用 substr 而不是 LIKE：task_id 允许 `_`，而它在 LIKE 里是单字符通配符，
            // 会让前缀 `a_` 意外匹配到 `ab`。
            sql.push_str(" AND substr(h.task_id, 1, length(?)) = ?");
            values.push(p.to_string().into());
            values.push(p.to_string().into());
        }
        sql.push_str(" ORDER BY h.created_at DESC, h.task_id ASC");
        if limit != usize::MAX {
            sql.push_str(" LIMIT ?");
            values.push((limit as i64).into());
        }

        let models: Vec<handoffs::Model> = handoffs::Entity::find()
            .from_raw_sql(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                sql.as_str(),
                values,
            ))
            .all(&self.db)
            .await?;

        let full: Vec<Handoff> = models
            .into_iter()
            .map(Self::to_handoff)
            .collect::<Result<Vec<_>, _>>()?;
        let summaries = full.iter().map(normalize::handoff_summary).collect();
        Ok(normalize::order_handoff_summaries(summaries, limit))
    }
}

#[async_trait]
impl HandoffRepository for SqliteSingleHandoffRepo {
    async fn save_handoff(&self, input: HandoffInput) -> Result<Handoff, StorageError> {
        // 先整批校验、再落库：非法输入不产生半条修订（docs §3.5）
        let normalized = normalize::handoff(&input)?;

        let txn = self.db.begin().await?;
        match Self::append_revision(&txn, &self.project_id, &normalized).await {
            Ok(saved) => {
                txn.commit().await?;
                Ok(saved)
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    async fn resolve_handoff(
        &self,
        task_id: Option<&str>,
    ) -> Result<HandoffResolution, StorageError> {
        match task_id {
            Some(id) => Ok(match self.load_latest(id.trim()).await? {
                Some(h) => HandoffResolution::Found(Box::new(h)),
                None => HandoffResolution::None,
            }),
            None => {
                // 复用 list 的候选口径：resolve(None) 与「列表里看到的东西」必须同源
                let candidates = self.list_latest(None, true, usize::MAX).await?;
                match candidates.len() {
                    0 => Ok(HandoffResolution::None),
                    1 => {
                        let id = candidates[0].task_id.clone();
                        Ok(match self.load_latest(&id).await? {
                            Some(h) => HandoffResolution::Found(Box::new(h)),
                            // 并发删除的窄窗口：如实返回「没有了」，不 panic
                            None => HandoffResolution::None,
                        })
                    }
                    _ => Ok(HandoffResolution::Ambiguous(candidates)),
                }
            }
        }
    }

    async fn list_handoffs(
        &self,
        prefix: Option<&str>,
        unacked_only: bool,
        limit: usize,
    ) -> Result<Vec<HandoffSummary>, StorageError> {
        self.list_latest(prefix, unacked_only, limit).await
    }

    async fn ack_handoff(
        &self,
        task_id: Option<&str>,
        client: &str,
    ) -> Result<AckOutcome, StorageError> {
        let Some(id) = task_id else {
            // 无 ID：由共用规则决定「0 条什么都不做 / 1 条接它 / ≥2 条绝不替人猜」
            return crate::storage::ack_resolving_none(self, client).await;
        };

        let Some(current) = self.load_latest(id.trim()).await? else {
            return Ok(AckOutcome {
                resolution: HandoffResolution::None,
                won: false,
            });
        };
        if current.acked_at.is_some() {
            // 幂等：已接手时保持首次的 acked_at / acked_by 不变
            return Ok(AckOutcome {
                resolution: HandoffResolution::Found(Box::new(current)),
                won: false,
            });
        }

        // CAS：只有「本租户仍未被接手」的那条修订会被写中，受影响行数即「是否抢到」
        let affected = self
            .db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE handoffs SET acked_at = ?, acked_by = ? \
                 WHERE project_id = ? AND task_id = ? AND revision = ? AND acked_at IS NULL",
                [
                    now().into(),
                    client.trim().to_string().into(),
                    self.project_id.clone().into(),
                    current.task_id.clone().into(),
                    current.revision.into(),
                ],
            ))
            .await?
            .rows_affected();
        let won = affected == 1;

        // 抢输的一方也要看到「谁抢到了」：故一律读回当前最新修订
        let latest = self.load_latest(&current.task_id).await?.unwrap_or(current);
        Ok(AckOutcome {
            resolution: HandoffResolution::Found(Box::new(latest)),
            won,
        })
    }

    async fn delete_handoff(&self, task_id: &str) -> Result<(), StorageError> {
        // 幂等：不存在视为成功；带租户条件，同名任务在别的项目里不受影响
        self.db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "DELETE FROM handoffs WHERE project_id = ? AND task_id = ?",
                [
                    self.project_id.clone().into(),
                    task_id.trim().to_string().into(),
                ],
            ))
            .await?;
        Ok(())
    }
}

/// 单租户视图下的会话仓库（P5.1；与记忆 / 交接仓库共享同一个连接池）。
///
/// @intent 与文件后端同一套轮次分配手法（单条 `INSERT … SELECT MAX+1`），但**每个
///         SQL 都带租户条件**：轮次上限的 MAX 只统计本项目的行——否则项目 A 的
///         会话会把项目 B 的轮次序号顶起来；列表同样只看本租户。
pub struct SqliteSingleConversationRepo {
    /// 所属项目（租户）
    project_id: String,
    /// 共享库连接
    db: DatabaseConnection,
}

#[async_trait]
impl ConversationRepository for SqliteSingleConversationRepo {
    async fn append_turn(&self, input: SessionInput) -> Result<Turn, StorageError> {
        let (session_id, role, content) = normalize::session_turn(&input)?;
        let created_at = now();

        let res = self
            .db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO conversations (project_id, session_id, turn_index, role, content, \
                 created_at) \
                 SELECT ?, ?, COALESCE(MAX(turn_index) + 1, 0), ?, ?, ? \
                 FROM conversations WHERE project_id = ? AND session_id = ?",
                [
                    self.project_id.clone().into(),
                    session_id.clone().into(),
                    role.clone().into(),
                    content.clone().into(),
                    created_at.into(),
                    self.project_id.clone().into(),
                    session_id.clone().into(),
                ],
            ))
            .await?;
        if res.rows_affected() != 1 {
            return Err(StorageError::Backend(
                "conversation insert affected no rows".to_string(),
            ));
        }
        let turn_index: i64 = match conversations::Entity::find()
            .filter(conversations::Column::ProjectId.eq(&self.project_id))
            .filter(conversations::Column::SessionId.eq(&session_id))
            .order_by_desc(conversations::Column::TurnIndex)
            .one(&self.db)
            .await?
        {
            Some(m) => m.turn_index,
            None => {
                return Err(StorageError::Backend(
                    "conversation insert succeeded but read-back found no turn".to_string(),
                ))
            }
        };
        Ok(Turn {
            session_id,
            turn_index,
            role,
            content,
            created_at,
        })
    }

    async fn get_session(&self, session_id: &str) -> Result<Vec<Turn>, StorageError> {
        let models: Vec<conversations::Model> = conversations::Entity::find()
            .filter(conversations::Column::ProjectId.eq(&self.project_id))
            .filter(conversations::Column::SessionId.eq(session_id.trim()))
            .order_by_asc(conversations::Column::TurnIndex)
            .all(&self.db)
            .await?;
        Ok(models
            .into_iter()
            .map(|m| Turn {
                session_id: m.session_id,
                turn_index: m.turn_index,
                role: m.role,
                content: m.content,
                created_at: m.created_at,
            })
            .collect())
    }

    async fn list_sessions(&self, prefix: Option<&str>) -> Result<Vec<String>, StorageError> {
        // substr 而非 LIKE（session_id 允许 `_`）；租户条件永远在最前
        let mut sql = String::from(
            "SELECT session_id, MAX(created_at) AS last_at FROM conversations \
             WHERE project_id = ?",
        );
        if prefix.is_some() {
            sql.push_str(" AND substr(session_id, 1, length(?)) = ?");
        }
        sql.push_str(" GROUP BY session_id ORDER BY last_at DESC, session_id ASC");

        let mut params: Vec<sea_orm::Value> = vec![self.project_id.clone().into()];
        if let Some(p) = prefix {
            params.push(p.to_string().into());
            params.push(p.to_string().into());
        }
        let rows = self
            .db
            .query_all(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                &sql,
                params,
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.try_get::<String>("", "session_id").ok())
            .collect())
    }
}

/// SQLite 单库后端：所有项目共用一个连接池。
///
/// @intent 整库只有**一个**连接池、**一把**建池闸门——这是与文件后端最大的结构差异。
///         文件后端「按项目串行建池」，这里「整库串行建一次」，此后所有租户共享。
///         WAL 模式允许跨连接并发读 + 单写排队（sqlx 的 busy_timeout 兜底写竞争），
///         与文件后端在同一项目内的并发行为一致。
pub struct SqliteSingleBackend {
    /// 数据根目录（来自 DATA_DIR）
    data_dir: PathBuf,
    /// 全库唯一连接池；`None` = 尚未建立
    pool: Mutex<Option<DatabaseConnection>>,
    /// 建池闸门：整库只有一把，保证连接池只被创建一次
    gate: Mutex<()>,
}

impl SqliteSingleBackend {
    /// 构造后端。
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            pool: Mutex::new(None),
            gate: Mutex::new(()),
        }
    }

    /// 共享库文件路径：`{data_dir}/mem-single.db`。
    ///
    /// @intent 与文件后端的 `db_path(project_id)` 不同，本后端**只有这一个文件**；
    ///         项目目录 `data/{project_id}` 是文件后端的领地，两者互不重叠
    ///         （project_id 校验不含 `.`，目录名永远不会是 `mem-single.db`）。
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join(SINGLE_DB_FILE)
    }

    /// 建立并初始化共享库连接池（调用方必须已持有建池闸门）。
    async fn create_pool(&self) -> Result<DatabaseConnection, StorageError> {
        let path = self.db_path();
        std::fs::create_dir_all(&self.data_dir)?;

        let url = format!("sqlite://{}?mode=rwc", path.display());
        let db = Database::connect(&url).await?;

        // WAL + NORMAL：与文件后端同一取舍；journal_mode = WAL 需要独占锁，
        // 故本函数必须被闸门串行化保护
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
        init_single_db(&db).await?;
        Ok(db)
    }

    /// 组装仓库集合（两处返回点共用，避免字段漂移）。
    fn repos_with(&self, project_id: &str, db: DatabaseConnection) -> ProjectRepos {
        ProjectRepos {
            project_id: project_id.to_string(),
            backend: BackendKind::SqliteSingle,
            memory: Arc::new(SqliteSingleMemoryRepo {
                project_id: project_id.to_string(),
                db: db.clone(),
            }),
            handoff: Arc::new(SqliteSingleHandoffRepo {
                project_id: project_id.to_string(),
                db: db.clone(),
            }),
            conversation: Arc::new(SqliteSingleConversationRepo {
                project_id: project_id.to_string(),
                db,
            }),
        }
    }

    /// 取当前连接池；尚未建立则返回 `None`（快路径用）。
    async fn pooled(&self) -> Option<DatabaseConnection> {
        self.pool.lock().await.clone()
    }
}

#[async_trait]
impl StorageBackend for SqliteSingleBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::SqliteSingle
    }

    async fn repositories_for(&self, project_id: &str) -> Result<ProjectRepos, StorageError> {
        crate::storage::validate_project_id(project_id)?;

        // 快路径：池已就绪则直接发仓库
        if let Some(db) = self.pooled().await {
            return Ok(self.repos_with(project_id, db));
        }

        // 慢路径：整库串行建池（journal_mode = WAL 需要独占锁，见 create_pool）
        let _guard = self.gate.lock().await;

        // 双重检查：等待期间可能已有其他任务建好
        if let Some(db) = self.pooled().await {
            return Ok(self.repos_with(project_id, db));
        }

        let db = self.create_pool().await?;
        *self.pool.lock().await = Some(db.clone());

        Ok(self.repos_with(project_id, db))
    }

    async fn drop_project(&self, project_id: &str) -> Result<(), StorageError> {
        crate::storage::validate_project_id(project_id)?;

        let Some(db) = self.pooled().await else {
            // 库从未打开 = 该项目从未落过数据，幂等成功
            return Ok(());
        };

        // 删该租户的全部行；共享库文件保留——其他项目还住在里面。
        // FTS 索引一并清（按租户），避免留下可被检索的幽灵内容。
        // conversations 无 FTS，但 transcript 同样是项目数据，必须一并清。
        for table in [
            "entities",
            "observations",
            "relations",
            "handoffs",
            "conversations",
        ] {
            db.execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("DELETE FROM {table} WHERE project_id = ?"),
                [project_id.into()],
            ))
            .await?;
        }
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM memory_fts WHERE project_id = ?",
            [project_id.into()],
        ))
        .await?;
        Ok(())
    }

    fn db_path_label(&self, _project_id: &str) -> String {
        // 单库形态：所有项目共用同一文件，登记值与项目无关
        SINGLE_DB_FILE.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EntityInput, HandoffInput};
    use crate::storage::contract;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 构造隔离的临时数据目录。
    fn temp_data_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("memora_p6_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 记忆 / 交接 / 后端三级契约必须与内存后端、文件后端行为完全等价。
    #[tokio::test]
    async fn sqlite_single_satisfies_memory_contract() {
        let backend = SqliteSingleBackend::new(temp_data_dir());
        let repos = backend.repositories_for("contract").await.unwrap();
        contract::run_all(&*repos.memory).await;
    }

    #[tokio::test]
    async fn sqlite_single_satisfies_handoff_contract() {
        let backend = SqliteSingleBackend::new(temp_data_dir());
        let repos = backend.repositories_for("handoff-contract").await.unwrap();
        contract::run_handoff_contract(&*repos.handoff).await;
    }

    #[tokio::test]
    async fn sqlite_single_satisfies_conversation_contract() {
        let backend = SqliteSingleBackend::new(temp_data_dir());
        let repos = backend
            .repositories_for("conversation-contract")
            .await
            .unwrap();
        contract::run_conversation_contract(&*repos.conversation).await;
    }

    #[tokio::test]
    async fn sqlite_single_satisfies_backend_contract() {
        let backend = SqliteSingleBackend::new(temp_data_dir());
        contract::run_backend_contract(&backend).await;
    }

    /// 单库 + 租户列：项目隔离必须与文件后端一样是**硬**隔离（同名实体并存互不可见）。
    #[tokio::test]
    async fn tenant_filtering_is_hard_isolation() {
        let backend = SqliteSingleBackend::new(temp_data_dir());
        let a = backend.repositories_for("proj-a").await.unwrap();
        let b = backend.repositories_for("proj-b").await.unwrap();

        a.memory
            .create_entities(vec![EntityInput::new("same-name", "person")])
            .await
            .unwrap();
        // 同名实体在另一个项目里独立存在，互不可见
        b.memory
            .create_entities(vec![EntityInput::new("same-name", "note")])
            .await
            .unwrap();

        let ga = a.memory.read_graph().await.unwrap();
        let gb = b.memory.read_graph().await.unwrap();
        assert_eq!(ga.entity_count(), 1);
        assert_eq!(gb.entity_count(), 1);
        assert_eq!(ga.entities[0].entity_type, "person");
        assert_eq!(gb.entities[0].entity_type, "note");
    }

    /// 跨项目引用他人观测 id 的删除必须被租户条件挡住（id 是全库自增，非项目内）。
    #[tokio::test]
    async fn cross_tenant_observation_id_is_invisible() {
        let backend = SqliteSingleBackend::new(temp_data_dir());
        let a = backend.repositories_for("ten-a").await.unwrap();
        let b = backend.repositories_for("ten-b").await.unwrap();

        a.memory
            .create_entities(vec![EntityInput::new("e", "note")])
            .await
            .unwrap();
        a.memory
            .add_observations(vec![ObservationInput::single("e", "a 的秘密")])
            .await
            .unwrap();
        let ga = a.memory.read_graph().await.unwrap();
        let foreign_id = ga.entities[0].observations[0].id;

        b.memory
            .create_entities(vec![EntityInput::new("e", "note")])
            .await
            .unwrap();
        // B 拿 A 的观测 id 来删：不得影响 A 的数据
        b.memory.delete_observations(&[foreign_id]).await.unwrap();

        let ga_after = a.memory.read_graph().await.unwrap();
        assert_eq!(
            ga_after.entities[0].observations.len(),
            1,
            "跨租户的观测 id 不得删到别人的数据"
        );
        assert_eq!(ga_after.entities[0].observations[0].content, "a 的秘密");
    }

    /// 检索（FTS + 子串回退）都只能命中本租户的内容。
    #[tokio::test]
    async fn search_is_scoped_to_tenant() {
        let backend = SqliteSingleBackend::new(temp_data_dir());
        let a = backend.repositories_for("sea-a").await.unwrap();
        let b = backend.repositories_for("sea-b").await.unwrap();

        a.memory
            .create_entities(vec![EntityInput::new("深色主题", "preference")])
            .await
            .unwrap();
        a.memory
            .add_observations(vec![ObservationInput::single("深色主题", "偏好深色界面")])
            .await
            .unwrap();

        b.memory
            .create_entities(vec![EntityInput::new("unrelated", "note")])
            .await
            .unwrap();

        let hits_a = a.memory.search_nodes("深色").await.unwrap();
        assert_eq!(hits_a.len(), 1, "本租户必须能检索到");
        let hits_b = b.memory.search_nodes("深色").await.unwrap();
        assert!(hits_b.is_empty(), "其他租户不得检索到本租户内容");
    }

    /// 会话：同名 session_id 在不同项目里必须是**两个会话**，轮次序号各自独立。
    ///
    /// @intent 会话寻址键是 `session_id`，而客户端生成 id 时并不知道别的项目用了什么——
    ///         单库里两个项目撞名是常态而非异常，MAX(turn_index) 的租户条件漏一处
    ///         就会把别人项目的轮次顶起来，这里直接钉死。
    #[tokio::test]
    async fn conversations_are_scoped_to_tenant() {
        use crate::domain::SessionInput;

        let backend = SqliteSingleBackend::new(temp_data_dir());
        let a = backend.repositories_for("con-a").await.unwrap();
        let b = backend.repositories_for("con-b").await.unwrap();

        // 两项目写同名会话：A 三轮、B 一轮
        for content in ["a-1", "a-2", "a-3"] {
            a.conversation
                .append_turn(SessionInput {
                    session_id: "shared-session".to_string(),
                    role: "user".to_string(),
                    content: content.to_string(),
                })
                .await
                .unwrap();
        }
        b.conversation
            .append_turn(SessionInput {
                session_id: "shared-session".to_string(),
                role: "user".to_string(),
                content: "b-1".to_string(),
            })
            .await
            .unwrap();

        let ta = a.conversation.get_session("shared-session").await.unwrap();
        let tb = b.conversation.get_session("shared-session").await.unwrap();
        assert_eq!(
            ta.iter().map(|t| t.content.as_str()).collect::<Vec<_>>(),
            vec!["a-1", "a-2", "a-3"],
            "A 的 transcript 不受 B 干扰"
        );
        assert_eq!(
            tb.iter()
                .map(|t| (t.turn_index, t.content.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "b-1")],
            "B 的轮次序号必须独立从 0 起（MAX 只统计本租户）"
        );

        // 列表同样只见本租户的会话
        assert_eq!(
            a.conversation.list_sessions(Some("shared")).await.unwrap(),
            vec!["shared-session".to_string()],
        );
        assert_eq!(
            b.conversation.list_sessions(None).await.unwrap(),
            vec!["shared-session".to_string()],
        );

        // drop A 项目：B 的会话必须原样保留（租户删行、共享库留存）
        backend.drop_project("con-a").await.unwrap();
        assert_eq!(
            b.conversation
                .get_session("shared-session")
                .await
                .unwrap()
                .len(),
            1,
            "drop 掉的是 A 的租户行，B 的会话不得被连带删除"
        );
    }

    /// 交接：同名任务（含零 ID 槽位）在不同项目里互不可见、各自独立修订。
    #[tokio::test]
    async fn handoffs_are_scoped_to_tenant() {
        let backend = SqliteSingleBackend::new(temp_data_dir());
        let a = backend.repositories_for("ho-a").await.unwrap();
        let b = backend.repositories_for("ho-b").await.unwrap();

        let mut req = HandoffInput::new("a 的活", "进行中");
        req.client = "zed".to_string();
        let saved_a = a.handoff.save_handoff(req).await.unwrap();
        assert_eq!(saved_a.task_id, "current@zed");

        // B 项目用同一个写入方零 ID save：不得落到 A 的槽位上（应各自独立成任务）
        let mut req_b = HandoffInput::new("b 的活", "进行中");
        req_b.client = "zed".to_string();
        let saved_b = b.handoff.save_handoff(req_b).await.unwrap();
        assert_eq!(saved_b.task_id, "current@zed");

        // 各自 resolve 都只看到自己那一条，修订号各自从 1 开始
        match a.handoff.resolve_handoff(None).await.unwrap() {
            crate::domain::HandoffResolution::Found(h) => {
                assert_eq!(h.goal, "a 的活");
                assert_eq!(h.revision, 1);
            }
            other => panic!("A 应解析到自己唯一的待接手，实际 {other:?}"),
        }
        match b.handoff.resolve_handoff(None).await.unwrap() {
            crate::domain::HandoffResolution::Found(h) => {
                assert_eq!(h.goal, "b 的活");
                assert_eq!(h.revision, 1, "同名任务在 B 项目必须独立计修订号");
            }
            other => panic!("B 应解析到自己唯一的待接手，实际 {other:?}"),
        }
    }

    /// 单库被多项目共享：并发写入（含跨项目）必须全部成功且不丢数据。
    ///
    /// @intent 与文件后端同一条 P4.2 铁律：SQLite 单写者模型 + multi_thread runtime，
    ///         并发写要在真实并发下验证，串行用例会假绿。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_across_tenants_all_succeed() {
        const WRITERS: usize = 12;

        let backend = std::sync::Arc::new(SqliteSingleBackend::new(temp_data_dir()));
        let mut handles = Vec::with_capacity(WRITERS);
        for i in 0..WRITERS {
            let backend = backend.clone();
            handles.push(tokio::spawn(async move {
                let project = if i % 2 == 0 { "conc-a" } else { "conc-b" };
                let repos = backend.repositories_for(project).await.unwrap();
                repos
                    .memory
                    .create_entities(vec![EntityInput::new(format!("w{i}"), "concept")])
                    .await
            }));
        }
        for (i, handle) in handles.into_iter().enumerate() {
            handle
                .await
                .expect("任务不得 panic")
                .unwrap_or_else(|err| panic!("第 {i} 个并发写入失败: {err}"));
        }

        // 两个项目各 6 条，互不可见
        for (project, count) in [("conc-a", 6), ("conc-b", 6)] {
            let repos = backend.repositories_for(project).await.unwrap();
            assert_eq!(
                repos.memory.read_graph().await.unwrap().entity_count(),
                count,
                "{project} 必须保有全部 {count} 条写入"
            );
        }
    }

    /// 并发首次取用不同项目：共享连接池只建一次（闸门串行 + 双重检查）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_access_creates_a_single_pool() {
        const CALLERS: usize = 8;

        let backend = std::sync::Arc::new(SqliteSingleBackend::new(temp_data_dir()));
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

    /// 丢弃项目 = 删该租户的全部行，**共享库文件必须保留**（其他项目还住在里面）。
    #[tokio::test]
    async fn drop_project_deletes_rows_but_keeps_shared_db() {
        let backend = SqliteSingleBackend::new(temp_data_dir());
        for id in ["keep", "gone"] {
            let repos = backend.repositories_for(id).await.unwrap();
            repos
                .memory
                .create_entities(vec![EntityInput::new("x", "person")])
                .await
                .unwrap();
            repos
                .handoff
                .save_handoff(HandoffInput::new(format!("{id} 的活"), "待接").with_task("t1"))
                .await
                .unwrap();
        }

        let shared = backend.db_path();
        assert!(shared.exists(), "前置：共享库应已落盘");

        backend.drop_project("gone").await.unwrap();

        let keep = backend.repositories_for("keep").await.unwrap();
        assert_eq!(
            keep.memory.read_graph().await.unwrap().entity_count(),
            1,
            "其他项目的记忆必须完好"
        );
        match keep.handoff.resolve_handoff(Some("t1")).await.unwrap() {
            crate::domain::HandoffResolution::Found(h) => assert_eq!(h.goal, "keep 的活"),
            other => panic!("其他项目的交接不得被误删，实际 {other:?}"),
        }
        assert!(shared.exists(), "共享库文件必须保留（其他项目还在用）");

        // gone 重新取用后必须为空
        let gone = backend.repositories_for("gone").await.unwrap();
        assert_eq!(gone.memory.read_graph().await.unwrap().entity_count(), 0);
        assert!(matches!(
            gone.handoff.resolve_handoff(Some("t1")).await.unwrap(),
            crate::domain::HandoffResolution::None
        ));
    }

    /// 非法 project_id 依旧拒绝：租户隔离的入口校验与文件后端保持同一口径。
    #[tokio::test]
    async fn rejects_unsafe_project_id() {
        let backend = SqliteSingleBackend::new(temp_data_dir());
        assert!(backend.repositories_for("../evil").await.is_err());
        assert!(backend.repositories_for("a/b").await.is_err());
    }

    /// 建表语句必须幂等（重复初始化共享库不应报错）。
    #[tokio::test]
    async fn init_single_db_is_idempotent() {
        let dir = temp_data_dir();
        let backend = SqliteSingleBackend::new(dir);
        backend.repositories_for("twice").await.unwrap();
        let db = Database::connect(format!("sqlite://{}?mode=rwc", backend.db_path().display()))
            .await
            .unwrap();
        init_single_db(&db).await.unwrap();
        init_single_db(&db).await.unwrap();
        assert_eq!(backend.kind(), BackendKind::SqliteSingle);
    }
}
