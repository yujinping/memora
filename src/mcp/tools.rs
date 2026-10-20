//! 9 个 MCP 工具的后端无关语义实现。
//!
//! @author yujinping
//! @intent P3：把「协议适配」与「工具语义」拆开——本模块只依赖 `MemoryRepository`
//!         与入参 / 出参契约，可脱离 MCP 传输层用内存后端做单测；
//!         协议侧的宏、会话、HTTP 细节全部留在 `mcp::server`。
//!         这也是 docs §12.5「handler 层对存储实现完全盲视」在 MCP 侧的落点。

use crate::domain::{
    EntityInput, HandoffInput, HandoffResolution, ObservationInput, RelationInput, SessionInput,
};
use crate::error::StorageError;
use crate::mcp::dto::*;
use crate::storage::{normalize, ConversationRepository, HandoffRepository, MemoryRepository};
use rmcp::ErrorData;

/// 官方对齐的记忆工具名（顺序即 `tools/list` 的稳定顺序）。
///
/// @intent 这 9 个名字**一个都不改**：它们是「与官方 MCP Memory 语义对齐」的承诺，
///         客户端与既有提示词据此零改造接入。交接工具另立家族，见下。
pub const MEMORY_TOOL_NAMES: [&str; 9] = [
    "create_entities",
    "create_relations",
    "add_observations",
    "delete_entities",
    "delete_observations",
    "delete_relations",
    "read_graph",
    "search_nodes",
    "open_nodes",
];

/// 交接工具名（家族前缀 `handoff_*`）。
///
/// @intent 用家族前缀而非续用「动词_名词」：工具面从「单一官方家族」扩为两个家族，
///         前缀让模型与人在 `tools/list` 里一眼分组（docs §5.1）。
pub const HANDOFF_TOOL_NAMES: [&str; 4] = [
    "handoff_save",
    "handoff_load",
    "handoff_list",
    "handoff_ack",
];

/// 会话工具名（P5.1）：细粒度逐轮 transcript 的读写。
///
/// @intent 与 handoff 同一命名纪律——家族前缀 `session` 不需要，这三个名字本身就是
///         「会话」语义；关键是与官方 9 工具的「动词_名词」分属不同语义域。
pub const SESSION_TOOL_NAMES: [&str; 3] = ["log_turn", "get_session", "list_sessions"];

/// 对外承诺的完整工具面 = 三个家族之和（供 `mcp::mod::assert_tool_manifest` 使用）。
///
/// @intent 用**常量索引**拼出来而不是再抄一遍 16 个字符串：抄一遍就多一个漂移点，
///         而漂移的后果是「启动自检通过、但清单与实际不符」——自检本身失去意义。
pub const TOOL_NAMES: [&str; 16] = [
    MEMORY_TOOL_NAMES[0],
    MEMORY_TOOL_NAMES[1],
    MEMORY_TOOL_NAMES[2],
    MEMORY_TOOL_NAMES[3],
    MEMORY_TOOL_NAMES[4],
    MEMORY_TOOL_NAMES[5],
    MEMORY_TOOL_NAMES[6],
    MEMORY_TOOL_NAMES[7],
    MEMORY_TOOL_NAMES[8],
    HANDOFF_TOOL_NAMES[0],
    HANDOFF_TOOL_NAMES[1],
    HANDOFF_TOOL_NAMES[2],
    HANDOFF_TOOL_NAMES[3],
    SESSION_TOOL_NAMES[0],
    SESSION_TOOL_NAMES[1],
    SESSION_TOOL_NAMES[2],
];

/// 服务端故障对 MCP 客户端暴露的通用文案（不含任何内部细节）。
///
/// @intent 后端原始报错（表名 / SQL 片段 / 文件路径）只进服务端日志，绝不回传给客户端——
///         否则等于把内部拓扑免费送去做侦察。`Backend` 分支据此返回固定文案。
const INTERNAL_ERROR_MESSAGE: &str = "internal server error";

/// 存储错误 → 工具错误。
///
/// @intent 区分「调用方可修正」与「服务端故障」：入参非法、实体不存在属于前者（-32602），
///         后端不可用 / 项目 id 非法属于后者（-32603）。调用方据此决定是否重试或改参数。
///         其中 `Backend` 携带的是底层数据库原始报错（`StorageError` 已将其转成字符串），
///         含表名 / SQL / 路径等内部细节——**只记录到服务端日志，绝不回传客户端**。
fn tool_error(err: StorageError) -> ErrorData {
    match err {
        StorageError::InvalidInput(m) => ErrorData::invalid_params(m, None),
        StorageError::EntityNotFound(n) => {
            ErrorData::invalid_params(format!("entity not found: {n}"), None)
        }
        StorageError::InvalidProjectId(p) => {
            ErrorData::internal_error(format!("invalid project id: {p}"), None)
        }
        StorageError::Unsupported(k) => {
            ErrorData::internal_error(format!("unsupported storage backend: {k}"), None)
        }
        StorageError::Backend(m) => {
            tracing::error!(error = %m, "storage backend error; see server log for details");
            ErrorData::internal_error(INTERNAL_ERROR_MESSAGE.to_string(), None)
        }
    }
}

/// 实体领域模型 → 出参视图。
fn entity_view(e: crate::domain::Entity) -> EntityView {
    EntityView {
        name: e.name,
        entity_type: e.entity_type,
        created_at: e.created_at,
        source: e.source,
        observations: e
            .observations
            .into_iter()
            .map(|o| ObservationView {
                id: o.id,
                content: o.content,
                created_at: o.created_at,
                source: o.source,
            })
            .collect(),
    }
}

/// 关系领域模型 → 出参视图。
fn relation_view(r: crate::domain::Relation) -> RelationView {
    RelationView {
        id: r.id,
        from_name: r.from_name,
        to_name: r.to_name,
        relation_type: r.relation_type,
        created_at: r.created_at,
        source: r.source,
    }
}

/// 去重但保持首次出现顺序（用于 id 列表；名字列表走 `storage::normalize::name_list`）。
fn dedup_ids(ids: &[i64]) -> Vec<i64> {
    let mut seen = std::collections::HashSet::new();
    ids.iter().copied().filter(|i| seen.insert(*i)).collect()
}

/// 名字索引类查询的结果按 name 升序转成视图。
///
/// @intent `open_nodes` 在 SQL 侧是 `name IN (...)`，返回顺序不由后端保证；统一排序后，
///         工具的对外输出与后端实现无关（契约等价性的又一落点）。
///         `search_nodes` 不适用——相关度顺序本身就是结果的一部分。
fn sorted_entities(mut entities: Vec<crate::domain::Entity>) -> Vec<EntityView> {
    entities.sort_by(|a, b| a.name.cmp(&b.name));
    entities.into_iter().map(entity_view).collect()
}

