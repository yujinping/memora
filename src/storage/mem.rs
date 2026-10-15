//! 内存记忆后端：纯 HashMap 实现，仅供测试与临时验证。
//!
//! @author yujinping
//! @intent P2：为 Repository 契约提供一个零依赖的参照实现。SQLite 后端的行为是否
//!         正确，以「与内存后端跑同一份契约测试」为准；同时使 P3 的 MCP 工具层
//!          可以在不触碰文件系统的前提下做端到端测试。

use crate::domain::{
    AckOutcome, Entity, EntityInput, Graph, Handoff, HandoffInput, HandoffResolution,
    HandoffSummary, Observation, ObservationInput, Relation, RelationInput,
};
use crate::error::StorageError;
use crate::storage::normalize;
use crate::storage::repo::{HandoffRepository, MemoryRepository};
use crate::storage::{BackendKind, ProjectRepos, StorageBackend};
use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// 实体登记项（不含观测，观测单独存放）。
#[derive(Debug, Clone)]
struct EntityRecord {
    /// 实体类型
    entity_type: String,
    /// 创建时间（Unix 秒）
    created_at: i64,
    /// 登记来源标识（首次登记生效），空串表示未声明
    source: String,
}

/// 单项目内存状态。
#[derive(Debug, Default)]
struct Store {
    /// 实体：name → 登记项（BTreeMap 保证按 name 有序）
    entities: BTreeMap<String, EntityRecord>,
    /// 全部观测
    observations: Vec<Observation>,
    /// 全部关系
    relations: Vec<Relation>,
    /// 观测自增 id
    next_observation_id: i64,
    /// 关系自增 id
    next_relation_id: i64,
}

/// 当前 Unix 秒。
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 单项目的内存记忆仓库。
pub struct InMemMemoryRepo {
    /// 项目状态（`std::sync::Mutex`：临界区内无 await，不会跨 await 持锁）
    store: Mutex<Store>,
}

impl InMemMemoryRepo {
    /// 构造空仓库。
    fn new() -> Self {
        Self {
            store: Mutex::new(Store::default()),
        }
    }

    /// 依据名字集合取出实体明细（含观测），按 name 升序。
    fn collect(&self, store: &Store, names: &[String]) -> Vec<Entity> {
        let mut wanted: Vec<&String> = names.iter().collect();
        wanted.sort();
        wanted.dedup();

        wanted
            .into_iter()
            .filter_map(|name| {
                store.entities.get(name).map(|rec| Entity {
                    name: name.clone(),
                    entity_type: rec.entity_type.clone(),
                    created_at: rec.created_at,
                    source: rec.source.clone(),
                    observations: store
                        .observations
                        .iter()
                        .filter(|o| &o.entity_name == name)
                        .cloned()
                        .collect(),
                })
            })
            .collect()
    }
}

#[async_trait]
impl MemoryRepository for InMemMemoryRepo {
    async fn create_entities(&self, entities: Vec<EntityInput>) -> Result<(), StorageError> {
        // 先整批校验，再写入：保证非法批次不产生部分结果
        let normalized = normalize::entities(&entities)?;

        let ts = now();
        let mut store = self.store.lock().unwrap();
        for (name, entity_type, source) in normalized {
            store.entities.entry(name).or_insert(EntityRecord {
                entity_type,
                created_at: ts,
                source,
            });
        }
        Ok(())
    }

    async fn create_relations(&self, relations: Vec<RelationInput>) -> Result<(), StorageError> {
        let normalized = normalize::relations(&relations)?;
        let mut store = self.store.lock().unwrap();
        let ts = now();
        for (from, to, rtype, source) in normalized {
            let exists = store
                .relations
                .iter()
                .any(|e| e.from_name == from && e.to_name == to && e.relation_type == rtype);
            if exists {
                continue;
            }
            store.next_relation_id += 1;
            let id = store.next_relation_id;
            store.relations.push(Relation {
                id,
                from_name: from,
                to_name: to,
                relation_type: rtype,
                created_at: ts,
                source,
            });
        }
        Ok(())
    }

