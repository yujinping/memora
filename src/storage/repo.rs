//! 记忆仓库抽象（后端无关契约）。
//!
//! @author yujinping
//! @intent P2：以 Repository 模式屏蔽存储实现与 SQL 方言。MCP 工具层（P3）与
//!          REST 管理层（P4）只持有 `Arc<dyn MemoryRepository>`，不引用 sea-orm 实体，
//!          因而「换后端」的改动点收敛在后端 impl 内部（见 docs §12.5）。

use crate::domain::{
    AckOutcome, Entity, EntityInput, Graph, Handoff, HandoffInput, HandoffResolution,
    HandoffSummary, ObservationInput, RelationInput, SessionInput, Turn,
};
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

/// 交接仓库：跨客户端 / 跨机器的**过程状态**读写入口（P4.3）。
///
/// @intent 与 [`MemoryRepository`] 并列而非合并：交接是「追加修订」语义，记忆是
///         「幂等合并」语义，两者对同一张表的要求相反（见 docs §3.4）。
///         实现方必须同时通过 `crate::storage::contract::run_handoff_contract`，
///         保证 `SqliteFileBackend` 与 `InMemBackend` 行为等价。
///         `resolve_handoff` 是 load / ack 的**唯一寻址入口**（见 docs §4.1）。
#[async_trait::async_trait]
pub trait HandoffRepository: Send + Sync {
    /// 追加一条不可变修订，返回落库后的完整记录。
    ///
    /// @intent `input.task_id == None` 时落到 `current@<client>` 分片槽位（docs §3.7）；
    ///         `revision` 由后端分配（从 1 单调递增）；新修订的 `acked_at` 恒为 `None`
    ///         ——内容变了，上一次接手不算数（docs §7.2）。
    async fn save_handoff(&self, input: HandoffInput) -> Result<Handoff, StorageError>;

    /// 解析一条交接。
    ///
    /// - `task_id == Some(id)`：精确取该任务的**最新修订**（不论是否已接手）；
    /// - `task_id == None`：取「待接手」的那一条，规则见 docs §3.7 R1
    ///   （候选 = 每任务最新修订中未接手的，按 `created_at` 降序、`task_id` 升序；
    ///   0 条 → [`HandoffResolution::None`]，1 条 → [`HandoffResolution::Found`]，
    ///   ≥2 条 → [`HandoffResolution::Ambiguous`]）。
    ///
    /// 三种结果都是**正常返回**，没有一种是错误。
    async fn resolve_handoff(
        &self,
        task_id: Option<&str>,
    ) -> Result<HandoffResolution, StorageError>;

    /// 列出任务摘要（每个任务只给最新修订）。
    ///
    /// - `unacked_only = true`：只列待接手的（「有哪些活等着我」）；
    /// - `prefix`：按 `task_id` 前缀过滤，不影响排序口径；
    /// - `limit`：最多返回多少条；`0` 表示**返回空集**（不是「不限」，后者是静默陷阱）；
    /// - 排序：`created_at` 降序，同秒时 `task_id` 升序。
    ///
    /// 注意**截断必须发生在排序之后**：先 LIMIT 再排序会返回最旧的几条，
    /// 而这个错误在小数据量下测不出来（docs §4.3 H11）。
    async fn list_handoffs(
        &self,
        prefix: Option<&str>,
        unacked_only: bool,
        limit: usize,
    ) -> Result<Vec<HandoffSummary>, StorageError>;

    /// 标记接手，寻址规则与 [`HandoffRepository::resolve_handoff`] 完全一致（同样接受 `None`）。
    ///
    /// @intent `won` 说明本次调用是否真的抢到了接手权。已接手时为**幂等空操作**：
    ///         `won = false` 且 `acked_at` / `acked_by` 保持首次值不变（docs §4.4）。
    ///         多个客户端同时接手时，后到者据此决定停下来问人，而不是同时改同一份代码。
    async fn ack_handoff(
        &self,
        task_id: Option<&str>,
        client: &str,
    ) -> Result<AckOutcome, StorageError>;

    /// 删除某任务的**全部修订**（幂等；不存在视为成功）。
    async fn delete_handoff(&self, task_id: &str) -> Result<(), StorageError>;
}

/// 会话领域仓库（P5.1，docs/memory-system-design.md §5.4）：细粒度 transcript 的读写入口。
///
/// @intent 与 handoff（结论级交接）互补：会话工具存**完整逐轮记录**，供新机器上
///         原样拉回一段对话。仓库仍按项目隔离——`repositories_for(project_id)`
///         返回的实例天然只见本项目的行，实现方不得在本 trait 方法里再收 project 参数。
#[async_trait::async_trait]
pub trait ConversationRepository: Send + Sync {
    /// 追加一轮对话，`turn_index` 由后端分配（会话内从 0 单调递增、无空洞），
    /// 返回落库后的完整轮次（含分配出的 `turn_index` / `created_at`）。
    ///
    /// @intent 回读是硬约定：MCP 工具层直接把返回值交给调用方续接，
    ///         不允许「写完再猜序号」。会话不存在也**不是错误**——首写即建会话。
    async fn append_turn(&self, input: SessionInput) -> Result<Turn, StorageError>;

    /// 拉回完整会话 transcript，按 `turn_index` 升序。
    /// 会话不存在（或从未写入）返回**空数组**——空 transcript 是合法答案，不是错误。
    async fn get_session(&self, session_id: &str) -> Result<Vec<Turn>, StorageError>;

    /// 列出会话标识：有 `prefix` 时按前缀过滤，按**最近活跃降序**、
    /// 并列时按 `session_id` 升序（确定性排序，供恢复方「先接最近的」）。
    async fn list_sessions(&self, prefix: Option<&str>) -> Result<Vec<String>, StorageError>;
}
