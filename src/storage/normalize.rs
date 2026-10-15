//! 入参规范化与校验（各后端共用）。
//!
//! @author yujinping
//! @intent P2：契约要求「整批非法即整批拒绝」，因此校验必须在任何写入之前一次性完成。
//!          规则由两个后端共用，避免内存后端与 SQLite 后端的语义漂移
//!          （行为等价是「换后端不改业务代码」的前提）。

use crate::domain::{EntityInput, HandoffInput, ObservationInput, RelationInput, SessionInput};
use crate::error::StorageError;

/// 规范化单个实体：名称去空白后不得为空；类型为空则落为 `unknown`；来源仅去空白（可空）。
///
/// 返回三元组 `(name, entity_type, source)`。
pub fn entity(input: &EntityInput) -> Result<(String, String, String), StorageError> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(StorageError::InvalidInput(
            "entity name must be non-empty".to_string(),
        ));
    }
    let entity_type = match input.entity_type.trim() {
        "" => "unknown".to_string(),
        t => t.to_string(),
    };
    Ok((
        name.to_string(),
        entity_type,
        input.source.trim().to_string(),
    ))
}

/// 规范化实体批次。
pub fn entities(inputs: &[EntityInput]) -> Result<Vec<(String, String, String)>, StorageError> {
    inputs.iter().map(entity).collect()
}

/// 规范化关系批次：两端与关系类型均不得为空；来源仅去空白（可空）。
pub fn relations(inputs: &[RelationInput]) -> Result<Vec<RelationKey>, StorageError> {
    inputs
        .iter()
        .map(|r| {
            let (from_name, to_name, relation_type) =
                (r.from_name.trim(), r.to_name.trim(), r.relation_type.trim());
            if from_name.is_empty() || to_name.is_empty() || relation_type.is_empty() {
                return Err(StorageError::InvalidInput(
                    "relation endpoints and type must be non-empty".to_string(),
                ));
            }
            Ok((
                from_name.to_string(),
                to_name.to_string(),
                relation_type.to_string(),
                r.source.trim().to_string(),
            ))
        })
        .collect()
}

/// 规范化观测批次：实体名与每条内容均不得为空；来源仅去空白（可空）。
pub fn observations(
    inputs: &[ObservationInput],
) -> Result<Vec<(String, Vec<String>, String)>, StorageError> {
    inputs
        .iter()
        .map(|o| {
            let entity_name = o.entity_name.trim();
            if entity_name.is_empty() {
                return Err(StorageError::InvalidInput(
                    "observation entity name must be non-empty".to_string(),
                ));
            }
            if o.contents.iter().any(|c| c.trim().is_empty()) {
                return Err(StorageError::InvalidInput(
                    "observation content must be non-empty".to_string(),
                ));
            }
            Ok((
                entity_name.to_string(),
                o.contents.iter().map(|c| c.trim().to_string()).collect(),
                o.source.trim().to_string(),
            ))
        })
        .collect()
}

/// 关系四元组：(起点, 终点, 关系类型, 来源)。
pub type RelationKey = (String, String, String, String);

// ---------------------------------------------------------------------------
// 交接（handoff）校验（docs §3.2 / §3.5）
// ---------------------------------------------------------------------------

/// `goal` / `state` 单字段上限。
pub const HANDOFF_TEXT_MAX_BYTES: usize = 16 * 1024;
/// 数组单元素上限。
pub const HANDOFF_ITEM_MAX_BYTES: usize = 4 * 1024;
/// 每个数组的元素数上限。
pub const HANDOFF_ITEMS_MAX: usize = 64;
/// 整条交接的正文总量上限。
pub const HANDOFF_TOTAL_MAX_BYTES: usize = 64 * 1024;
/// `client` / `session_id` 这类标识字段的上限（它们不是正文，但同样不该无界）。
pub const HANDOFF_IDENT_MAX_BYTES: usize = 256;
/// `task_id` 长度上限（字符）。
pub const HANDOFF_TASK_ID_MAX_CHARS: usize = 128;
/// 列表摘要里状态片段的长度上限（字符）。
pub const HANDOFF_EXCERPT_CHARS: usize = 120;

/// 校验并规范化后的交接输入：字段一律去首尾空白，`task_id` 已确定（不再有 `None`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedHandoff {
    /// 最终任务标识（显式指定或分片槽位）
    pub task_id: String,
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
}