    async fn add_observations(
        &self,
        observations: Vec<ObservationInput>,
    ) -> Result<(), StorageError> {
        // 整批校验：先确认内容非空，再确认实体存在，最后统一写入
        let normalized = normalize::observations(&observations)?;

        let mut store = self.store.lock().unwrap();
        for (entity_name, _, _) in &normalized {
            if !store.entities.contains_key(entity_name) {
                return Err(StorageError::EntityNotFound(entity_name.clone()));
            }
        }

        let ts = now();
        for (entity_name, contents, source) in normalized {
            for content in contents {
                store.next_observation_id += 1;
                let id = store.next_observation_id;
                store.observations.push(Observation {
                    id,
                    entity_name: entity_name.clone(),
                    content,
                    created_at: ts,
                    source: source.clone(),
                });
            }
        }
        Ok(())
    }

    async fn delete_entities(&self, names: &[String]) -> Result<(), StorageError> {
        let mut store = self.store.lock().unwrap();
        let targets: Vec<String> = names.iter().map(|n| n.trim().to_string()).collect();

        store.entities.retain(|name, _| !targets.contains(name));
        store
            .observations
            .retain(|o| !targets.contains(&o.entity_name));
        store
            .relations
            .retain(|r| !targets.contains(&r.from_name) && !targets.contains(&r.to_name));
        Ok(())
    }

    async fn delete_observations(&self, ids: &[i64]) -> Result<(), StorageError> {
        let mut store = self.store.lock().unwrap();
        store.observations.retain(|o| !ids.contains(&o.id));
        Ok(())
    }

    async fn delete_relations(&self, ids: &[i64]) -> Result<(), StorageError> {
        let mut store = self.store.lock().unwrap();
        store.relations.retain(|r| !ids.contains(&r.id));
        Ok(())
    }

    async fn read_graph(&self) -> Result<Graph, StorageError> {
        let store = self.store.lock().unwrap();
        let names: Vec<String> = store.entities.keys().cloned().collect();
        let mut relations = store.relations.clone();
        relations.sort_by_key(|r| r.id);
        Ok(Graph {
            entities: self.collect(&store, &names),
            relations,
        })
    }

    async fn search_nodes(&self, query: &str) -> Result<Vec<Entity>, StorageError> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let terms: Vec<&str> = q.split_whitespace().collect();

        let store = self.store.lock().unwrap();
        let hits: Vec<String> = store
            .entities
            .iter()
            .filter(|(name, rec)| {
                let haystacks = [
                    name.to_lowercase(),
                    rec.entity_type.to_lowercase(),
                    store
                        .observations
                        .iter()
                        .filter(|o| &o.entity_name == *name)
                        .map(|o| o.content.to_lowercase())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ];
                terms.iter().all(|t| haystacks.iter().any(|h| h.contains(t)))
            })
            .map(|(name, _)| name.clone())
            .collect();

        Ok(self.collect(&store, &hits))
    }

    async fn open_nodes(&self, names: &[String]) -> Result<Vec<Entity>, StorageError> {
        let store = self.store.lock().unwrap();
        Ok(self.collect(&store, names))
    }
}

/// 单项目的内存交接仓库。
///
/// @intent `BTreeMap<task_id, Vec<Handoff>>`：按任务分桶、桶内按修订号升序追加。
///         一次 save 只追加、绝不改写既有元素——与 SQLite 后端「不可变修订」一致。
pub struct InMemHandoffRepo {
    /// 项目交接状态（`std::sync::Mutex`：临界区内无 await，不会跨 await 持锁）
    store: Mutex<BTreeMap<String, Vec<Handoff>>>,
}

impl InMemHandoffRepo {
    /// 构造空仓库。
    fn new() -> Self {
        Self {
            store: Mutex::new(BTreeMap::new()),
        }
    }

    /// 每任务最新修订的摘要（已排序、已截断）。
    ///
    /// @intent 排序与截断一律走共用的 [`normalize::order_handoff_summaries`]，
    ///         不在本文件里另写一份 `sort_by`：排序口径分叉是 H6 / H11 最先失效的地方。
    fn list_latest(
        store: &BTreeMap<String, Vec<Handoff>>,
        prefix: Option<&str>,
        unacked_only: bool,
        limit: usize,
    ) -> Vec<HandoffSummary> {
        if limit == 0 {
            return Vec::new();
        }
        let items = store
            .values()
            .filter_map(|revs| revs.last())
            .filter(|h| prefix.is_none_or(|p| h.task_id.starts_with(p)))
            .filter(|h| !unacked_only || h.is_pending())
            .map(normalize::handoff_summary)
            .collect();
        normalize::order_handoff_summaries(items, limit)
    }
}

