//! MCP 工具层的入参 / 出参数据契约。
//!
//! @author yujinping
//! @intent P3：工具形态对齐官方 MCP Memory 的「实体 - 关系 - 观测」三元组，
//!         使既有客户端与代理提示词无需改造即可接入。
//!         字段统一用 snake_case 与领域模型一致；同时为官方 schema 的 camelCase 写法
//!         （`entityType` / `relationType` / `entityName` / `from` / `to`）保留 serde 别名，
//!         容忍模型照抄旧 schema。别名不进入 JSON Schema——对外只宣传规范字段名。
//!
//! @intent 全部入参结构体启用 `deny_unknown_fields`：字段名写错、或把两个工具的入参混在
//!         一层（如顶层同时给 `entities` 与 `observations`）时**必须报错**。默认的「忽略
//!         未知字段」会让服务端返回 200 而数据静默消失，调用方拿不到任何线索——这是最坏
//!         的失败形态。同时它对模型是**可自我修正**的：错误信息会点名出错字段。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

/// `create_entities` 的元素。
///
/// @intent 带 `observations`，对齐官方 schema 的「一次调用建实体并附带事实」形态：
///         不接这个字段时，官方习惯的调用会**静默丢观测**（服务端返回 200，数据为空）。
///         观测的来源继承实体的 `source`——同一次调用视为同一来源。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntityParam {
    /// 实体名（项目内唯一，作为主键；重复创建同名实体不会覆盖其类型）
    pub name: String,
    /// 实体类型，如 person / project / concept；省略或留空时落为 `unknown`
    #[serde(default, alias = "entityType")]
    pub entity_type: String,
    /// 建实体时一并写入的观测（按给定顺序落库）；省略或空数组表示不带观测
    #[serde(default)]
    pub observations: Vec<String>,
    /// 写入来源标识（哪个客户端 / 助手），省略或留空落为空串；仅首次登记生效
    #[serde(default)]
    pub source: String,
}

/// `create_relations` 的元素。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// 写入来源标识，省略或留空落为空串；仅首次落库生效（不参与判重）
    #[serde(default)]
    pub source: String,
}

/// `add_observations` 的元素：一次调用为同一实体追加多条事实。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservationParam {
    /// 目标实体名；实体必须已存在，否则返回 `-32602`
    #[serde(alias = "entityName")]
    pub entity_name: String,
    /// 追加的事实内容列表
    pub contents: Vec<String>,
    /// 写入来源标识，省略或留空落为空串
    #[serde(default)]
    pub source: String,
}

/// `create_entities` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateEntitiesRequest {
    /// 待创建的实体列表
    pub entities: Vec<EntityParam>,
}

/// `create_relations` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateRelationsRequest {
    /// 待创建的关系列表；三元组重复时跳过
    pub relations: Vec<RelationParam>,
}

/// `add_observations` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddObservationsRequest {
    /// 待追加的观测列表
    pub observations: Vec<ObservationParam>,
}

/// `delete_entities` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteEntitiesRequest {
    /// 待删除的实体名；连同其观测、关联关系与检索索引一并级联删除
    pub names: Vec<String>,
}

/// `delete_observations` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteObservationsRequest {
    /// 待删除的观测 id（由 `read_graph` / `search_nodes` / `open_nodes` 获得）
    pub ids: Vec<i64>,
}

/// `delete_relations` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteRelationsRequest {
    /// 待删除的关系 id（由 `read_graph` 获得）
    pub ids: Vec<i64>,
}

/// `search_nodes` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchNodesRequest {
    /// 关键词；命中实体名、实体类型或观测内容，结果按相关度（bm25）排序
    pub query: String,
}