/// `create_entities`：批量建实体（同名忽略），返回请求名在调用后的状态。
///
/// @intent 入参可携带 `observations`（对齐官方 schema 的「一次调用建实体并附观测」）：
///         建完实体后紧跟一次 `add_observations`，来源沿用实体的 `source`。
///         两个批次的校验都在**任何写入之前**完成——否则非法观测会留下
///         「实体已建、观测没写」的半成品，而调用方只看到一次报错。
pub async fn create_entities(
    repo: &dyn MemoryRepository,
    req: CreateEntitiesRequest,
) -> Result<EntitiesResponse, ErrorData> {
    let inputs: Vec<EntityInput> = req
        .entities
        .iter()
        .map(|e| {
            EntityInput::new(e.name.clone(), e.entity_type.clone()).with_source(e.source.clone())
        })
        .collect();

    // 随实体一起给出的观测：同一次调用视为同一来源，故沿用该实体的 `source`。
    let carried: Vec<ObservationInput> = req
        .entities
        .iter()
        .filter(|e| !e.observations.is_empty())
        .map(|e| {
            ObservationInput::new(e.name.clone(), e.observations.clone())
                .with_source(e.source.clone())
        })
        .collect();

    // 先用共享规范化器取「规范名」，再据此回读：写路径与读路径必须用同一套去空白规则，
    // 否则「写入带空白名字 → 按原样回读」会漏命中。
    let names: Vec<String> = normalize::entities(&inputs)
        .map_err(tool_error)?
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();

    if !carried.is_empty() {
        normalize::observations(&carried).map_err(tool_error)?;
    }

    repo.create_entities(inputs).await.map_err(tool_error)?;
    if !carried.is_empty() {
        repo.add_observations(carried).await.map_err(tool_error)?;
    }

    let entities = repo
        .open_nodes(&normalize::name_list(&names))
        .await
        .map_err(tool_error)?;
    Ok(EntitiesResponse {
        entities: sorted_entities(entities),
    })
}

/// `create_relations`：批量建关系（三元组去重），返回实际落库的关系（含 id）。
pub async fn create_relations(
    repo: &dyn MemoryRepository,
    req: CreateRelationsRequest,
) -> Result<RelationsResponse, ErrorData> {
    let inputs: Vec<RelationInput> = req
        .relations
        .iter()
        .map(|r| {
            RelationInput::new(
                r.from_name.clone(),
                r.to_name.clone(),
                r.relation_type.clone(),
            )
            .with_source(r.source.clone())
        })
        .collect();

    let keys: std::collections::HashSet<normalize::RelationKey> = normalize::relations(&inputs)
        .map_err(tool_error)?
        .into_iter()
        .collect();

    repo.create_relations(inputs).await.map_err(tool_error)?;

    // 回读：关系 id 是 `delete_relations` 的唯一凭据，不回读则调用方拿不到句柄。
    // 仓库契约未提供「按三元组反查」，故此处读全图后过滤；记忆规模在个人 / 小团队量级，可接受。
    let graph = repo.read_graph().await.map_err(tool_error)?;
    let relations = graph
        .relations
        .into_iter()
        .filter(|r| {
            keys.contains(&(
                r.from_name.clone(),
                r.to_name.clone(),
                r.relation_type.clone(),
                r.source.clone(),
            ))
        })
        .map(relation_view)
        .collect();
    Ok(RelationsResponse { relations })
}

/// `add_observations`：为已存在实体追加观测，返回受影响实体（含新观测 id）。
pub async fn add_observations(
    repo: &dyn MemoryRepository,
    req: AddObservationsRequest,
) -> Result<EntitiesResponse, ErrorData> {
    let inputs: Vec<ObservationInput> = req
        .observations
        .iter()
        .map(|o| {
            ObservationInput::new(o.entity_name.clone(), o.contents.clone())
                .with_source(o.source.clone())
        })
        .collect();

    let names: Vec<String> = normalize::observations(&inputs)
        .map_err(tool_error)?
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();

    repo.add_observations(inputs).await.map_err(tool_error)?;
    let entities = repo
        .open_nodes(&normalize::name_list(&names))
        .await
        .map_err(tool_error)?;
    Ok(EntitiesResponse {
        entities: sorted_entities(entities),
    })
}

/// `delete_entities`：级联删除实体，返回确实存在并被删除的名字。
pub async fn delete_entities(
    repo: &dyn MemoryRepository,
    req: DeleteEntitiesRequest,
) -> Result<DeleteEntitiesResponse, ErrorData> {
    let names = normalize::name_list(&req.names);

    // 名字是主键，`open_nodes` 开销 O(k)，因此可以真实回读「哪些名字存在过」，
    // 而不是笼统地回显请求参数。空列表直接短路，避免后端拼出 `IN ()`。
    let deleted = if names.is_empty() {
        Vec::new()
    } else {
        repo.open_nodes(&names)
            .await
            .map_err(tool_error)?
            .into_iter()
            .map(|e| e.name)
            .collect()
    };

    repo.delete_entities(&names).await.map_err(tool_error)?;
    Ok(DeleteEntitiesResponse { deleted })
}

/// `delete_observations`：按 id 删除观测（幂等）。
pub async fn delete_observations(
    repo: &dyn MemoryRepository,
    req: DeleteObservationsRequest,
) -> Result<DeleteObservationsResponse, ErrorData> {
    let ids = dedup_ids(&req.ids);
    if !ids.is_empty() {
        repo.delete_observations(&ids).await.map_err(tool_error)?;
    }
    Ok(DeleteObservationsResponse { ids })
}

/// `delete_relations`：按 id 删除关系（幂等）。
pub async fn delete_relations(
    repo: &dyn MemoryRepository,
    req: DeleteRelationsRequest,
) -> Result<DeleteRelationsResponse, ErrorData> {
    let ids = dedup_ids(&req.ids);
    if !ids.is_empty() {
        repo.delete_relations(&ids).await.map_err(tool_error)?;
    }
    Ok(DeleteRelationsResponse { ids })
}

/// `read_graph`：返回全图快照。
pub async fn read_graph(repo: &dyn MemoryRepository) -> Result<GraphResponse, ErrorData> {
    let graph = repo.read_graph().await.map_err(tool_error)?;
    Ok(GraphResponse {
        entities: sorted_entities(graph.entities),
        relations: graph.relations.into_iter().map(relation_view).collect(),
    })
}

/// `search_nodes`：关键词检索，返回命中实体（含观测）。
pub async fn search_nodes(
    repo: &dyn MemoryRepository,
    req: SearchNodesRequest,
) -> Result<EntitiesResponse, ErrorData> {
    let query = req.query.trim();
    if query.is_empty() {
        return Err(ErrorData::invalid_params(
            "query must be non-empty".to_string(),
            None,
        ));
    }

    let hits = repo.search_nodes(query).await.map_err(tool_error)?;
    // 不排序：相关度（bm25）顺序即结果语义，重排会丢掉排序信息。
    let entities = hits.into_iter().map(entity_view).collect();
    Ok(EntitiesResponse { entities })
}

/// `open_nodes`：按名展开实体，未命中的名字跳过。
pub async fn open_nodes(
    repo: &dyn MemoryRepository,
    req: OpenNodesRequest,
) -> Result<EntitiesResponse, ErrorData> {
    let names = normalize::name_list(&req.names);
    if names.is_empty() {
        return Ok(EntitiesResponse {
            entities: Vec::new(),
        });
    }

    let entities = repo.open_nodes(&names).await.map_err(tool_error)?;
    Ok(EntitiesResponse {
        entities: sorted_entities(entities),
    })
}

// ---------------------------------------------------------------------------
// 交接（handoff）工具：语义实现（后端无关）
// ---------------------------------------------------------------------------

/// 交接领域模型 → 出参视图。
pub(crate) fn handoff_view(h: crate::domain::Handoff) -> HandoffView {
    HandoffView {
        task_id: h.task_id,
        revision: h.revision,
        goal: h.goal,
        state: h.state,
        next_steps: h.next_steps,
        files: h.files,
        decisions: h.decisions,
        open_questions: h.open_questions,
        client: h.client,
        session_id: h.session_id,
        created_at: h.created_at,
        acked_at: h.acked_at,
        acked_by: h.acked_by,
    }
}

/// 摘要 → 出参视图。
pub(crate) fn handoff_summary_view(s: crate::domain::HandoffSummary) -> HandoffSummaryView {
    HandoffSummaryView {
        task_id: s.task_id,
        revision: s.revision,
        goal: s.goal,
        state_excerpt: s.state_excerpt,
        client: s.client,
        session_id: s.session_id,
        created_at: s.created_at,
        acked: s.acked,
        acked_by: s.acked_by,
        next_step_count: s.next_step_count,
    }
}