#[async_trait]
impl HandoffRepository for InMemHandoffRepo {
    async fn save_handoff(&self, input: HandoffInput) -> Result<Handoff, StorageError> {
        // 先整批校验、再落库：非法输入不产生半条修订（docs §3.5）
        let normalized = normalize::handoff(&input)?;

        let mut store = self.store.lock().unwrap();
        let revs = store.entry(normalized.task_id.clone()).or_default();
        let saved = Handoff {
            task_id: normalized.task_id,
            revision: revs.last().map_or(1, |h| h.revision + 1),
            goal: normalized.goal,
            state: normalized.state,
            next_steps: normalized.next_steps,
            files: normalized.files,
            decisions: normalized.decisions,
            open_questions: normalized.open_questions,
            client: normalized.client,
            session_id: normalized.session_id,
            created_at: now(),
            // 新修订一律重新回到待接手：内容变了，上一次接手不算数（docs §7.2）
            acked_at: None,
            acked_by: String::new(),
        };
        revs.push(saved.clone());
        Ok(saved)
    }

    async fn resolve_handoff(
        &self,
        task_id: Option<&str>,
    ) -> Result<HandoffResolution, StorageError> {
        let store = self.store.lock().unwrap();
        match task_id {
            Some(id) => Ok(match store.get(id.trim()).and_then(|revs| revs.last()) {
                Some(h) => HandoffResolution::Found(Box::new(h.clone())),
                None => HandoffResolution::None,
            }),
            None => {
                // 与 SQLite 后端同源：候选口径来自 list，避免「列表有一条、load 说没有」
                let candidates = Self::list_latest(&store, None, true, usize::MAX);
                match candidates.len() {
                    0 => Ok(HandoffResolution::None),
                    1 => {
                        let h = store
                            .get(&candidates[0].task_id)
                            .and_then(|revs| revs.last())
                            .cloned();
                        Ok(match h {
                            Some(h) => HandoffResolution::Found(Box::new(h)),
                            // 同一把锁内不可能发生；防御性写法而非 panic
                            None => HandoffResolution::None,
                        })
                    }
                    _ => Ok(HandoffResolution::Ambiguous(candidates)),
                }
            }
        }
    }

    async fn list_handoffs(
        &self,
        prefix: Option<&str>,
        unacked_only: bool,
        limit: usize,
    ) -> Result<Vec<HandoffSummary>, StorageError> {
        let store = self.store.lock().unwrap();
        Ok(Self::list_latest(&store, prefix, unacked_only, limit))
    }

    async fn ack_handoff(
        &self,
        task_id: Option<&str>,
        client: &str,
    ) -> Result<AckOutcome, StorageError> {
        // 无 ID 的共享规则在锁外执行（它会回调 resolve_handoff，而 std Mutex 不可重入）
        let Some(id) = task_id else {
            return crate::storage::ack_resolving_none(self, client).await;
        };

        let id = id.trim().to_string();
        let mut store = self.store.lock().unwrap();
        let Some(latest) = store
            .get_mut(&id)
            .and_then(|revs| revs.last_mut())
        else {
            return Ok(AckOutcome {
                resolution: HandoffResolution::None,
                won: false,
            });
        };

        // 单把锁内完成「判断 + 写入」＝天然 CAS；已接手则为幂等空操作
        let won = latest.acked_at.is_none();
        if won {
            latest.acked_at = Some(now());
            latest.acked_by = client.trim().to_string();
        }
        Ok(AckOutcome {
            resolution: HandoffResolution::Found(Box::new(latest.clone())),
            won,
        })
    }

    async fn delete_handoff(&self, task_id: &str) -> Result<(), StorageError> {
        // 幂等：不存在视为成功
        self.store.lock().unwrap().remove(task_id.trim());
        Ok(())
    }
}

/// 单项目的内存仓库集合（记忆 + 交接共享同一个项目条目，丢弃项目时一并消失）。
struct InMemProject {
    /// 记忆仓库
    memory: Arc<InMemMemoryRepo>,
    /// 交接仓库
    handoff: Arc<InMemHandoffRepo>,
}

