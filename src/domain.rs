//! 记忆领域模型（后端无关）。
//!
//! @author yujinping
//! @intent P2：以「实体 - 关系 - 观测」三元组为领域中心，定义输入 DTO 与返回模型。
//!          MCP 工具层（P3）与 REST 管理层（P4）只依赖本模块类型，
//!          不感知底层是 SQLite 文件 / 单库 / Postgres，见 docs §12。
//!
//! 注：P2 阶段本模块的写侧类型（各类 Input）曾仅由测试构造；P3 起 MCP 工具层、
//!     P4 起管理面统计均已使用，模块级 `allow(dead_code)` 已移除。
/// 实体创建输入。
///
/// @intent 与官方 MCP Memory 的 `create_entities` 入参对齐，保证客户端零改造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityInput {
    /// 实体名（项目内唯一，作为主键）
    pub name: String,
    /// 实体类型，如 person / project / concept；缺省为 unknown
    pub entity_type: String,
    /// 写入来源标识（哪个客户端 / 助手），空串表示未声明；仅首次登记生效
    pub source: String,
}

impl EntityInput {
    /// 构造实体输入；`entity_type` 传空串时由存储层落为 `unknown`（来源未声明）。
    pub fn new(name: impl Into<String>, entity_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entity_type: entity_type.into(),
            source: String::new(),
        }
    }

    /// 链式声明来源（`EntityInput::new(..).with_source("workbuddy")`）。
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }
}

/// 关系创建输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationInput {
    /// 起点实体名
    pub from_name: String,
    /// 终点实体名
    pub to_name: String,
    /// 关系类型，如 works_at / depends_on
    pub relation_type: String,
    /// 写入来源标识，空串表示未声明；仅首次落库生效（来源不参与判重）
    pub source: String,
}

impl RelationInput {
    /// 构造关系输入（来源未声明）。
    pub fn new(
        from_name: impl Into<String>,
        to_name: impl Into<String>,
        relation_type: impl Into<String>,
    ) -> Self {
        Self {
            from_name: from_name.into(),
            to_name: to_name.into(),
            relation_type: relation_type.into(),
            source: String::new(),
        }
    }

    /// 链式声明来源。
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }
}

/// 观测追加输入：一次调用可为同一实体追加多条事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationInput {
    /// 目标实体名（必须已存在，否则报 `EntityNotFound`）
    pub entity_name: String,
    /// 追加的事实内容列表
    pub contents: Vec<String>,
    /// 写入来源标识，空串表示未声明
    pub source: String,
}

impl ObservationInput {
    /// 构造观测输入（来源未声明）。
    pub fn new(entity_name: impl Into<String>, contents: Vec<String>) -> Self {
        Self {
            entity_name: entity_name.into(),
            contents,
            source: String::new(),
        }
    }

    /// 链式声明来源。
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// 便捷构造：单条观测。
    ///
    /// @intent 仅测试与共享契约使用，故不进入非测试构建。
    #[cfg(test)]
    pub fn single(entity_name: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(entity_name, vec![content.into()])
    }
}

/// 观测（实体的一条事实）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// 观测主键（同一项目库内自增）
    pub id: i64,
    /// 所属实体名
    pub entity_name: String,
    /// 事实内容
    pub content: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 写入来源标识，空串表示未声明
    pub source: String,
}

/// 实体（含其全部观测）。
///
/// @intent 对应设计文档中的 Node：检索与展开共用同一模型，避免两套结构互相转换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    /// 实体名（主键）
    pub name: String,
    /// 实体类型
    pub entity_type: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 登记来源标识，空串表示未声明；同名实体首次登记生效
    pub source: String,
    /// 该实体的观测列表，按 id 升序
    pub observations: Vec<Observation>,
}

/// 关系。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// 关系主键（同一项目库内自增）
    pub id: i64,
    /// 起点实体名
    pub from_name: String,
    /// 终点实体名
    pub to_name: String,
    /// 关系类型
    pub relation_type: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 写入来源标识，空串表示未声明
    pub source: String,
}

/// 全图快照：实体（含观测）+ 关系。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    /// 全部实体，按 name 升序
    pub entities: Vec<Entity>,
    /// 全部关系，按 id 升序
    pub relations: Vec<Relation>,
}

impl Graph {
    /// 实体数量。
    ///
    /// @intent P4 起由管理面用量统计（`GET /api/v1/projects/{id}/stats`）使用。
    ///         计数一律从 `read_graph` 派生，使统计口径与客户端看到的数据同源。
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// 关系数量。
    pub fn relation_count(&self) -> usize {
        self.relations.len()
    }

    /// 观测总数（跨实体累加）。
    pub fn observation_count(&self) -> usize {
        self.entities.iter().map(|e| e.observations.len()).sum()
    }
}