/// 寻址结果 → `(状态, 完整记录?, 候选摘要)`。
///
/// @intent 三种结果都映射成 `status` 而不是错误：`none` 是「第一次接手时还没有交接」
///         这一正常状态，`ambiguous` 是「服务端不替调用方决定接哪条」（docs §5.1）。
pub(crate) fn resolution_parts(
    resolution: HandoffResolution,
) -> (HandoffStatus, Option<HandoffView>, Vec<HandoffSummaryView>) {
    match resolution {
        HandoffResolution::Found(h) => (HandoffStatus::Found, Some(handoff_view(*h)), Vec::new()),
        HandoffResolution::None => (HandoffStatus::None, None, Vec::new()),
        HandoffResolution::Ambiguous(list) => (
            HandoffStatus::Ambiguous,
            None,
            list.into_iter().map(handoff_summary_view).collect(),
        ),
    }
}

/// `handoff_save`：追加一条不可变修订。
///
/// @intent 缺 `task_id` 时落 `current@<client>` 分片槽位——**不同客户端的待办互不遮蔽**
///         （docs §3.7）。出参回传最终 `task_id`，代理原样带回即「继续同一工作项」。
pub async fn handoff_save(
    repo: &dyn HandoffRepository,
    req: HandoffSaveRequest,
) -> Result<HandoffSaveResponse, ErrorData> {
    let input = HandoffInput {
        task_id: req.task_id,
        goal: req.goal,
        state: req.state,
        next_steps: req.next_steps,
        files: req.files,
        decisions: req.decisions,
        open_questions: req.open_questions,
        client: req.client,
        session_id: req.session_id,
    };

    let saved = repo.save_handoff(input).await.map_err(tool_error)?;
    Ok(HandoffSaveResponse {
        task_id: saved.task_id.clone(),
        revision: saved.revision,
        handoff: handoff_view(saved),
    })
}

/// `handoff_load`：不给 `task_id` = 取当前待接手的那条；给了则精确取。
pub async fn handoff_load(
    repo: &dyn HandoffRepository,
    req: HandoffLoadRequest,
) -> Result<HandoffLoadResponse, ErrorData> {
    let resolution = repo
        .resolve_handoff(req.task_id.as_deref())
        .await
        .map_err(tool_error)?;
    let (status, handoff, candidates) = resolution_parts(resolution);
    Ok(HandoffLoadResponse {
        status,
        handoff,
        candidates,
    })
}

/// `handoff_list`：列出「有哪些活等着我」，接手选择界面的数据来源。
pub async fn handoff_list(
    repo: &dyn HandoffRepository,
    req: HandoffListRequest,
) -> Result<HandoffListResponse, ErrorData> {
    let listed = repo
        .list_handoffs(req.prefix.as_deref(), req.unacked_only, req.limit)
        .await
        .map_err(tool_error)?;
    Ok(HandoffListResponse {
        handoffs: listed.into_iter().map(handoff_summary_view).collect(),
    })
}

/// `handoff_ack`：标记接手（CAS）。
///
/// @intent `won=false` 是**正常竞态结果**而不是错误：后到者据此看到「谁先接的」，
///         进而决定停下来问人，而不是两个客户端同时改同一份代码（docs §4.4）。
pub async fn handoff_ack(
    repo: &dyn HandoffRepository,
    req: HandoffAckRequest,
) -> Result<HandoffAckResponse, ErrorData> {
    let outcome = repo
        .ack_handoff(req.task_id.as_deref(), &req.client)
        .await
        .map_err(tool_error)?;
    let (status, handoff, candidates) = resolution_parts(outcome.resolution);
    Ok(HandoffAckResponse {
        status,
        won: outcome.won,
        handoff,
        candidates,
    })
}

// ---------------------------------------------------------------------------
// 会话工具（P5.1，docs/memory-system-design.md §5.4）
// ---------------------------------------------------------------------------

/// `log_turn`：追加一轮对话，`turn_index` 由后端分配并回读。
///
/// @intent 细粒度 transcript 与 handoff（结论级摘要）互补：要把「完整对话原样带回」
///         时用会话工具。回读 `turn_index` 是硬约定——调用方不数轮次。
pub async fn log_turn(
    repo: &dyn ConversationRepository,
    req: LogTurnRequest,
) -> Result<TurnView, ErrorData> {
    let turn = repo
        .append_turn(SessionInput {
            session_id: req.session_id,
            role: req.role,
            content: req.content,
        })
        .await
        .map_err(tool_error)?;
    Ok(TurnView {
        session_id: turn.session_id,
        turn_index: turn.turn_index,
        role: turn.role,
        content: turn.content,
        created_at: turn.created_at,
    })
}

/// `get_session`：拉回完整 transcript（轮次升序）。
///
/// @intent 缺失会话返回空数组而非错误——「还没聊过」是正常状态，
///         调用方无需为此分支重试或报错。
pub async fn get_session(
    repo: &dyn ConversationRepository,
    req: GetSessionRequest,
) -> Result<GetSessionResponse, ErrorData> {
    let turns = repo
        .get_session(&req.session_id)
        .await
        .map_err(tool_error)?;
    Ok(GetSessionResponse {
        turns: turns
            .into_iter()
            .map(|t| TurnView {
                session_id: t.session_id,
                turn_index: t.turn_index,
                role: t.role,
                content: t.content,
                created_at: t.created_at,
            })
            .collect(),
    })
}

