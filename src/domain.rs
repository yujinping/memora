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

// ---------------------------------------------------------------------------
// 交接（handoff）领域模型
//
// @intent P4.3：交接与记忆是**两种东西**，故类型也分列于本模块下游（见 docs §2）：
//         记忆是幂等合并的知识，交接是追加修订的**过程状态**。混用类型会让
//         「同一张表两种写语义」的诱惑一直存在，故类型层面就分开。
// ---------------------------------------------------------------------------

/// 交接保存输入。
///
/// @intent `task_id` 为 `None` 表示「落到按写入方分片的默认槽位」（docs §3.7）：
///         交接者无需为一个临时标记起名，这是零 ID 主路径的入口。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoffInput {
    /// `None` = 落到默认槽位 `current@<client>`；`Some` 时须通过 task_id 校验
    pub task_id: Option<String>,
    /// 这活要达成什么（必填，非空）
    pub goal: String,
    /// 现在到哪了（必填，非空）。只写结论性状态，不写过程
    pub state: String,
    /// 下一步要做什么（可空数组 = 明确的「无事可做」）
    pub next_steps: Vec<String>,
    /// 关键文件 / 位置（接手方最需要的东西：去哪看）
    pub files: Vec<String>,
    /// 已定但接手方不知道就会重开讨论的事
    pub decisions: Vec<String>,
    /// 需要人拍板的
    pub open_questions: Vec<String>,
    /// 写入方标识，空串 = 未声明。**同时是零 ID 保存的分片键**
    pub client: String,
    /// 写入方会话标识，空串 = 未声明。仅作溯源展示，不参与寻址
    pub session_id: String,
}

#[cfg(test)]
impl HandoffInput {
    /// 构造输入（`task_id` 缺省 = 落分片槽位，来源未声明）。
    pub fn new(goal: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            state: state.into(),
            ..Self::default()
        }
    }

    /// 链式指定任务标识（刻意并行多条工作流时才需要）。
    pub fn with_task(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    /// 链式声明写入方（分片键）。
    pub fn with_client(mut self, client: impl Into<String>) -> Self {
        self.client = client.into();
        self
    }

    /// 链式声明会话标签（展示用）。
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    /// 链式设置下一步列表。
    pub fn with_next_steps(mut self, steps: Vec<String>) -> Self {
        self.next_steps = steps;
        self
    }

    /// 链式设置文件列表。
    pub fn with_files(mut self, files: Vec<String>) -> Self {
        self.files = files;
        self
    }

    /// 链式设置已定事项列表。
    pub fn with_decisions(mut self, decisions: Vec<String>) -> Self {
        self.decisions = decisions;
        self
    }

    /// 链式设置待拍板列表。
    pub fn with_open_questions(mut self, questions: Vec<String>) -> Self {
        self.open_questions = questions;
        self
    }
}

/// 落库后的完整交接修订（含接手状态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// 任务标识。落库后**总有值**（默认槽位或调用方显式指定）
    pub task_id: String,
    /// 修订号，从 1 单调递增；同一 task 内不可变
    pub revision: i64,
    /// 目标
    pub goal: String,
    /// 进展
    pub state: String,
    /// 下一步
    pub next_steps: Vec<String>,
    /// 关键文件
    pub files: Vec<String>,
    /// 已定事项
    pub decisions: Vec<String>,
    /// 待拍板事项
    pub open_questions: Vec<String>,
    /// 写入方标识，空串 = 未声明
    pub client: String,
    /// 写入方会话标签，空串 = 未声明
    pub session_id: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 接手时间；`None` = 尚未被接手
    pub acked_at: Option<i64>,
    /// 接手方标识，空串 = 未声明
    pub acked_by: String,
}

impl Handoff {
    /// 是否处于「待接手」状态。
    pub fn is_pending(&self) -> bool {
        self.acked_at.is_none()
    }
}

/// list 用的轻量摘要：只留「**认得出是哪一条**」所需的信息。
///
/// @intent 这是选择界面的数据来源，字段取舍的标准不是「信息完整」而是「可辨认」：
///         要在一屏 5 条里认出「就是那个」，靠的是 goal + 状态片段 + 谁写的 +
///         什么时候 + 会话标签。正文（next_steps / decisions）不进摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffSummary {
    /// 任务标识（挑选后用它与 load / ack 精确寻址）
    pub task_id: String,
    /// 最新修订号
    pub revision: i64,
    /// 目标
    pub goal: String,
    /// 状态首行截断（≤ [`crate::storage::normalize::HANDOFF_EXCERPT_CHARS`] 字符）
    pub state_excerpt: String,
    /// 写入方标识，空串 = 未声明
    pub client: String,
    /// 写入方会话标签。**不是地址，只是一列可辨认的展示信息**
    pub session_id: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 最新修订是否已被接手
    pub acked: bool,
    /// 接手方标识，空串 = 未声明
    pub acked_by: String,
    /// 下一步条数（摘要里给个规模感，不给正文）
    pub next_step_count: usize,
}

