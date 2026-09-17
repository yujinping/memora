//! 9 个 MCP 工具的后端无关语义实现。
//!
//! @author yujinping
//! @intent P3：把「协议适配」与「工具语义」拆开——本模块只依赖 `MemoryRepository`
//!         与入参 / 出参契约，可脱离 MCP 传输层用内存后端做单测；
//!         协议侧的宏、会话、HTTP 细节全部留在 `mcp::server`。
//!         这也是 docs §12.5「handler 层对存储实现完全盲视」在 MCP 侧的落点。

use crate::domain::{EntityInput, ObservationInput, RelationInput};
use crate::error::StorageError;
use crate::mcp::dto::*;
use crate::storage::{normalize, MemoryRepository};
use rmcp::ErrorData;

/// 工具名清单（顺序即 `tools/list` 的稳定顺序）。
///
/// @intent 这是「对外承诺的工具面」的单一来源：`mcp::build_service` 在启动时用它做装配自检，
///         路由层测试用它校验 `tools/list` 的实际返回值。文档写了 9 个工具、
///         实际只注册 8 个这类问题因此无法静默存在。
pub const TOOL_NAMES: [&str; 9] = [
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

/// 存储错误 → 工具错误。
///
/// @intent 区分「调用方可修正」与「服务端故障」：入参非法、实体不存在属于前者（-32602），
///         后端不可用 / 项目 id 非法属于后者（-32603）。调用方据此决定是否重试或改参数。
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
        StorageError::Backend(m) => ErrorData::internal_error(m, None),
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

    // 先用共享规范化器取「规范名」，再据此回读：写路径与读路径必须用同一套去空白规则，
    // 否则「写入带空白名字 → 按原样回读」会漏命中。
    let names: Vec<String> = normalize::entities(&inputs)
        .map_err(tool_error)?
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();

    repo.create_entities(inputs).await.map_err(tool_error)?;
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

    fn entity(name: &str, ty: &str) -> EntityParam {
        EntityParam {
            name: name.to_string(),
            entity_type: ty.to_string(),
            source: String::new(),
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
        assert!(r.id > 0, "回读的关系必须带 id，否则无法调用 delete_relations");
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
        let ids: Vec<i64> = added.entities[0].observations.iter().map(|o| o.id).collect();

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
        assert_code(err, INVALID_PARAMS);
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
        create_entities(
            &*repo,
            CreateEntitiesRequest { entities: vec![e] },
        )
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
        create_relations(
            &*repo,
            CreateRelationsRequest { relations: vec![r] },
        )
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
            tool_error(StorageError::InvalidInput("x".to_string())).code.0,
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

    /// 工具名清单必须与 `tools/list` 暴露的 9 个工具一致（防改名 / 漏接）。
    #[test]
    fn tool_names_cover_the_nine_memory_tools() {
        assert_eq!(TOOL_NAMES.len(), 9);
        let unique: std::collections::HashSet<&&str> = TOOL_NAMES.iter().collect();
        assert_eq!(unique.len(), 9, "工具名不得重复");
        assert!(TOOL_NAMES.contains(&"read_graph"));
    }
}