/// 内存后端：按项目产出 `InMemMemoryRepo`，进程内不落盘。
pub struct InMemBackend {
    /// 项目 → 仓库集合
    projects: Mutex<HashMap<String, Arc<InMemProject>>>,
}

impl InMemBackend {
    /// 构造空后端。
    pub fn new() -> Self {
        Self {
            projects: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StorageBackend for InMemBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::InMem
    }

    async fn repositories_for(&self, project_id: &str) -> Result<ProjectRepos, StorageError> {
        crate::storage::validate_project_id(project_id)?;
        let project = {
            let mut guard = self.projects.lock().unwrap();
            guard
                .entry(project_id.to_string())
                .or_insert_with(|| {
                    Arc::new(InMemProject {
                        memory: Arc::new(InMemMemoryRepo::new()),
                        handoff: Arc::new(InMemHandoffRepo::new()),
                    })
                })
                .clone()
        };
        Ok(ProjectRepos {
            project_id: project_id.to_string(),
            backend: BackendKind::InMem,
            memory: project.memory.clone(),
            handoff: project.handoff.clone(),
        })
    }

    async fn drop_project(&self, project_id: &str) -> Result<(), StorageError> {
        crate::storage::validate_project_id(project_id)?;
        // 丢弃 map 条目即完成清理：数据只存在于该实例内。已取出的 Arc 句柄仍可用，
        // 但后续 repositories_for 会构造全新的空项目（见后端级契约 bc_03）。
        self.projects.lock().unwrap().remove(project_id);
        Ok(())
    }

    fn db_path_label(&self, _project_id: &str) -> String {
        // 内存后端没有文件，登记一个明确的「无文件」说明值
        "(memory)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::contract;
    use crate::storage::{StorageBackend, StorageError};

    /// 内存后端必须通过共享契约（作为 SQLite 后端的行为基准）。
    #[tokio::test]
    async fn in_mem_satisfies_repository_contract() {
        let backend = super::InMemBackend::new();
        let repos = backend.repositories_for("contract").await.unwrap();
        contract::run_all(&*repos.memory).await;
    }

    /// 内存后端同样必须通过后端级契约（项目生命周期语义与文件后端等价）。
    #[tokio::test]
    async fn in_mem_satisfies_backend_contract() {
        let backend = super::InMemBackend::new();
        contract::run_backend_contract(&backend).await;
    }

    /// 交接仓库契约：内存后端是 SQLite 后端的行为基准，两边必须逐条一致。
    #[tokio::test]
    async fn in_mem_satisfies_handoff_contract() {
        let backend = super::InMemBackend::new();
        let repos = backend.repositories_for("handoff-contract").await.unwrap();
        contract::run_handoff_contract(&*repos.handoff).await;
    }

    /// 不同项目必须互不可见。
    #[tokio::test]
    async fn in_mem_isolates_projects() {
        let backend = super::InMemBackend::new();
        let a = backend.repositories_for("proj-a").await.unwrap();
        let b = backend.repositories_for("proj-b").await.unwrap();

        a.memory
            .create_entities(vec![crate::domain::EntityInput::new("only-a", "person")])
            .await
            .unwrap();

        assert_eq!(a.memory.read_graph().await.unwrap().entity_count(), 1);
        assert_eq!(
            b.memory.read_graph().await.unwrap().entity_count(),
            0,
            "项目之间必须硬隔离"
        );
    }

    /// 同一 project_id 重复取用应复用同一份数据（会话跨请求可见）。
    #[tokio::test]
    async fn in_mem_reuses_state_for_same_project() {
        let backend = super::InMemBackend::new();
        let first = backend.repositories_for("same").await.unwrap();
        first
            .memory
            .create_entities(vec![crate::domain::EntityInput::new("keep", "concept")])
            .await
            .unwrap();

        let second = backend.repositories_for("same").await.unwrap();
        assert_eq!(second.memory.read_graph().await.unwrap().entity_count(), 1);
    }

    /// 非法 project_id 必须被拒绝。
    #[tokio::test]
    async fn in_mem_rejects_unsafe_project_id() {
        let backend = super::InMemBackend::new();
        assert_eq!(
            backend.repositories_for("../evil").await.err(),
            Some(StorageError::InvalidProjectId("../evil".to_string()))
        );
    }
}