/// 校验并规范化一条交接输入。
///
/// @intent 与记忆写侧同源：**先整批校验、再落库**，非法输入不产生半条修订（docs §3.5）。
///         `task_id` 为 `None` 时由 [`crate::domain::default_handoff_task_id`] 推导分片槽位；
///         而 `Some("")` 是**显式给了个空 ID**，属于调用方笔误，故报错而非静默落默认槽位
///         ——静默兜底会让「我明明指定了」与「系统给我换了一个」无法区分。
pub fn handoff(input: &HandoffInput) -> Result<NormalizedHandoff, StorageError> {
    let client = ident("client", &input.client)?;
    let session_id = ident("session_id", &input.session_id)?;

    let task_id = match input.task_id.as_deref() {
        None => crate::domain::default_handoff_task_id(&client),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(StorageError::InvalidInput(
                    "task_id must be non-empty when provided; omit it to use the default slot"
                        .to_string(),
                ));
            }
            if trimmed.chars().count() > HANDOFF_TASK_ID_MAX_CHARS {
                return Err(StorageError::InvalidInput(format!(
                    "task_id must be at most {HANDOFF_TASK_ID_MAX_CHARS} characters"
                )));
            }
            if let Some(bad) = trimmed
                .chars()
                .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-')))
            {
                return Err(StorageError::InvalidInput(format!(
                    "task_id contains an unsupported character {bad:?}; allowed: A-Z a-z 0-9 . _ @ -"
                )));
            }
            trimmed.to_string()
        }
    };

    let goal = text_field("goal", &input.goal)?;
    let state = text_field("state", &input.state)?;
    let next_steps = item_list("next_steps", &input.next_steps)?;
    let files = item_list("files", &input.files)?;
    let decisions = item_list("decisions", &input.decisions)?;
    let open_questions = item_list("open_questions", &input.open_questions)?;

    let total = goal.len()
        + state.len()
        + next_steps.iter().map(String::len).sum::<usize>()
        + files.iter().map(String::len).sum::<usize>()
        + decisions.iter().map(String::len).sum::<usize>()
        + open_questions.iter().map(String::len).sum::<usize>();
    if total > HANDOFF_TOTAL_MAX_BYTES {
        return Err(StorageError::InvalidInput(format!(
            "handoff body is {total} bytes, over the {HANDOFF_TOTAL_MAX_BYTES}-byte limit; \
             a handoff is a summary — use a transcript for full detail"
        )));
    }

    Ok(NormalizedHandoff {
        task_id,
        goal,
        state,
        next_steps,
        files,
        decisions,
        open_questions,
        client,
        session_id,
    })
}

/// 必填正文：去空白后非空，且不超过单字段上限。
fn text_field(name: &str, raw: &str) -> Result<String, StorageError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(StorageError::InvalidInput(format!(
            "{name} must be non-empty"
        )));
    }
    if trimmed.len() > HANDOFF_TEXT_MAX_BYTES {
        return Err(StorageError::InvalidInput(format!(
            "{name} is {} bytes, over the {HANDOFF_TEXT_MAX_BYTES}-byte limit",
            trimmed.len()
        )));
    }
    Ok(trimmed.to_string())
}

/// 可选标识：只去空白，允许为空；但不允许无界。
fn ident(name: &str, raw: &str) -> Result<String, StorageError> {
    let trimmed = raw.trim();
    if trimmed.len() > HANDOFF_IDENT_MAX_BYTES {
        return Err(StorageError::InvalidInput(format!(
            "{name} is {} bytes, over the {HANDOFF_IDENT_MAX_BYTES}-byte limit",
            trimmed.len()
        )));
    }
    Ok(trimmed.to_string())
}

/// 列表字段：条数与单元素长度受限，**保序**，空元素视为笔误报错。
///
/// @intent 不静默丢弃空元素：交接是给下一个人看的，「这条下一步是空的」只会让接手方
///         以为漏了东西；宁可让写入方现在就知道自己传了个空串。
fn item_list(name: &str, raw: &[String]) -> Result<Vec<String>, StorageError> {
    if raw.len() > HANDOFF_ITEMS_MAX {
        return Err(StorageError::InvalidInput(format!(
            "{name} has {} items, over the {HANDOFF_ITEMS_MAX}-item limit",
            raw.len()
        )));
    }
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            return Err(StorageError::InvalidInput(format!(
                "{name} contains an empty item"
            )));
        }
        if trimmed.len() > HANDOFF_ITEM_MAX_BYTES {
            return Err(StorageError::InvalidInput(format!(
                "{name} item is {} bytes, over the {HANDOFF_ITEM_MAX_BYTES}-byte limit",
                trimmed.len()
            )));
        }
        out.push(trimmed.to_string());
    }
    Ok(out)
}

