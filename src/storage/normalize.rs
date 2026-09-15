//! 入参规范化与校验（各后端共用）。
//!
//! @author yujinping
//! @intent P2：契约要求「整批非法即整批拒绝」，因此校验必须在任何写入之前一次性完成。
//!          规则由两个后端共用，避免内存后端与 SQLite 后端的语义漂移
//!          （行为等价是「换后端不改业务代码」的前提）。

use crate::domain::{EntityInput, ObservationInput, RelationInput};
use crate::error::StorageError;

/// 规范化单个实体：名称去空白后不得为空；类型为空则落为 `unknown`。
pub fn entity(input: &EntityInput) -> Result<(String, String), StorageError> {
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
    Ok((name.to_string(), entity_type))
}

/// 规范化实体批次。
pub fn entities(inputs: &[EntityInput]) -> Result<Vec<(String, String)>, StorageError> {
    inputs.iter().map(entity).collect()
}

/// 规范化关系批次：两端与关系类型均不得为空。
pub fn relations(inputs: &[RelationInput]) -> Result<Vec<RelationKey>, StorageError> {
    inputs
        .iter()
        .map(|r| {
            let (from_name, to_name, relation_type) = (
                r.from_name.trim(),
                r.to_name.trim(),
                r.relation_type.trim(),
            );
            if from_name.is_empty() || to_name.is_empty() || relation_type.is_empty() {
                return Err(StorageError::InvalidInput(
                    "relation endpoints and type must be non-empty".to_string(),
                ));
            }
            Ok((
                from_name.to_string(),
                to_name.to_string(),
                relation_type.to_string(),
            ))
        })
        .collect()
}

/// 规范化观测批次：实体名与每条内容均不得为空。
pub fn observations(
    inputs: &[ObservationInput],
) -> Result<Vec<(String, Vec<String>)>, StorageError> {
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
            ))
        })
        .collect()
}

/// 关系三元组：(起点, 终点, 关系类型)。
pub type RelationKey = (String, String, String);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_trims_and_defaults_type() {
        assert_eq!(
            entity(&EntityInput::new("  alice  ", " person ")).unwrap(),
            ("alice".to_string(), "person".to_string())
        );
        assert_eq!(
            entity(&EntityInput::new("alice", "   ")).unwrap(),
            ("alice".to_string(), "unknown".to_string())
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
            vec![("a".to_string(), "b".to_string(), "knows".to_string())]
        );
        assert!(relations(&[RelationInput::new("a", "", "knows")]).is_err());
        assert!(relations(&[RelationInput::new("a", "b", "  ")]).is_err());
    }

    #[test]
    fn observations_reject_blank_content() {
        assert_eq!(
            observations(&[ObservationInput::new("a", vec![" x ".to_string()])]).unwrap(),
            vec![("a".to_string(), vec!["x".to_string()])]
        );
        assert!(observations(&[ObservationInput::single("a", "   ")]).is_err());
        assert!(observations(&[ObservationInput::new(
            "a",
            vec!["ok".to_string(), "".to_string()]
        )])
        .is_err());
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

        let (canonical, _) = entity(&EntityInput::new("  a  ", "")).unwrap();
        assert_eq!(
            name_list(&["  a  ".to_string()]),
            vec![canonical],
            "同一输入经写路径与读路径必须得到同一个名字"
        );
    }
}