/// `open_nodes` 入参。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// 写入来源标识，空串 = 未声明
    pub source: String,
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
    /// 登记来源标识，空串 = 未声明
    pub source: String,
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
    /// 写入来源标识，空串 = 未声明
    pub source: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 未知字段必须**硬失败**，而不是被 serde 静默忽略。
    ///
    /// @intent 原先全仓库没有 `deny_unknown_fields`：字段名拼错、或按别家 schema 习惯
    ///         传参时，服务端一律返回 `200` 而数据静默消失（实测 `create_entities` 带
    ///         `observations` 时返回体里是 `"observations": []`）。「成功但无数据」是最坏的
    ///         失败形态——调用方拿不到任何线索，只会在很久以后发现记忆是空的。
    ///         故此处宁可报错也不静默：字段名写错必须在**调用当时**暴露。
    #[test]
    fn unknown_fields_are_rejected_instead_of_silently_ignored() {
        let err = serde_json::from_value::<EntityParam>(json!({
            "name": "alpha",
            "observation": ["字段名写成了单数"]
        }))
        .expect_err("未知字段必须报错");

        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "错误信息应点明是「未知字段」，实际：{msg}"
        );
        assert!(
            msg.contains("observation"),
            "错误信息应点名出错字段，便于调用方自我修正，实际：{msg}"
        );
    }

    /// 最现实的误用：把 `create_entities` 与 `add_observations` 混在同一层，
    /// 顶层同时给 `entities` 和 `observations`。若放行，后者会整批静默消失。
    #[test]
    fn extra_field_beside_a_valid_one_is_rejected_not_dropped() {
        let err = serde_json::from_value::<CreateEntitiesRequest>(json!({
            "entities": [{"name": "alpha"}],
            "observations": [{"entity_name": "alpha", "contents": ["fact"]}]
        }))
        .expect_err("顶层多余字段必须报错，否则 observations 会被静默丢弃");

        assert!(
            err.to_string().contains("unknown field"),
            "实际：{err}"
        );
    }

    /// 加了 `deny_unknown_fields` 后，规范 snake_case 与官方 camelCase 别名必须都仍可用。
    #[test]
    fn deny_unknown_fields_coexists_with_the_official_aliases() {
        let canonical: EntityParam =
            serde_json::from_value(json!({"name": "a", "entity_type": "person", "source": "wb"}))
                .expect("规范字段名必须可用");
        assert_eq!(canonical.entity_type, "person");

        let alias: EntityParam =
            serde_json::from_value(json!({"name": "a", "entityType": "person"}))
                .expect("官方 camelCase 别名必须仍可用");
        assert_eq!(alias.entity_type, "person");

        let rel: RelationParam =
            serde_json::from_value(json!({"from": "a", "to": "b", "relationType": "knows"}))
                .expect("关系的官方别名必须仍可用");
        assert_eq!(
            (
                rel.from_name.as_str(),
                rel.to_name.as_str(),
                rel.relation_type.as_str()
            ),
            ("a", "b", "knows")
        );
    }

    /// 对外 schema 必须声明 `additionalProperties: false`，让调用的模型在**生成参数时**
    /// 就知道不许加额外字段，而不是等打到服务端才被拒。
    #[test]
    fn generated_schemas_forbid_additional_properties() {
        let schemas: Vec<(&str, serde_json::Value)> = vec![
            (
                "EntityParam",
                serde_json::to_value(schemars::schema_for!(EntityParam)).unwrap(),
            ),
            (
                "RelationParam",
                serde_json::to_value(schemars::schema_for!(RelationParam)).unwrap(),
            ),
            (
                "ObservationParam",
                serde_json::to_value(schemars::schema_for!(ObservationParam)).unwrap(),
            ),
            (
                "CreateEntitiesRequest",
                serde_json::to_value(schemars::schema_for!(CreateEntitiesRequest)).unwrap(),
            ),
        ];

        for (label, schema) in schemas {
            assert_eq!(
                schema["additionalProperties"],
                json!(false),
                "{label} 的 JSON Schema 应声明 additionalProperties: false"
            );
        }
    }
}