/// 列表摘要用的状态片段：取首行、去空白、截断到 [`HANDOFF_EXCERPT_CHARS`] 字符。
///
/// @intent 截断必须**可见**（末尾加 `…`）而不是静默砍掉：接手方看到被截断的句子会去
///         打开正文，看到一句「完整」的话却缺少关键信息时则无从得知还有下文。
pub fn state_excerpt(state: &str) -> String {
    let first_line = state.trim().lines().next().unwrap_or("").trim();
    let mut chars = first_line.chars();
    let head: String = chars.by_ref().take(HANDOFF_EXCERPT_CHARS).collect();
    if chars.next().is_some() {
        // 超限才截断：回退 1 字符给省略号腾位，保证可见总长仍 ≤ HANDOFF_EXCERPT_CHARS。
        // 恰好达上限的句子是完整的，不得加省略号（见 normalize 用例「恰好达上限」）。
        let mut head = head;
        head.pop();
        format!("{head}…")
    } else {
        head
    }
}

// --- 交接：两后端共用的派生规则 -------------------------------------------
//
// @intent 下面几个函数是「后端必须行为一致」的部分，抽出来共用而不是各写一遍：
//         行为等价是「换后端不改业务代码」的前提，而契约测试只能事后发现漂移。

/// 由完整修订生成列表摘要（摘要口径只在这里定义一次）。
pub fn handoff_summary(h: &crate::domain::Handoff) -> crate::domain::HandoffSummary {
    crate::domain::HandoffSummary {
        task_id: h.task_id.clone(),
        revision: h.revision,
        goal: h.goal.clone(),
        state_excerpt: state_excerpt(&h.state),
        client: h.client.clone(),
        session_id: h.session_id.clone(),
        created_at: h.created_at,
        acked: h.acked_at.is_some(),
        acked_by: h.acked_by.clone(),
        next_step_count: h.next_steps.len(),
    }
}

/// **先排序、后截断**——顺序不可颠倒（docs §4.3 H11）。
///
/// @intent 把两步合成一个函数，是为了让「先 LIMIT 再 ORDER BY」这个高发缺陷
///         在后端代码里**没有地方可以写出来**：调用方拿到的是「已排好序的前 N 条」。
///         `limit == 0` 返回空集（不是「不限」——那是静默的语义陷阱）。
pub fn order_handoff_summaries(
    mut items: Vec<crate::domain::HandoffSummary>,
    limit: usize,
) -> Vec<crate::domain::HandoffSummary> {
    items.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
    if limit == 0 {
        return Vec::new();
    }
    items.truncate(limit);
    items
}

/// 数组列序列化：空数组落为 `[]`，**不落 NULL**（docs §3.3）。
pub fn handoff_array_to_json(items: &[String]) -> String {
    // `Vec<String>` 的序列化不可能失败（无自定义 Serialize、无 map 键），
    // 故此处不用 `unwrap_or_else` 兜底——静默降级成 `[]` 会悄悄丢数据。
    serde_json::to_string(items).expect("serializing Vec<String> to JSON cannot fail")
}

/// 数组列反序列化：列内容损坏属于**后端故障**（不是调用方输入问题），故归 `Backend`。
pub fn handoff_array_from_json(raw: &str) -> Result<Vec<String>, StorageError> {
    serde_json::from_str(raw)
        .map_err(|e| StorageError::Backend(format!("handoff array column is not valid JSON: {e}")))
}

/// 宽松规范化实体名列表：去空白、丢弃空项、按首次出现去重。
///
/// @intent 读路径（`open_nodes`）与删除路径（`delete_entities`）对「名字不存在」是容忍的，
///         不能套用写路径的「整批拒绝」语义；但**去空白规则必须与 [`entity`] 一致**，
///         否则写完再按名回读会漏命中。故该规则收在此处共用，并由单测钉住一致性。
pub fn name_list(names: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
            continue;
        }
        out.push(trimmed.to_string());
    }
    out
}

// ---------------------------------------------------------------------------
// 会话（P5.1，docs/memory-system-design.md §5.4 / §2 conversations 表）校验
// ---------------------------------------------------------------------------

