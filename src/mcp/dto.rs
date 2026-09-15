//! MCP 工具层的入参 / 出参数据契约。
//!
//! @author yujinping
//! @intent P3：工具形态对齐官方 MCP Memory 的「实体 - 关系 - 观测」三元组，
//!         使既有客户端与代理提示词无需改造即可接入。
//!         字段统一用 snake_case 与领域模型一致；同时为官方 schema 的 camelCase 写法
//!         （`entityType` / `relationType` / `entityName` / `from` / `to`）保留 serde 别名，
//!         容忍模型照抄旧 schema。别名不进入 JSON Schema——对外只宣传规范字段名。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

/// `create_entities` 的元素。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EntityParam {
    /// 实体名（项目内唯一，作为主键；重复创建同名实体不会覆盖其类型）
    pub name: String,
    /// 实体类型，如 person / project / concept；省略或留空时落为 `unknown`
    #[serde(default, alias = "entityType")]
    pub entity_type: String,
}

/// `create_relations` 的元素。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RelationParam {
    /// 起点实体名
    #[serde(alias = "from")]
    pub from_name: String,
    /// 终点实体名
    #[serde(alias = "to")]
    pub to_name: String,
    /// 关系类型，如 works_at / depends_on
    #[serde(alias = "relationType")]
    pub relation_type: String,
}

/// `add_observations` 的元素：一次调用为同一实体追加多条事实。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ObservationParam {
    /// 目标实体名；实体必须已存在，否则返回 `-32602`
    #[serde(alias = "entityName")]
    pub entity_name: String,
    /// 追加的事实内容列表
    pub contents: Vec<String>,
}

/// `create_entities` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateEntitiesRequest {
    /// 待创建的实体列表
    pub entities: Vec<EntityParam>,
}

/// `create_relations` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateRelationsRequest {
    /// 待创建的关系列表；三元组重复时跳过
    pub relations: Vec<RelationParam>,
}

/// `add_observations` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddObservationsRequest {
    /// 待追加的观测列表
    pub observations: Vec<ObservationParam>,
}

/// `delete_entities` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeleteEntitiesRequest {
    /// 待删除的实体名；连同其观测、关联关系与检索索引一并级联删除
    pub names: Vec<String>,
}

/// `delete_observations` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeleteObservationsRequest {
    /// 待删除的观测 id（由 `read_graph` / `search_nodes` / `open_nodes` 获得）
    pub ids: Vec<i64>,
}

/// `delete_relations` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeleteRelationsRequest {
    /// 待删除的关系 id（由 `read_graph` 获得）
    pub ids: Vec<i64>,
}

/// `search_nodes` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchNodesRequest {
    /// 关键词；命中实体名、实体类型或观测内容，结果按相关度（bm25）排序
    pub query: String,
}

/// `open_nodes` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenNodesRequest {
    /// 待展开的实体名；未命中的名字被跳过
    pub names: Vec<String>,
}

// ---------------------------------------------------------------------------
// 出参
// ---------------------------------------------------------------------------

/// 观测视图。
///
/// @intent 带上 `id`：`delete_observations` 只接受 id，不回读则调用方无法构造删除请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ObservationView {
    /// 观测 id
    pub id: i64,
    /// 观测内容
    pub content: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
}

/// 实体视图（含其全部观测）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EntityView {
    /// 实体名
    pub name: String,
    /// 实体类型
    pub entity_type: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 该实体的观测（按 id 升序）
    pub observations: Vec<ObservationView>,
}

/// 关系视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RelationView {
    /// 关系 id
    pub id: i64,
    /// 起点实体名
    pub from_name: String,
    /// 终点实体名
    pub to_name: String,
    /// 关系类型
    pub relation_type: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
}

/// 实体集合出参：`create_entities` / `add_observations` / `open_nodes` / `search_nodes` 共用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EntitiesResponse {
    /// 实体列表（含观测）
    pub entities: Vec<EntityView>,
}

/// 关系集合出参：`create_relations` 使用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RelationsResponse {
    /// 关系列表
    pub relations: Vec<RelationView>,
}

/// 全图出参：`read_graph` 使用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GraphResponse {
    /// 全部实体（按名升序）
    pub entities: Vec<EntityView>,
    /// 全部关系（按 id 升序）
    pub relations: Vec<RelationView>,
}

/// 实体删除出参。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeleteEntitiesResponse {
    /// 实际存在并被删除的实体名（未命中的名字被忽略，故不回显）
    pub deleted: Vec<String>,
}

/// 观测删除出参。
///
/// @intent 只回显请求 id 而不回读「确实存在过」的集合：仓库契约未提供按 id 的存在性查询，
///         为一次删除引入全图扫描不划算。删除本身是幂等的，调用方据此即可确认语义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeleteObservationsResponse {
    /// 本次请求的观测 id（去重后按原序回显）
    pub ids: Vec<i64>,
}

/// 关系删除出参。语义同 [`DeleteObservationsResponse`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeleteRelationsResponse {
    /// 本次请求的关系 id（去重后按原序回显）
    pub ids: Vec<i64>,
}