/// 寻址结果：三种都是**正常返回**，没有一种是错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffResolution {
    /// 命中唯一一条
    ///
    /// @intent 装箱是有意为之：`Handoff` 约 272 字节，而 `None` 变体为空——
    ///         不装箱会让每个 `HandoffResolution` 都按最大变体搬运（clippy
    ///         `large_enum_variant`）。寻址是冷路径，一次堆分配的代价可忽略。
    Found(Box<Handoff>),
    /// 没有可接手的交接（项目里从来没有，或全部都已被接手）
    None,
    /// 有多条同时待接手，无法替调用方决定；带摘要让调用方挑选后再用 ID 精确取
    Ambiguous(Vec<HandoffSummary>),
}

/// ack 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckOutcome {
    /// 寻址结果（与 load 共用同一份寻址语义，避免两处漂移）
    pub resolution: HandoffResolution,
    /// 本次调用是否真的抢到了接手权（CAS 的结果）
    pub won: bool,
}

/// 默认槽位前缀：`current` 或 `current@<client>`（见 docs §3.7）。
pub const HANDOFF_DEFAULT_SLOT: &str = "current";

/// 分片后缀的最大长度，使 `current@<suffix>` 始终远小于 task_id 的 128 上限。
const HANDOFF_SLOT_SUFFIX_MAX: usize = 64;

/// 由写入方标识推导默认槽位名：`client` 为空则为 `current`，否则 `current@<client>`。
///
/// @intent 写入方标识是自由文本（可能是 `Claude Code`、`中文客户端`），而 task_id 有
///         字符集限制 `[A-Za-z0-9._@-]`（docs §3.2）。若不规范化，一个带空格的
///         client 名会让**零 ID 主路径直接报错**——那正是本设计最不能失败的地方。
///         故此处把 client 规范化为安全的槽位后缀：不可用字符折为 `-`、连续 `-` 收敛、
///         首尾 `-` 去掉、超长截断；规范化后为空则退回项目级槽位 `current`。
///
/// @intent 规范化是**确定性的且结果可见**（save 把最终 task_id 回传给调用方），
///         因此不构成「静默改写」：调用方随时能看到自己落在了哪个槽位。
pub fn default_handoff_task_id(client: &str) -> String {
    match slot_suffix(client) {
        Some(suffix) => format!("{HANDOFF_DEFAULT_SLOT}@{suffix}"),
        None => HANDOFF_DEFAULT_SLOT.to_string(),
    }
}

/// 规范化分片后缀；`None` 表示应退回无后缀的项目级槽位。
fn slot_suffix(client: &str) -> Option<String> {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in client.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            // 不可用字符折为单个 `-`，但延后到下一个可用字符出现时才落笔，
            // 这样连续不可用字符与结尾的不可用字符都不会留下多余的分隔符。
            pending_dash = true;
        }
        if out.chars().count() >= HANDOFF_SLOT_SUFFIX_MAX {
            break;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;

    /// 零 ID 主路径的基石：同一个 client 必须永远落回同一个槽位。
    #[test]
    fn default_slot_is_sharded_by_client() {
        assert_eq!(default_handoff_task_id("zed"), "current@zed");
        assert_eq!(default_handoff_task_id("  workbuddy  "), "current@workbuddy");
    }

    /// 未声明 client 时退回项目级单槽位（这是「没填 client」时唯一可用的语义）。
    #[test]
    fn default_slot_falls_back_when_client_is_blank() {
        assert_eq!(default_handoff_task_id(""), "current");
        assert_eq!(default_handoff_task_id("   "), "current");
    }

    /// 带空格 / 标点的 client 名不得让零 ID 保存失败——规范化后仍须是合法 task_id。
    #[test]
    fn default_slot_sanitizes_client_into_a_valid_task_id_shape() {
        let slot = default_handoff_task_id("Claude Code (v2)");
        assert_eq!(slot, "current@Claude-Code-v2");
        assert!(
            slot.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._@-".contains(c)),
            "推导出的槽位必须是合法 task_id：{slot}"
        );

        // 纯非 ASCII 的客户端名规范化后为空 → 退回项目级槽位，而不是产出非法 task_id
        assert_eq!(default_handoff_task_id("中文客户端"), "current");
    }

    /// 超长 client 名不得把 task_id 顶过 128 字符上限。
    #[test]
    fn default_slot_bounds_the_suffix_length() {
        let long = "z".repeat(500);
        let slot = default_handoff_task_id(&long);
        assert!(slot.starts_with("current@"));
        assert!(
            slot.chars().count() <= 128,
            "槽位长度 {} 超过 task_id 上限",
            slot.chars().count()
        );
    }

    /// 同一 client 名的不同书写（前后空白）必须收敛到同一槽位，否则分片会碎掉。
    #[test]
    fn default_slot_is_stable_across_trimming() {
        assert_eq!(
            default_handoff_task_id("zed"),
            default_handoff_task_id("  zed  ")
        );
    }
}