/// 会话单轮正文上限。一轮对话可以比交接的结论字段长（它是过程记录，不是摘要），
/// 但仍须有界：transcript 是整轮整轮读回的，无界正文会拖垮「拉回完整会话」。
pub const SESSION_TEXT_MAX_BYTES: usize = 64 * 1024;

/// 会话标识长度上限（字符）。
pub const SESSION_ID_MAX_CHARS: usize = 128;

/// 合法说话角色（docs §2 表注释：`'user' | 'assistant' | 'system'`）。
///
/// @intent 枚举在**校验层**定死而不是落在库层：role 拼错（如 `Assistant`）应当在
///         写入前被拒绝，而不是落库后才在拉取 transcript 时冒出脏数据。
pub const SESSION_ROLES: [&str; 3] = ["user", "assistant", "system"];

/// 规范化一条会话轮次输入，返回 `(session_id, role, content)`（均已去首尾空白）。
///
/// @intent `session_id` 的字符集与 `task_id` 完全一致：两者都是「客户端生成、
///         要放进 URL / 日志 / MCP 参数里的定位标识」，两套规则必然造成
///         「这个名字 handoff 能用、会话不能用」的困惑。首写即建会话，
///         不存在「会话必须先创建」的额外步骤。
pub fn session_turn(input: &SessionInput) -> Result<(String, String, String), StorageError> {
    let session_id = input.session_id.trim();
    if session_id.is_empty() {
        return Err(StorageError::InvalidInput(
            "session_id must be non-empty".to_string(),
        ));
    }
    if session_id.chars().count() > SESSION_ID_MAX_CHARS {
        return Err(StorageError::InvalidInput(format!(
            "session_id must be at most {SESSION_ID_MAX_CHARS} characters"
        )));
    }
    if let Some(bad) = session_id
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-')))
    {
        return Err(StorageError::InvalidInput(format!(
            "session_id contains an unsupported character {bad:?}; allowed: A-Z a-z 0-9 . _ @ -"
        )));
    }

    let role = input.role.trim();
    if !SESSION_ROLES.contains(&role) {
        return Err(StorageError::InvalidInput(format!(
            "role must be one of {SESSION_ROLES:?}; got {:?}",
            input.role
        )));
    }

    let content = input.content.trim();
    if content.is_empty() {
        return Err(StorageError::InvalidInput(
            "content must be non-empty; an empty turn conveys nothing worth resuming".to_string(),
        ));
    }
    if content.len() > SESSION_TEXT_MAX_BYTES {
        return Err(StorageError::InvalidInput(format!(
            "content is {} bytes, over the {SESSION_TEXT_MAX_BYTES}-byte limit",
            content.len()
        )));
    }

    Ok((
        session_id.to_string(),
        role.to_string(),
        content.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_trims_and_defaults_type() {
        assert_eq!(
            entity(&EntityInput::new("  alice  ", " person ")).unwrap(),
            ("alice".to_string(), "person".to_string(), String::new())
        );
        assert_eq!(
            entity(&EntityInput::new("alice", "   ")).unwrap(),
            ("alice".to_string(), "unknown".to_string(), String::new())
        );
        assert_eq!(
            entity(&EntityInput::new("  ", "person")),
            Err(StorageError::InvalidInput(
                "entity name must be non-empty".to_string()
            ))
        );
    }

    #[test]
    fn entities_rejects_whole_batch_when_any_invalid() {
        assert!(entities(&[
            EntityInput::new("ok", "person"),
            EntityInput::new("", "person"),
        ])
        .is_err());
    }

    #[test]
    fn relations_require_both_endpoints_and_type() {
        assert_eq!(
            relations(&[RelationInput::new(" a ", " b ", " knows ")]).unwrap(),
            vec![(
                "a".to_string(),
                "b".to_string(),
                "knows".to_string(),
                String::new()
            )]
        );
        assert!(relations(&[RelationInput::new("a", "", "knows")]).is_err());
        assert!(relations(&[RelationInput::new("a", "b", "  ")]).is_err());
    }

    #[test]
    fn observations_reject_blank_content() {
        assert_eq!(
            observations(&[ObservationInput::new("a", vec![" x ".to_string()])]).unwrap(),
            vec![("a".to_string(), vec!["x".to_string()], String::new())]
        );
        assert!(observations(&[ObservationInput::single("a", "   ")]).is_err());
        assert!(observations(&[ObservationInput::new(
            "a",
            vec!["ok".to_string(), "".to_string()]
        )])
        .is_err());
    }

    /// 来源标识只去首尾空白，允许为空（未声明来源是合法状态）。
    #[test]
    fn source_is_trimmed_but_optional() {
        let (name, _, source) =
            entity(&EntityInput::new("alice", "person").with_source("  workbuddy  ")).unwrap();
        assert_eq!(name, "alice");
        assert_eq!(source, "workbuddy");

        let (_, _, source) = entity(&EntityInput::new("alice", "person")).unwrap();
        assert_eq!(source, "", "未声明来源应落为空串");

        let (from, to, rtype, source) =
            relations(&[RelationInput::new("a", "b", "knows").with_source(" cursor ")])
                .unwrap()
                .remove(0);
        assert_eq!(
            (from.as_str(), to.as_str(), rtype.as_str()),
            ("a", "b", "knows")
        );
        assert_eq!(source, "cursor");

        let (_, contents, source) =
            observations(&[ObservationInput::new("a", vec!["x".to_string()]).with_source("w")])
                .unwrap()
                .remove(0);
        assert_eq!(contents, vec!["x".to_string()]);
        assert_eq!(source, "w");
    }

    /// `name_list` 与 `entity` 的去空白规则必须一致，否则「写入后按名回读」会漏命中。
    #[test]
    fn name_list_trims_like_entity_and_drops_blanks() {
        let input = vec![
            "  a  ".to_string(),
            "a".to_string(),
            "   ".to_string(),
            "".to_string(),
            "b".to_string(),
        ];
        assert_eq!(name_list(&input), vec!["a".to_string(), "b".to_string()]);

        let (canonical, _, _) = entity(&EntityInput::new("  a  ", "")).unwrap();
        assert_eq!(
            name_list(&["  a  ".to_string()]),
            vec![canonical],
            "同一输入经写路径与读路径必须得到同一个名字"
        );
    }

    // -----------------------------------------------------------------------
    // 交接（handoff）校验
    // -----------------------------------------------------------------------

    /// 基准输入：一条合法的最小交接。
    fn base() -> HandoffInput {
        HandoffInput::new("  把 handoff 并入 memora  ", "  设计完成，待实现  ")
    }

    /// 零 ID 主路径：不给 task_id 时由 client 推导分片槽位，并统一去空白。
    #[test]
    fn handoff_defaults_to_sharded_slot_and_trims() {
        let n = handoff(&base().with_client("  zed  ")).unwrap();
        assert_eq!(n.task_id, "current@zed");
        assert_eq!(n.client, "zed");
        assert_eq!(n.goal, "把 handoff 并入 memora");
        assert_eq!(n.state, "设计完成，待实现");
        assert_eq!(n.session_id, "", "未声明会话应落空串");
        assert!(n.next_steps.is_empty(), "未给数组应落空数组");
    }

    /// 未声明 client 时退回项目级槽位（而不是报错——client 是可选字段）。
    #[test]
    fn handoff_without_client_uses_project_wide_slot() {
        assert_eq!(handoff(&base()).unwrap().task_id, "current");
    }

    /// 显式 task_id 允许 `.` 与 `@`（与 project_id 的字符集**刻意不同**，docs §3.2）。
    #[test]
    fn handoff_accepts_explicit_task_id_with_dot_and_at() {
        assert_eq!(
            handoff(&base().with_task("v1.2-api@refactor"))
                .unwrap()
                .task_id,
            "v1.2-api@refactor"
        );
    }

    /// 非法 task_id 一律拒绝：空串是笔误（不是「请用默认槽位」），路径分隔符尤其危险。
    #[test]
    fn handoff_rejects_illegal_task_id() {
        for bad in ["", "   ", "a/b", "a b", "a#b", "../etc", "a\\b"] {
            assert!(
                handoff(&base().with_task(bad)).is_err(),
                "task_id {bad:?} 应被拒绝"
            );
        }
        let too_long = "a".repeat(HANDOFF_TASK_ID_MAX_CHARS + 1);
        assert!(handoff(&base().with_task(too_long)).is_err());
        // 边界：恰好 128 字符应通过
        let at_limit = "a".repeat(HANDOFF_TASK_ID_MAX_CHARS);
        assert!(handoff(&base().with_task(at_limit)).is_ok());
    }

    /// goal / state 必填：空交接没有意义，必须当场拒绝而不是存一条空记录。
    #[test]
    fn handoff_requires_goal_and_state() {
        assert!(handoff(&HandoffInput::new("", "有进展")).is_err());
        assert!(handoff(&HandoffInput::new("有目标", "   ")).is_err());
    }

    /// 各层容量上限：单字段、单元素、条数、整条。边界值本身必须通过。
    #[test]
    fn handoff_enforces_layer_limits_with_inclusive_boundaries() {
        // 单字段：恰好 16 KiB 通过，多 1 字节拒绝
        let ok_text = "a".repeat(HANDOFF_TEXT_MAX_BYTES);
        assert!(handoff(&HandoffInput::new(ok_text.clone(), "s")).is_ok());
        let over_text = "a".repeat(HANDOFF_TEXT_MAX_BYTES + 1);
        assert!(handoff(&HandoffInput::new(over_text, "s")).is_err());

        // 数组条数：恰好 64 通过，65 拒绝
        let max_items = vec!["step".to_string(); HANDOFF_ITEMS_MAX];
        assert!(handoff(&base().with_next_steps(max_items)).is_ok());
        let too_many = vec!["step".to_string(); HANDOFF_ITEMS_MAX + 1];
        assert!(handoff(&base().with_next_steps(too_many)).is_err());

        // 单元素：恰好 4 KiB 通过，多 1 字节拒绝
        let ok_item = vec!["b".repeat(HANDOFF_ITEM_MAX_BYTES)];
        assert!(handoff(&base().with_files(ok_item)).is_ok());
        let over_item = vec!["b".repeat(HANDOFF_ITEM_MAX_BYTES + 1)];
        assert!(handoff(&base().with_files(over_item)).is_err());
    }

    /// 整条上限：每个字段都合法，加起来超限也要拒绝（否则「用错工具」无人制止）。
    #[test]
    fn handoff_enforces_total_size_across_fields() {
        let bulk = || -> Vec<String> { vec!["c".repeat(256); HANDOFF_ITEMS_MAX] };
        let input = HandoffInput::new("g".repeat(8 * 1024), "s".repeat(8 * 1024))
            .with_next_steps(bulk())
            .with_files(bulk())
            .with_decisions(bulk())
            .with_open_questions(bulk());
        let err = handoff(&input).unwrap_err();
        assert!(
            matches!(&err, StorageError::InvalidInput(m) if m.contains("over the")),
            "应因整条超限而拒绝，实际：{err:?}"
        );
    }

    /// 空元素是笔误，报错而不是静默丢弃。
    #[test]
    fn handoff_rejects_blank_items() {
        assert!(handoff(&base().with_next_steps(vec!["ok".into(), "  ".into()])).is_err());
    }

    /// 数组保序且逐项去空白——接手方看到的顺序必须与写入方一致。
    #[test]
    fn handoff_preserves_item_order_and_trims_each() {
        let n =
            handoff(&base().with_next_steps(vec![" 第二步 ".to_string(), "第一步".to_string()]))
                .unwrap();
        assert_eq!(
            n.next_steps,
            vec!["第二步".to_string(), "第一步".to_string()]
        );
    }

    /// 标识字段有界（它们不是正文，但不该无界）。
    #[test]
    fn handoff_bounds_ident_fields() {
        let long = "c".repeat(HANDOFF_IDENT_MAX_BYTES + 1);
        assert!(handoff(&base().with_client(long)).is_err());
        let long = "s".repeat(HANDOFF_IDENT_MAX_BYTES + 1);
        assert!(handoff(&base().with_session(long)).is_err());
    }

    /// 摘要取首行并**可见地**标记截断。
    #[test]
    fn state_excerpt_takes_first_line_and_marks_truncation() {
        assert_eq!(state_excerpt("第一行\n第二行"), "第一行");
        assert_eq!(state_excerpt("  空白包围  "), "空白包围");

        let exact = "x".repeat(HANDOFF_EXCERPT_CHARS);
        assert_eq!(state_excerpt(&exact), exact, "恰好达上限不应出现省略号");

        let over = state_excerpt(&"x".repeat(HANDOFF_EXCERPT_CHARS + 1));
        assert!(over.ends_with('…'), "超长必须标记截断：{over}");
        assert_eq!(over.chars().count(), HANDOFF_EXCERPT_CHARS);
    }
}
