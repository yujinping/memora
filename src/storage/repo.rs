//! 记忆仓库抽象（后端无关契约）。
//!
//! @author yujinping
//! @intent P2：以 Repository 模式屏蔽存储实现与 SQL 方言。MCP 工具层（P3）与
//!          REST 管理层（P4）只持有 `Arc<dyn MemoryRepository>`，不引用 sea-orm 实体，
//!          因而「换后端」的改动点收敛在后端 impl 内部（见 docs §12.5）。

use crate::domain::{Entity, EntityInput, Graph, ObservationInput, RelationInput};
use crate::error::StorageError;

/// 记忆领域仓库：实体 / 观测 / 关系的全部读写入口。
///
/// @intent 方法语义对齐官方 MCP Memory 的 9 个工具，一处实现即可直接映射为 MCP 工具。
///         实现方必须满足 `SqliteFileBackend` 与 `InMemBackend` 共享的契约测试
///         （见 `crate::storage::contract`）。
#[async_trait::async_trait]
pub trait MemoryRepository: Send + Sync {
    /// 批量创建实体；同名实体已存在时忽略（不覆盖原类型），整体幂等。
    async fn create_entities(&self, entities: Vec<EntityInput>) -> Result<(), StorageError>;

    /// 批量创建关系；(from, to, relation_type) 三元组重复时跳过。
    async fn create_relations(&self, relations: Vec<RelationInput>) -> Result<(), StorageError>;

    /// 为已存在实体追加观测；实体不存在时报 `EntityNotFound`。
    async fn add_observations(
        &self,
        observations: Vec<ObservationInput>,
    ) -> Result<(), StorageError>;

    /// 按名删除实体，并级联删除其观测、关联关系与检索索引。
    async fn delete_entities(&self, names: &[String]) -> Result<(), StorageError>;

    /// 按 id 删除观测（不存在时静默忽略，保证幂等）。
    async fn delete_observations(&self, ids: &[i64]) -> Result<(), StorageError>;

    /// 按 id 删除关系（不存在时静默忽略，保证幂等）。
    async fn delete_relations(&self, ids: &[i64]) -> Result<(), StorageError>;

    /// 返回全图快照（实体按 name 升序，关系按 id 升序）。
    async fn read_graph(&self) -> Result<Graph, StorageError>;

    /// 关键词检索：命中实体名、实体类型或观测内容，返回实体明细（含观测）。
    ///
    /// @intent 检索方言（FTS5 / tsvector / 内存子串）锁死在各后端实现内部，
    ///         调用方只约定「全词命中 + 相关度排序」这一行为契约。
    async fn search_nodes(&self, query: &str) -> Result<Vec<Entity>, StorageError>;

    /// 按名批量取实体明细；未命中的名字直接跳过。
    async fn open_nodes(&self, names: &[String]) -> Result<Vec<Entity>, StorageError>;
}