/// `list_sessions`：列出会话标识（最近活跃降序），恢复方「先接最近的」。
pub async fn list_sessions(
    repo: &dyn ConversationRepository,
    req: ListSessionsRequest,
) -> Result<ListSessionsResponse, ErrorData> {
    let sessions = repo
        .list_sessions(req.prefix.as_deref())
        .await
        .map_err(tool_error)?;
    Ok(ListSessionsResponse { sessions })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{InMemBackend, StorageBackend};
    use std::sync::Arc;

    /// JSON-RPC 参数错误码（MCP 规范）。
    const INVALID_PARAMS: i32 = -32602;
    /// JSON-RPC 服务端内部错误码。
    const INTERNAL_ERROR: i32 = -32603;

    /// 每个用例独立的内存仓库，避免相互污染。
    async fn repo() -> Arc<dyn MemoryRepository> {
        InMemBackend::new()
            .repositories_for("t")
            .await
            .unwrap()
            .memory
    }

    /// 每个用例独立的交接仓库（同一后端，但走 `handoff` 字段）。
    async fn handoff_repo() -> Arc<dyn HandoffRepository> {
        InMemBackend::new()
            .repositories_for("t")
            .await
            .unwrap()
            .handoff
    }

    /// 便捷构造 save 入参（测试里只关心差异字段）。
    fn save_req(goal: &str, state: &str) -> HandoffSaveRequest {
        HandoffSaveRequest {
            goal: goal.to_string(),
            state: state.to_string(),
            next_steps: Vec::new(),
            files: Vec::new(),
            decisions: Vec::new(),
            open_questions: Vec::new(),
            task_id: None,
            client: String::new(),
            session_id: String::new(),
        }
    }

    fn entity(name: &str, ty: &str) -> EntityParam {
        EntityParam {
            name: name.to_string(),
            entity_type: ty.to_string(),
            observations: Vec::new(),
            source: String::new(),
        }
    }

    /// 带观测的实体入参（校验 `create_entities` 的「一次建实体并附观测」路径）。
    fn entity_with_observations(name: &str, ty: &str, contents: &[&str]) -> EntityParam {
        EntityParam {
            observations: contents.iter().map(|c| c.to_string()).collect(),
            ..entity(name, ty)
        }
    }

    fn relation(from: &str, to: &str, ty: &str) -> RelationParam {
        RelationParam {
            from_name: from.to_string(),
            to_name: to.to_string(),
            relation_type: ty.to_string(),
            source: String::new(),
        }
    }

    fn observation(name: &str, contents: &[&str]) -> ObservationParam {
        ObservationParam {
            entity_name: name.to_string(),
            contents: contents.iter().map(|c| c.to_string()).collect(),
            source: String::new(),
        }
    }

    /// 断言错误码，兼顾「不是预期的入参错误」这一失败信息。
    fn assert_code(err: ErrorData, expected: i32) {
        assert_eq!(err.code.0, expected, "错误信息：{}", err.message);
    }

    #[tokio::test]
    async fn create_entities_returns_persisted_views() {
        let repo = repo().await;
        let resp = create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("alpha", "person"), entity("beta", "")],
            },
        )
        .await
        .unwrap();

        let names: Vec<&str> = resp.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(resp.entities[0].entity_type, "person");
        assert_eq!(
            resp.entities[1].entity_type, "unknown",
            "留空类型应规范化落为 unknown"
        );
        assert!(resp.entities[0].observations.is_empty());
    }

    /// 同名重复创建不覆盖既有类型，且返回的是「当前状态」而非「本次新建」。
    #[tokio::test]
    async fn create_entities_is_idempotent_and_keeps_original_type() {
        let repo = repo().await;
        create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("alpha", "person")],
            },
        )
        .await
        .unwrap();

        let again = create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("alpha", "concept")],
            },
        )
        .await
        .unwrap();

        assert_eq!(again.entities.len(), 1);
        assert_eq!(again.entities[0].entity_type, "person");
    }

    /// A：官方 schema 允许「一次调用建实体并附带观测」。
    ///
    /// @intent 原先 `EntityParam` 没有 `observations` 字段，而 serde 又默认忽略未知字段，
    ///         于是官方习惯的调用会返回 200 且观测**一条都不落库**，调用方毫无察觉。
    ///         本用例钉住「请求里带的观测必须真的写进去」，并回读复核而不只看响应。
    #[tokio::test]
    async fn create_entities_persists_the_observations_carried_in_the_request() {
        let repo = repo().await;
        let resp = create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![
                    entity_with_observations("alpha", "person", &["第一条事实", "第二条事实"]),
                    entity("beta", ""),
                ],
            },
        )
        .await
        .unwrap();

        let contents: Vec<&str> = resp.entities[0]
            .observations
            .iter()
            .map(|o| o.content.as_str())
            .collect();
        assert_eq!(contents, vec!["第一条事实", "第二条事实"]);
        assert!(
            resp.entities[1].observations.is_empty(),
            "未带观测的实体不应凭空多出观测"
        );

        let graph = read_graph(&*repo).await.unwrap();
        let alpha = graph
            .entities
            .iter()
            .find(|e| e.name == "alpha")
            .expect("alpha 应已存在");
        assert_eq!(alpha.observations.len(), 2, "观测应已真正落库");
    }

    /// 官方 camelCase 写法（`entityType`）与 `observations` 必须能同时使用。
    #[tokio::test]
    async fn create_entities_accepts_the_official_camel_case_shape_with_observations() {
        let repo = repo().await;
        let req: CreateEntitiesRequest = serde_json::from_value(serde_json::json!({
            "entities": [
                {"name": "alpha", "entityType": "person", "observations": ["官方写法的事实"]}
            ]
        }))
        .expect("官方 schema 的写法必须被接受");

        let resp = create_entities(&*repo, req).await.unwrap();
        assert_eq!(resp.entities[0].entity_type, "person");
        assert_eq!(resp.entities[0].observations.len(), 1);
        assert_eq!(resp.entities[0].observations[0].content, "官方写法的事实");
    }

    /// 观测非法时**整批拒绝**，且不得留下「实体已建、观测没写」的半成品。
    #[tokio::test]
    async fn create_entities_rejects_the_whole_batch_when_an_observation_is_blank() {
        let repo = repo().await;
        let err = create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity_with_observations("alpha", "person", &["ok", "   "])],
            },
        )
        .await
        .unwrap_err();
        assert_code(err, INVALID_PARAMS);

        let graph = read_graph(&*repo).await.unwrap();
        assert!(
            graph.entities.is_empty(),
            "校验必须在写入前完成：非法观测不得在库里留下任何实体"
        );
    }

    /// 观测继承实体的 `source`：同一次调用视为同一来源，调用方无需写两遍。
    #[tokio::test]
    async fn observations_carried_by_create_entities_inherit_the_entity_source() {
        let repo = repo().await;
        let mut e = entity_with_observations("alpha", "person", &["fact"]);
        e.source = "workbuddy".to_string();

        let resp = create_entities(&*repo, CreateEntitiesRequest { entities: vec![e] })
            .await
            .unwrap();
        assert_eq!(resp.entities[0].source, "workbuddy");
        assert_eq!(resp.entities[0].observations[0].source, "workbuddy");
    }

    #[tokio::test]
    async fn create_relations_returns_relations_with_ids() {
        let repo = repo().await;
        create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("a", "person"), entity("b", "person")],
            },
        )
        .await
        .unwrap();

        let resp = create_relations(
            &*repo,
            CreateRelationsRequest {
                relations: vec![relation("a", "b", "knows"), relation("a", "b", "knows")],
            },
        )
        .await
        .unwrap();

        assert_eq!(resp.relations.len(), 1, "三元组重复应被跳过");
        let r = &resp.relations[0];
        assert_eq!((r.from_name.as_str(), r.to_name.as_str()), ("a", "b"));
        assert_eq!(r.relation_type, "knows");
        assert!(
            r.id > 0,
            "回读的关系必须带 id，否则无法调用 delete_relations"
        );
    }

    #[tokio::test]
    async fn add_observations_returns_entity_with_observation_ids() {
        let repo = repo().await;
        create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("alpha", "person")],
            },
        )
        .await
        .unwrap();

        let resp = add_observations(
            &*repo,
            AddObservationsRequest {
                observations: vec![observation("alpha", &["爱吃面", "常驻上海"])],
            },
        )
        .await
        .unwrap();

        assert_eq!(resp.entities.len(), 1);
        let obs = &resp.entities[0].observations;
        let contents: Vec<&str> = obs.iter().map(|o| o.content.as_str()).collect();
        assert_eq!(contents, vec!["爱吃面", "常驻上海"]);
        assert!(
            obs.iter().all(|o| o.id > 0),
            "必须回读观测 id，否则无法调用 delete_observations"
        );
    }

    #[tokio::test]
    async fn add_observations_to_missing_entity_is_invalid_params() {
        let repo = repo().await;
        let err = add_observations(
            &*repo,
            AddObservationsRequest {
                observations: vec![observation("ghost", &["x"])],
            },
        )
        .await
        .unwrap_err();
        assert_code(err, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn create_entities_rejects_empty_name() {
        let repo = repo().await;
        let err = create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("   ", "person")],
            },
        )
        .await
        .unwrap_err();
        assert_code(err, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn delete_entities_reports_only_existing_names() {
        let repo = repo().await;
        create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("alpha", "person")],
            },
        )
        .await
        .unwrap();

        let resp = delete_entities(
            &*repo,
            DeleteEntitiesRequest {
                names: vec!["alpha".to_string(), "ghost".to_string()],
            },
        )
        .await
        .unwrap();

        assert_eq!(resp.deleted, vec!["alpha".to_string()]);
        let graph = read_graph(&*repo).await.unwrap();
        assert!(graph.entities.is_empty());
    }

    #[tokio::test]
    async fn delete_observations_echoes_ids_and_is_idempotent() {
        let repo = repo().await;
        create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("alpha", "person")],
            },
        )
        .await
        .unwrap();
        let added = add_observations(
            &*repo,
            AddObservationsRequest {
                observations: vec![observation("alpha", &["一", "二"])],
            },
        )
        .await
        .unwrap();
        let ids: Vec<i64> = added.entities[0]
            .observations
            .iter()
            .map(|o| o.id)
            .collect();

        let req = DeleteObservationsRequest {
            ids: vec![ids[0], ids[0], 999_999],
        };
        let resp = delete_observations(&*repo, req.clone()).await.unwrap();
        assert_eq!(
            resp.ids,
            vec![ids[0], 999_999],
            "回显需去重并保持原序；不存在的 id 不报错"
        );

        let after = open_nodes(
            &*repo,
            OpenNodesRequest {
                names: vec!["alpha".to_string()],
            },
        )
        .await
        .unwrap();
        assert_eq!(after.entities[0].observations.len(), 1);
        assert_eq!(after.entities[0].observations[0].id, ids[1]);

        // 二次删除同一批 id 仍应成功（幂等）
        delete_observations(&*repo, req).await.unwrap();
    }

    #[tokio::test]
    async fn delete_relations_echoes_ids_and_is_idempotent() {
        let repo = repo().await;
        create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("a", "person"), entity("b", "person")],
            },
        )
        .await
        .unwrap();
        let created = create_relations(
            &*repo,
            CreateRelationsRequest {
                relations: vec![relation("a", "b", "knows")],
            },
        )
        .await
        .unwrap();
        let id = created.relations[0].id;

        let req = DeleteRelationsRequest { ids: vec![id, id] };
        let resp = delete_relations(&*repo, req.clone()).await.unwrap();
        assert_eq!(resp.ids, vec![id]);
        assert!(read_graph(&*repo).await.unwrap().relations.is_empty());

        delete_relations(&*repo, req).await.unwrap();
    }

    #[tokio::test]
    async fn read_graph_returns_entities_and_relations() {
        let repo = repo().await;
        create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("a", "person"), entity("b", "person")],
            },
        )
        .await
        .unwrap();
        create_relations(
            &*repo,
            CreateRelationsRequest {
                relations: vec![relation("a", "b", "knows")],
            },
        )
        .await
        .unwrap();
        add_observations(
            &*repo,
            AddObservationsRequest {
                observations: vec![observation("a", &["备注"])],
            },
        )
        .await
        .unwrap();

        let graph = read_graph(&*repo).await.unwrap();
        assert_eq!(graph.entities.len(), 2);
        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.entities[0].observations.len(), 1);
    }

    /// 检索需覆盖名称、类型与观测内容三条路径。
    #[tokio::test]
    async fn search_nodes_matches_name_type_and_content() {
        let repo = repo().await;
        create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("深色主题", "preference"), entity("其他", "note")],
            },
        )
        .await
        .unwrap();
        add_observations(
            &*repo,
            AddObservationsRequest {
                observations: vec![
                    // 故意的用词隔离：观测里不含「深色主题」，使名称命中可单独断言
                    observation("其他", &["用户偏好使用深色界面"]),
                    observation("深色主题", &["来自 2026-09 的偏好确认"]),
                ],
            },
        )
        .await
        .unwrap();

        // 1) 按名称命中
        let by_name = search_nodes(
            &*repo,
            SearchNodesRequest {
                query: "深色主题".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(by_name.entities.len(), 1);
        assert_eq!(by_name.entities[0].name, "深色主题");
        assert_eq!(
            by_name.entities[0]
                .observations
                .iter()
                .map(|o| o.content.as_str())
                .collect::<Vec<_>>(),
            vec!["来自 2026-09 的偏好确认"],
            "命中实体应带出其完整观测集，而非仅匹配的那条"
        );

        // 2) 按观测内容命中（中文片段走子串回退）
        let by_content = search_nodes(
            &*repo,
            SearchNodesRequest {
                query: "深色界面".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(by_content.entities.len(), 1);
        assert_eq!(by_content.entities[0].name, "其他");

        // 3) 共同片段应同时命中两侧，说明检索覆盖名称与内容两条路径
        let by_common = search_nodes(
            &*repo,
            SearchNodesRequest {
                query: "深色".to_string(),
            },
        )
        .await
        .unwrap();
        let mut names: Vec<&str> = by_common.entities.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["其他", "深色主题"]);

        // 4) 按实体类型命中
        let by_type = search_nodes(
            &*repo,
            SearchNodesRequest {
                query: "preference".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(by_type.entities.len(), 1);
        assert_eq!(by_type.entities[0].name, "深色主题");
    }

    #[tokio::test]
    async fn search_nodes_rejects_blank_query() {
        let repo = repo().await;
        let err = search_nodes(
            &*repo,
            SearchNodesRequest {
                query: "   ".to_string(),
            },
        )
        .await
        .unwrap_err();
        assert_code(err.clone(), INVALID_PARAMS);
        // U4：错误信息必须点出字段名 `query`，否则调用方（CLI / 客户端）拿到
        // 的是「non-empty」这种无主语的提示，不知道该改哪个入参。
        assert!(
            err.message.to_lowercase().contains("query"),
            "错误信息应点出字段名 query，实际：{}",
            err.message
        );
    }

    #[tokio::test]
    async fn open_nodes_skips_unknown_names() {
        let repo = repo().await;
        create_entities(
            &*repo,
            CreateEntitiesRequest {
                entities: vec![entity("alpha", "person")],
            },
        )
        .await
        .unwrap();

        let resp = open_nodes(
            &*repo,
            OpenNodesRequest {
                names: vec!["ghost".to_string(), "alpha".to_string()],
            },
        )
        .await
        .unwrap();

        assert_eq!(resp.entities.len(), 1);
        assert_eq!(resp.entities[0].name, "alpha");
    }

    /// 出参 JSON 形态是代理可见契约，锁死字段名以防无意改名。
    #[test]
    fn responses_serialize_to_stable_json_shape() {
        let resp = EntitiesResponse {
            entities: vec![EntityView {
                name: "alpha".to_string(),
                entity_type: "person".to_string(),
                created_at: 1,
                source: "workbuddy".to_string(),
                observations: vec![ObservationView {
                    id: 7,
                    content: "备注".to_string(),
                    created_at: 2,
                    source: "claude-code".to_string(),
                }],
            }],
        };
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["entities"][0]["name"], "alpha");
        assert_eq!(value["entities"][0]["entity_type"], "person");
        assert_eq!(value["entities"][0]["source"], "workbuddy");
        assert_eq!(value["entities"][0]["observations"][0]["id"], 7);
        assert_eq!(value["entities"][0]["observations"][0]["content"], "备注");
        assert_eq!(
            value["entities"][0]["observations"][0]["source"],
            "claude-code"
        );

        let graph = GraphResponse {
            entities: vec![],
            relations: vec![RelationView {
                id: 3,
                from_name: "a".to_string(),
                to_name: "b".to_string(),
                relation_type: "knows".to_string(),
                created_at: 4,
                source: "cursor".to_string(),
            }],
        };
        let value = serde_json::to_value(&graph).unwrap();
        assert_eq!(value["relations"][0]["from_name"], "a");
        assert_eq!(value["relations"][0]["to_name"], "b");
        assert_eq!(value["relations"][0]["relation_type"], "knows");
        assert_eq!(value["relations"][0]["source"], "cursor");
    }

    /// 写侧携带的 `source` 必须穿透工具层落库，并随读路径完整返回。
    ///
    /// @intent 「事后按客户端审计」是 P5 前的过渡能力：入参可选、出参必带；
    ///         未携带来源的写入应落为空串而非编造值。
    #[tokio::test]
    async fn source_flows_through_write_tools_and_round_trips() {
        let repo = repo().await;

        let mut e = entity("src-e", "note");
        e.source = "workbuddy".to_string();
        create_entities(&*repo, CreateEntitiesRequest { entities: vec![e] })
            .await
            .unwrap();

        let mut o = observation("src-e", &["来自 claude-code"]);
        o.source = "claude-code".to_string();
        add_observations(
            &*repo,
            AddObservationsRequest {
                observations: vec![o, observation("src-e", &["未声明来源"])],
            },
        )
        .await
        .unwrap();

        let mut r = relation("src-e", "src-e", "self_ref");
        r.source = "cursor".to_string();
        create_relations(&*repo, CreateRelationsRequest { relations: vec![r] })
            .await
            .unwrap();

        let graph = read_graph(&*repo).await.unwrap();
        let e = &graph.entities[0];
        assert_eq!(e.source, "workbuddy");
        let obs_with = e
            .observations
            .iter()
            .find(|o| o.content == "来自 claude-code")
            .unwrap();
        let obs_without = e
            .observations
            .iter()
            .find(|o| o.content == "未声明来源")
            .unwrap();
        assert_eq!(obs_with.source, "claude-code");
        assert_eq!(obs_without.source, "", "未携带来源应落为空串");
        assert_eq!(graph.relations[0].source, "cursor");
    }

    /// 入参省略 `source` 时必须可反序列化为空串（对旧客户端零改造）。
    #[test]
    fn omitted_source_deserializes_to_empty_string() {
        let e: CreateEntitiesRequest = serde_json::from_value(serde_json::json!({
            "entities": [{ "name": "alpha", "entity_type": "person" }]
        }))
        .unwrap();
        assert_eq!(e.entities[0].source, "");

        let r: CreateRelationsRequest = serde_json::from_value(serde_json::json!({
            "relations": [{ "from_name": "a", "to_name": "b", "relation_type": "knows" }]
        }))
        .unwrap();
        assert_eq!(r.relations[0].source, "");

        let o: AddObservationsRequest = serde_json::from_value(serde_json::json!({
            "observations": [{ "entity_name": "alpha", "contents": ["x"] }]
        }))
        .unwrap();
        assert_eq!(o.observations[0].source, "");
    }

    /// 官方 camelCase 写法需被容忍，避免照抄旧 schema 的模型调用失败。
    #[test]
    fn camel_case_aliases_are_accepted() {
        let req: CreateEntitiesRequest = serde_json::from_value(serde_json::json!({
            "entities": [{ "name": "alpha", "entityType": "person" }]
        }))
        .unwrap();
        assert_eq!(req.entities[0].entity_type, "person");

        let rel: CreateRelationsRequest = serde_json::from_value(serde_json::json!({
            "relations": [{ "from": "a", "to": "b", "relationType": "knows" }]
        }))
        .unwrap();
        assert_eq!(rel.relations[0].from_name, "a");
        assert_eq!(rel.relations[0].to_name, "b");

        let obs: AddObservationsRequest = serde_json::from_value(serde_json::json!({
            "observations": [{ "entityName": "alpha", "contents": ["x"] }]
        }))
        .unwrap();
        assert_eq!(obs.observations[0].entity_name, "alpha");

        // entity_type 省略时默认为空串，交由存储层规范化为 unknown
        let req: CreateEntitiesRequest = serde_json::from_value(serde_json::json!({
            "entities": [{ "name": "alpha" }]
        }))
        .unwrap();
        assert_eq!(req.entities[0].entity_type, "");
    }

    /// 错误映射：调用方可修正的问题走 -32602，服务端故障走 -32603。
    #[test]
    fn storage_errors_map_to_the_right_jsonrpc_codes() {
        assert_eq!(
            tool_error(StorageError::InvalidInput("x".to_string()))
                .code
                .0,
            INVALID_PARAMS
        );
        assert_eq!(
            tool_error(StorageError::EntityNotFound("alpha".to_string()))
                .code
                .0,
            INVALID_PARAMS
        );
        for server_side in [
            StorageError::InvalidProjectId("../x".to_string()),
            StorageError::Unsupported("postgres".to_string()),
            StorageError::Backend("db down".to_string()),
        ] {
            assert_eq!(
                tool_error(server_side.clone()).code.0,
                INTERNAL_ERROR,
            "{server_side:?} 属服务端故障，不应误导调用方去改参数"
        );
    }
}

    /// S1（审计）：后端故障对客户端只暴露通用文案，不得泄露 SQLite 表名 / SQL / 路径等内部细节。
    ///
    /// @intent `StorageError::Backend` 携带的是底层数据库原始报错，原样回传给 MCP 客户端
    ///         等于把内部拓扑免费送去做侦察。正确做法是返回固定通用文案，原始错误仅入服务端日志。
    ///         本用例先以「当前实现仍原样回传」为前提断言其失败，再驱动修复（TDD red 阶段）。
    #[test]
    fn backend_error_does_not_leak_internals_to_client() {
        let raw =
            "table entities has no column named foo (SQL: SELECT foo FROM entities WHERE id=1)";
        let err = tool_error(StorageError::Backend(raw.to_string()));
        assert_eq!(err.code.0, INTERNAL_ERROR, "服务端故障必须走 -32603");
        assert_eq!(
            err.message, "internal server error",
            "客户端不应看到任何内部细节"
        );
        assert!(
            !err.message.contains("entities"),
            "表名不得泄露到客户端"
        );
        assert!(
            !err.message.contains("SELECT"),
            "SQL 片段不得泄露到客户端"
        );
    }

    /// 官方语义的 9 个记忆工具一个都不许改名（客户端零改造的承诺）。
    #[test]
    fn memory_tool_names_are_stable_and_untouched() {
        assert_eq!(MEMORY_TOOL_NAMES.len(), 9);
        let unique: std::collections::HashSet<&&str> = MEMORY_TOOL_NAMES.iter().collect();
        assert_eq!(unique.len(), 9, "工具名不得重复");
        assert!(MEMORY_TOOL_NAMES.contains(&"read_graph"));
    }

    /// 三个家族无重名、合起来 16 个，且 `TOOL_NAMES` 确实是三者的并集。
    ///
    /// @intent 这条钉住「对外承诺的工具面」本身：`assert_tool_manifest` 只比对清单与宏
    ///         实注册数，若清单自己漏了一族，自检会「一致地错」。
    #[test]
    fn tool_families_are_disjoint_and_cover_the_manifest() {
        let all: std::collections::HashSet<&&str> = TOOL_NAMES.iter().collect();
        assert_eq!(TOOL_NAMES.len(), 16);
        assert_eq!(all.len(), 16, "对外清单不得有重名");

        for name in MEMORY_TOOL_NAMES {
            assert!(all.contains(&name), "记忆工具 {name} 不在对外清单里");
        }
        for name in HANDOFF_TOOL_NAMES {
            assert!(all.contains(&name), "交接工具 {name} 不在对外清单里");
            assert!(
                name.starts_with("handoff_"),
                "交接工具应统一用 family 前缀：{name}"
            );
        }
        assert_eq!(HANDOFF_TOOL_NAMES.len(), 4);
        for name in SESSION_TOOL_NAMES {
            assert!(all.contains(&name), "会话工具 {name} 不在对外清单里");
        }
        assert_eq!(SESSION_TOOL_NAMES.len(), 3);
    }

    // -----------------------------------------------------------------------
    // 交接工具的语义（脱离 MCP 传输层，直接打内存后端）
    // -----------------------------------------------------------------------

    /// 空仓库 load：`status="none"` 是正常状态，不是错误。
    #[tokio::test]
    async fn handoff_load_on_empty_repo_reports_none() {
        let repo = handoff_repo().await;
        let res = handoff_load(&*repo, HandoffLoadRequest::default())
            .await
            .unwrap();
        assert_eq!(res.status, HandoffStatus::None);
        assert!(res.handoff.is_none());
        assert!(res.candidates.is_empty());
    }

    /// save 回传最终 task_id（分片槽位）——代理据此继续同一工作项，人无需参与。
    #[tokio::test]
    async fn handoff_save_returns_the_sharded_task_id() {
        let repo = handoff_repo().await;
        let mut req = save_req("把交接做出来", "设计完成");
        req.client = "zed".to_string();
        req.session_id = "A1".to_string();
        req.next_steps = vec!["写 repo".to_string()];

        let res = handoff_save(&*repo, req).await.unwrap();
        assert_eq!(res.task_id, "current@zed");
        assert_eq!(res.revision, 1);
        assert_eq!(res.handoff.session_id, "A1");
        assert_eq!(res.handoff.next_steps, vec!["写 repo".to_string()]);
        // 出参的 task_id 与 handoff.task_id 必须一致，否则代理会带错 ID 回去
        assert_eq!(res.handoff.task_id, res.task_id);
    }

    /// 零参数 load 命中唯一一条：`status="found"` 且带上完整正文。
    #[tokio::test]
    async fn handoff_load_without_id_finds_the_only_pending() {
        let repo = handoff_repo().await;
        handoff_save(&*repo, save_req("唯一的活", "进行中"))
            .await
            .unwrap();

        let res = handoff_load(&*repo, HandoffLoadRequest::default())
            .await
            .unwrap();
        assert_eq!(res.status, HandoffStatus::Found);
        let h = res.handoff.expect("found 必须带回完整记录");
        assert_eq!(h.goal, "唯一的活");
        assert!(h.acked_at.is_none());
        assert!(res.candidates.is_empty());
    }

    /// 多条待接手：返回候选摘要而不是错误，让调用方挑选后再精确取。
    #[tokio::test]
    async fn handoff_load_returns_candidates_when_ambiguous() {
        let repo = handoff_repo().await;
        let mut a = save_req("alpha", "起步");
        a.task_id = Some("alpha".to_string());
        let mut b = save_req("beta", "起步");
        b.task_id = Some("beta".to_string());
        handoff_save(&*repo, a).await.unwrap();
        handoff_save(&*repo, b).await.unwrap();

        let res = handoff_load(&*repo, HandoffLoadRequest::default())
            .await
            .unwrap();
        assert_eq!(res.status, HandoffStatus::Ambiguous);
        assert!(res.handoff.is_none());
        let mut ids: Vec<String> = res.candidates.iter().map(|c| c.task_id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["alpha".to_string(), "beta".to_string()]);
    }

    /// list 缺省只看待接手，且缺省 `limit` 与 REST 面一致（10）。
    #[tokio::test]
    async fn handoff_list_defaults_to_pending_only() {
        let repo = handoff_repo().await;
        let mut ack = save_req("要做掉的活", "进行中");
        ack.task_id = Some("done-one".to_string());
        handoff_save(&*repo, ack).await.unwrap();
        handoff_ack(
            &*repo,
            HandoffAckRequest {
                task_id: Some("done-one".to_string()),
                client: "zed".to_string(),
            },
        )
        .await
        .unwrap();

        let pending = handoff_list(
            &*repo,
            HandoffListRequest {
                unacked_only: true,
                limit: DEFAULT_HANDOFF_LIST_LIMIT,
                prefix: None,
            },
        )
        .await
        .unwrap();
        assert!(pending.handoffs.is_empty(), "已接手的不得出现在缺省列表里");

        let all = handoff_list(
            &*repo,
            HandoffListRequest {
                unacked_only: false,
                limit: DEFAULT_HANDOFF_LIST_LIMIT,
                prefix: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(all.handoffs.len(), 1);
        assert!(all.handoffs[0].acked);
        assert_eq!(all.handoffs[0].acked_by, "zed");
    }

    /// ack 的两种结局都不是错误：首次 `won=true`，重复 `won=false` 且不覆盖接手方。
    #[tokio::test]
    async fn handoff_ack_reports_win_then_loses_idempotently() {
        let repo = handoff_repo().await;
        let mut req = save_req("抢手的事", "待接");
        req.task_id = Some("race".to_string());
        handoff_save(&*repo, req).await.unwrap();

        let first = handoff_ack(
            &*repo,
            HandoffAckRequest {
                task_id: Some("race".to_string()),
                client: "zed".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(first.status, HandoffStatus::Found);
        assert!(first.won);
        assert_eq!(first.handoff.unwrap().acked_by, "zed");

        let second = handoff_ack(
            &*repo,
            HandoffAckRequest {
                task_id: Some("race".to_string()),
                client: "cursor".to_string(),
            },
        )
        .await
        .unwrap();
        assert!(!second.won, "后到者不得再次抢到");
        assert_eq!(
            second.handoff.unwrap().acked_by,
            "zed",
            "接手方必须保持首次值"
        );
    }

    /// 非法入参 → `-32602`（调用方改参数即可），而不是服务端故障。
    #[tokio::test]
    async fn handoff_save_maps_invalid_input_to_invalid_params() {
        let repo = handoff_repo().await;
        let err = handoff_save(&*repo, save_req("", "空目标"))
            .await
            .unwrap_err();
        assert_eq!(err.code.0, INVALID_PARAMS);

        let mut bad_id = save_req("目标", "有进展");
        bad_id.task_id = Some("bad/id".to_string());
        let err = handoff_save(&*repo, bad_id).await.unwrap_err();
        assert_eq!(err.code.0, INVALID_PARAMS);
    }

    // -----------------------------------------------------------------------
    // 会话工具（P5.1）
    // -----------------------------------------------------------------------

    /// 每个用例独立的会话仓库（同一后端，但走 `conversation` 字段）。
    async fn conversation_repo() -> Arc<dyn ConversationRepository> {
        InMemBackend::new()
            .repositories_for("t")
            .await
            .unwrap()
            .conversation
    }

    /// `log_turn` 回读必须带服务端分配的轮次序号；缺失会话拉回空 transcript。
    #[tokio::test]
    async fn log_turn_assigns_and_returns_turn_index() {
        let repo = conversation_repo().await;
        let t0 = log_turn(
            &*repo,
            LogTurnRequest {
                session_id: "s-1".to_string(),
                role: "user".to_string(),
                content: "第一轮".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(t0.turn_index, 0, "首写必须分配 0");
        let t1 = log_turn(
            &*repo,
            LogTurnRequest {
                session_id: "s-1".to_string(),
                role: "assistant".to_string(),
                content: "第二轮".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(t1.turn_index, 1, "次写必须分配 1");

        let got = get_session(
            &*repo,
            GetSessionRequest {
                session_id: "s-1".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(got.turns.len(), 2);
        assert_eq!(got.turns[0].content, "第一轮");
        assert_eq!(got.turns[1].role, "assistant");

        let missing = get_session(
            &*repo,
            GetSessionRequest {
                session_id: "s-nope".to_string(),
            },
        )
        .await
        .unwrap();
        assert!(
            missing.turns.is_empty(),
            "缺失会话 = 空 transcript，不是错误"
        );
    }

    /// 非法入参（角色枚举外 / 空正文 / 非法 session_id）→ `-32602`。
    #[tokio::test]
    async fn log_turn_maps_invalid_input_to_invalid_params() {
        let repo = conversation_repo().await;
        for req in [
            LogTurnRequest {
                session_id: "s".to_string(),
                role: "Tool".to_string(),
                content: "x".to_string(),
            },
            LogTurnRequest {
                session_id: "s".to_string(),
                role: "user".to_string(),
                content: "  ".to_string(),
            },
            LogTurnRequest {
                session_id: "bad/id".to_string(),
                role: "user".to_string(),
                content: "x".to_string(),
            },
        ] {
            let err = log_turn(&*repo, req).await.unwrap_err();
            assert_eq!(err.code.0, INVALID_PARAMS);
        }
    }

    /// list_sessions 返回最近活跃降序；get_session + list_sessions 组成恢复路径。
    #[tokio::test]
    async fn session_round_trip_via_list_and_get() {
        let repo = conversation_repo().await;
        for (sid, content) in [("s-a", "a1"), ("s-b", "b1"), ("s-a", "a2")] {
            log_turn(
                &*repo,
                LogTurnRequest {
                    session_id: sid.to_string(),
                    role: "user".to_string(),
                    content: content.to_string(),
                },
            )
            .await
            .unwrap();
        }
        let listed = list_sessions(&*repo, ListSessionsRequest::default())
            .await
            .unwrap();
        assert_eq!(
            listed.sessions,
            vec!["s-a".to_string(), "s-b".to_string()],
            "a 有二轮（最近活跃）必须排前"
        );
        let filtered = list_sessions(
            &*repo,
            ListSessionsRequest {
                prefix: Some("s-b".to_string()),
            },
        )
        .await
        .unwrap();
        assert_eq!(filtered.sessions, vec!["s-b".to_string()]);
    }

    // -----------------------------------------------------------------------
    // §11 回归（docs/handoff-integration-design.md 七条回归的 MCP 层钉死）
    // -----------------------------------------------------------------------

    /// 回归 #3：`search_nodes` 不受交接影响——写入多条交接前后，记忆检索结果一致
    /// （钉死 §2 的「交接与记忆分表」决策：交接正文不得渗进记忆检索）。
    #[tokio::test]
    async fn handoff_writes_leave_search_nodes_untouched() {
        let backend = InMemBackend::new();
        let repos = backend.repositories_for("t").await.unwrap();
        create_entities(
            &*repos.memory,
            CreateEntitiesRequest {
                entities: vec![entity("深色主题", "preference")],
            },
        )
        .await
        .unwrap();
        add_observations(
            &*repos.memory,
            AddObservationsRequest {
                observations: vec![observation("深色主题", &["用户偏好深色界面"])],
            },
        )
        .await
        .unwrap();

        let before = search_nodes(
            &*repos.memory,
            SearchNodesRequest {
                query: "深色".to_string(),
            },
        )
        .await
        .unwrap();

        // 写入多条交接：目标/状态里刻意包含检索词「深色」——
        // 若交接表混进记忆检索，此处必然污染快照
        let handoff = &*repos.handoff;
        handoff_save(handoff, save_req("深色主题迁移", "深色改造进行中"))
            .await
            .unwrap();
        handoff_save(handoff, save_req("深色收尾", "待验收"))
            .await
            .unwrap();

        let after = search_nodes(
            &*repos.memory,
            SearchNodesRequest {
                query: "深色".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(before, after, "交接写入不得改变记忆检索结果");
    }

    /// 回归 #4：零 ID 主路径端到端——`handoff_save`（不给 task_id）→ 另一客户端
    /// `handoff_load{}` → `handoff_ack` 成功接手，全程**没有任何人手输的标识符**；
    /// task_id 只在工具出参里流动（§3.7 UX 承诺的唯一硬保障）。
    #[tokio::test]
    async fn zero_id_path_is_end_to_end_without_user_supplied_ids() {
        let repo = handoff_repo().await;
        // 写入方：什么都不给 task_id
        handoff_save(
            &*repo,
            save_req("交接功能收尾", "测试全绿，待另一台机器接手验收"),
        )
        .await
        .unwrap();

        // 接手方：零参数 load
        let res = handoff_load(&*repo, HandoffLoadRequest::default())
            .await
            .unwrap();
        assert_eq!(res.status, HandoffStatus::Found);
        let h = res.handoff.expect("零 ID 主路径必须直接命中");
        let task_id = h.task_id; // ← 标识符唯一来源是工具出参

        // 接手方 ack：task_id 同样取自出参，不是用户记忆
        let ack = handoff_ack(
            &*repo,
            HandoffAckRequest {
                task_id: Some(task_id),
                client: "cursor".to_string(),
            },
        )
        .await
        .unwrap();
        assert!(ack.won, "唯一待接手必须被 ack 命中");
        assert_eq!(ack.handoff.unwrap().acked_by, "cursor");
    }

    /// 回归 #5：多写入方零 ID 保存互不遮蔽——两个 client 各存一条后，零参数 load
    /// 必须返回 ambiguous 且候选同时含两者（H8 的 MCP 层钉死，防「后写遮蔽先写」回归）。
    #[tokio::test]
    async fn two_writers_zero_id_saves_stay_mutually_visible() {
        let repo = handoff_repo().await;
        let mut a = save_req("zed 上的活", "进行中");
        a.client = "zed".to_string();
        let mut b = save_req("cursor 上的活", "进行中");
        b.client = "cursor".to_string();
        handoff_save(&*repo, a).await.unwrap();
        handoff_save(&*repo, b).await.unwrap();

        let res = handoff_load(&*repo, HandoffLoadRequest::default())
            .await
            .unwrap();
        assert_eq!(res.status, HandoffStatus::Ambiguous);
        assert!(res.handoff.is_none(), "歧义时绝不代选");
        let mut ids: Vec<String> = res.candidates.iter().map(|c| c.task_id.clone()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["current@cursor".to_string(), "current@zed".to_string()]
        );
    }

    /// 回归 #6：列表可用性端到端——A 端带 `session_id` 存，B 端只靠 `handoff_list`
    /// 的摘要（session_id + state_excerpt）认出它，再用摘要里的 task_id 完成 load + ack。
    /// 「人在列表里认、不靠记忆」这一交互被此用例钉住：删掉摘要字段它才会红。
    #[tokio::test]
    async fn list_summary_carries_enough_to_finish_the_job() {
        let repo = handoff_repo().await;
        let mut req = save_req("广西频道评审修复", "GX-25-01~31 修复中");
        req.session_id = "A1".to_string();
        handoff_save(&*repo, req).await.unwrap();

        let list = handoff_list(
            &*repo,
            HandoffListRequest {
                unacked_only: true,
                limit: DEFAULT_HANDOFF_LIST_LIMIT,
                prefix: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(list.handoffs.len(), 1);
        let summary = &list.handoffs[0];
        assert_eq!(summary.session_id, "A1", "摘要必须带 session_id 供人辨认");
        assert!(
            summary.state_excerpt.contains("GX-25"),
            "摘要必须带状态片段"
        );

        let loaded = handoff_load(
            &*repo,
            HandoffLoadRequest {
                task_id: Some(summary.task_id.clone()),
            },
        )
        .await
        .unwrap();
        assert_eq!(loaded.status, HandoffStatus::Found);

        let ack = handoff_ack(
            &*repo,
            HandoffAckRequest {
                task_id: Some(summary.task_id.clone()),
                client: "workbuddy".to_string(),
            },
        )
        .await
        .unwrap();
        assert!(ack.won);
    }
}
