//! 记忆仓库共享契约测试集（仅测试编译）。
//!
//! @author yujinping
//! @intent P2 的核心价值在于「换后端不改业务代码」，因此后端之间必须行为等价。
//!          本模块把 9 个方法的语义固化为一份可复用的契约，`InMemBackend` 与
//!          `SqliteFileBackend` 各自跑一遍，任何一方行为漂移都会在测试中暴露。
//!          新增后端（P6 的单库 / Postgres）只需接入同一份契约即可获得同等保证。

use crate::domain::{EntityInput, ObservationInput, RelationInput};
use crate::error::StorageError;
use crate::storage::repo::MemoryRepository;
use crate::storage::StorageBackend;

/// 执行全部契约用例；任一断言失败即 panic（由调用方测试报告）。
///
/// @intent 用例按阶段顺序共享同一仓库实例，因此全局计数断言只出现在前置阶段，
///         其余阶段一律按名 / 按 id 精确断言，避免相互耦合。
pub async fn run_all(repo: &dyn MemoryRepository) {
    phase_01_fresh_repo_is_empty(repo).await;
    phase_02_create_entities_is_idempotent(repo).await;
    phase_03_invalid_inputs_are_rejected_atomically(repo).await;
    phase_04_add_observations_appends(repo).await;
    phase_05_add_observations_requires_entity(repo).await;
    phase_06_create_relations_dedupes(repo).await;
    phase_07_search_matches_name_content_and_type(repo).await;
    phase_08_open_nodes_filters_and_orders(repo).await;
    phase_09_delete_observations_syncs_search_index(repo).await;
    phase_10_delete_relations_by_id(repo).await;
    phase_11_delete_entities_cascades(repo).await;
    phase_12_delete_is_idempotent(repo).await;
    phase_13_sources_persist_and_round_trip(repo).await;
}

/// 执行后端级契约用例：项目生命周期（取用幂等 / 项目隔离 / 丢弃项目）。
///
/// @intent `run_all` 覆盖的是「同一项目内的记忆语义」，而 P4 的管理面依赖另一层语义——
///         「项目本身」的创建与销毁。丢弃项目必然涉及后端私有资源（连接缓存、文件目录），
///         故必须像记忆语义一样用同一份契约钉住两个后端，否则「换后端不改业务代码」
///         在 `DELETE /projects` 上会失效。
pub async fn run_backend_contract(backend: &dyn StorageBackend) {
    bc_01_repositories_for_reuses_state(backend).await;
    bc_02_projects_are_isolated(backend).await;
    bc_03_drop_project_removes_only_that_project(backend).await;
    bc_04_drop_project_is_idempotent(backend).await;
    bc_05_drop_project_rejects_unsafe_project_id(backend).await;
}

// ---------------------------------------------------------------- 后端级用例

/// 同一 project_id 的两次取用必须指向同一份数据——否则「写入后下一个请求读不到」。
async fn bc_01_repositories_for_reuses_state(backend: &dyn StorageBackend) {
    let first = backend.repositories_for("bc1").await.unwrap();
    first
        .memory
        .create_entities(vec![EntityInput::new("bc1-keep", "concept")])
        .await
        .unwrap();

    let second = backend.repositories_for("bc1").await.unwrap();
    assert_eq!(second.project_id, "bc1");
    assert_eq!(
        second.memory.read_graph().await.unwrap().entity_count(),
        1,
        "同一项目的两次取用必须复用状态"
    );
}

/// 项目之间必须硬隔离（文件后端的物理隔离 / 内存后端的 map 分片）。
async fn bc_02_projects_are_isolated(backend: &dyn StorageBackend) {
    let a = backend.repositories_for("bc2-a").await.unwrap();
    let b = backend.repositories_for("bc2-b").await.unwrap();

    a.memory
        .create_entities(vec![EntityInput::new("bc2-only-a", "person")])
        .await
        .unwrap();

    assert_eq!(a.memory.read_graph().await.unwrap().entity_count(), 1);
    assert_eq!(
        b.memory.read_graph().await.unwrap().entity_count(),
        0,
        "另一个项目不得看到本项目的记忆"
    );
}

/// 丢弃项目后：该项目归零，其他项目不受影响。
///
/// @intent 刻意**不**断言「已持有的旧句柄立刻读到空」——旧句柄可能仍指向被 unlink 的
///         旧文件（文件后端）或已脱离索引的内存实例。契约只承诺「此后重新取用得到
///         全新空项目」，这才是管理面与后续请求实际依赖的语义。
async fn bc_03_drop_project_removes_only_that_project(backend: &dyn StorageBackend) {
    let keep = backend.repositories_for("bc3-keep").await.unwrap();
    keep.memory
        .create_entities(vec![EntityInput::new("bc3-kept", "person")])
        .await
        .unwrap();

    let doomed = backend.repositories_for("bc3-doomed").await.unwrap();
    doomed
        .memory
        .create_entities(vec![EntityInput::new("bc3-doomed-entity", "person")])
        .await
        .unwrap();
    drop(doomed);

    backend.drop_project("bc3-doomed").await.unwrap();

    let fresh = backend.repositories_for("bc3-doomed").await.unwrap();
    assert_eq!(
        fresh.memory.read_graph().await.unwrap().entity_count(),
        0,
        "丢弃后重新取用必须是全新空项目"
    );
    assert_eq!(
        keep.memory.read_graph().await.unwrap().entity_count(),
        1,
        "其他项目不得被牵连"
    );
}

/// 丢弃从未落盘的项目（或重复丢弃）必须成功，不得报错。
async fn bc_04_drop_project_is_idempotent(backend: &dyn StorageBackend) {
    backend.drop_project("bc4-never-used").await.unwrap();
    backend.drop_project("bc4-never-used").await.unwrap();
}

/// 非法 project_id 一律拒绝：丢弃操作直接触碰文件系统，是目录穿越的高危入口。
async fn bc_05_drop_project_rejects_unsafe_project_id(backend: &dyn StorageBackend) {
    for bad in ["../evil", "a/b", "", ".."] {
        assert_eq!(
            backend.drop_project(bad).await.err(),
            Some(StorageError::InvalidProjectId(bad.to_string())),
            "非法 project_id {bad:?} 必须被拒绝"
        );
    }
}

// ---------------------------------------------------------------- 用例实现

async fn phase_01_fresh_repo_is_empty(repo: &dyn MemoryRepository) {
    let g = graph(repo).await;
    assert_eq!(g.entity_count(), 0, "新建仓库不应有实体");
    assert_eq!(g.relation_count(), 0, "新建仓库不应有关系");
    assert_eq!(g.observation_count(), 0, "新建仓库不应有观测");

    assert!(
        search(repo, "anything").await.is_empty(),
        "空仓库检索应无结果"
    );
    assert!(
        repo.open_nodes(&[]).await.unwrap().is_empty(),
        "空名字列表应返回空集"
    );
}

async fn phase_02_create_entities_is_idempotent(repo: &dyn MemoryRepository) {
    repo.create_entities(vec![
        EntityInput::new("c2-alice", "person"),
        EntityInput::new("c2-bob", "person"),
        EntityInput::new("c2-acme", "org"),
    ])
    .await
    .unwrap();

    // 重复创建同名实体：忽略且不覆盖首次登记的 entity_type
    repo.create_entities(vec![EntityInput::new("c2-alice", "robot")])
        .await
        .unwrap();

    let g = graph(repo).await;
    assert_eq!(
        names(&g.entities),
        vec!["c2-acme", "c2-alice", "c2-bob"],
        "实体应按 name 升序返回"
    );
    assert_eq!(g.observation_count(), 0, "新建实体不应带观测");
    assert_eq!(g.relation_count(), 0, "本阶段不应有关系");

    let alice = open(repo, "c2-alice").await;
    assert_eq!(alice.entity_type, "person", "重复创建不得覆盖原类型");
    assert!(alice.created_at > 0, "created_at 应为正数时间戳");
}

async fn phase_03_invalid_inputs_are_rejected_atomically(repo: &dyn MemoryRepository) {
    // 空 / 纯空白实体名
    assert_invalid_input(repo.create_entities(vec![EntityInput::new("", "person")]).await);
    assert_invalid_input(repo.create_entities(vec![EntityInput::new("   ", "person")]).await);

    // 非法批次不得部分落库
    let err = repo
        .create_entities(vec![
            EntityInput::new("c3-valid", "person"),
            EntityInput::new("", "person"),
        ])
        .await;
    assert_invalid_input(err);
    assert!(
        repo.open_nodes(&["c3-valid".to_string()])
            .await
            .unwrap()
            .is_empty(),
        "整批非法时不得写入任何实体（需事务 / 先校验后写入）"
    );

    // 空观测内容、空实体名
    assert_invalid_input(
        repo.add_observations(vec![ObservationInput::single("c2-alice", "")])
            .await,
    );
    assert_invalid_input(
        repo.add_observations(vec![ObservationInput::single("   ", "x")])
            .await,
    );

    // 合法 + 非法混合批次：整批拒绝，不得部分落库
    assert_invalid_input(
        repo.add_observations(vec![ObservationInput::new(
            "c2-alice",
            vec!["c3-ok".to_string(), String::new()],
        )])
        .await,
    );
    assert!(
        contents(&open(repo, "c2-alice").await).is_empty(),
        "整批非法时不得写入任何观测"
    );
}

async fn phase_04_add_observations_appends(repo: &dyn MemoryRepository) {
    repo.add_observations(vec![
        ObservationInput::new(
            "c2-alice",
            vec!["works at Acme".to_string(), "偏好使用深色主题".to_string()],
        ),
        ObservationInput::single("c2-acme", "AI 记忆库服务"),
    ])
    .await
    .unwrap();

    let alice = open(repo, "c2-alice").await;
    assert_eq!(
        contents(&alice),
        vec!["works at Acme", "偏好使用深色主题"],
        "观测应按写入顺序、按 id 升序返回"
    );
    assert!(alice.observations[0].id < alice.observations[1].id, "观测 id 应递增");
    assert!(
        alice.observations.iter().all(|o| o.entity_name == "c2-alice"),
        "观测应归属正确实体"
    );
    assert!(alice.observations.iter().all(|o| o.created_at > 0));

    let acme = open(repo, "c2-acme").await;
    assert_eq!(contents(&acme), vec!["AI 记忆库服务"]);

    assert_eq!(
        graph(repo).await.observation_count(),
        3,
        "本阶段结束应共有 3 条观测"
    );
}

async fn phase_05_add_observations_requires_entity(repo: &dyn MemoryRepository) {
    let err = repo
        .add_observations(vec![ObservationInput::single("c5-ghost", "orphan")])
        .await
        .unwrap_err();
    assert_eq!(
        err,
        StorageError::EntityNotFound("c5-ghost".to_string()),
        "为不存在的实体追加观测应报 EntityNotFound"
    );
}

async fn phase_06_create_relations_dedupes(repo: &dyn MemoryRepository) {
    repo.create_relations(vec![
        RelationInput::new("c2-alice", "c2-acme", "works_at"),
        RelationInput::new("c2-bob", "c2-alice", "knows"),
    ])
    .await
    .unwrap();

    // 完全相同的三元组重复提交应被跳过
    repo.create_relations(vec![RelationInput::new("c2-alice", "c2-acme", "works_at")])
        .await
        .unwrap();

    let g = graph(repo).await;
    assert_eq!(g.relation_count(), 2, "重复关系应被跳过");
    assert!(
        g.relations[0].id < g.relations[1].id,
        "关系应按 id 升序返回"
    );
    assert_eq!(
        count_relations(&g, "c2-alice", "c2-acme", "works_at"),
        1,
        "相同三元组在库中只应存在一条"
    );

    // 端点未登记为实体时允许创建（对齐官方 MCP Memory 语义，不做存在性校验）
    repo.create_relations(vec![RelationInput::new("c6-x", "c6-ghost", "mentions")])
        .await
        .unwrap();
    assert_eq!(
        graph(repo).await.relation_count(),
        3,
        "端点未登记的关系应被接受"
    );
}

async fn phase_07_search_matches_name_content_and_type(repo: &dyn MemoryRepository) {
    assert_eq!(
        sorted_names(&search(repo, "acme").await),
        vec!["c2-acme", "c2-alice"],
        "应同时命中实体名与其观测内容"
    );
    assert_eq!(
        sorted_names(&search(repo, "person").await),
        vec!["c2-alice", "c2-bob"],
        "应命中实体类型"
    );
    assert_eq!(
        sorted_names(&search(repo, "org").await),
        vec!["c2-acme"],
        "实体类型应可被检索"
    );
    assert!(
        names(&search(repo, "works at").await).contains(&"c2-alice".to_string()),
        "多词查询按 AND 语义命中"
    );

    // 中文：不经分词也须可检索（SQLite 后端由子串回退保证）
    assert_eq!(
        sorted_names(&search(repo, "记忆库服务").await),
        vec!["c2-acme"]
    );
    assert_eq!(sorted_names(&search(repo, "深色").await), vec!["c2-alice"]);

    // 检索命中应返回该实体的完整明细（含全部观测，而非仅匹配的那条）
    let hit = search(repo, "深色").await;
    assert_eq!(names(&hit), vec!["c2-alice"]);
    assert_eq!(
        contents(&hit[0]),
        vec!["works at Acme", "偏好使用深色主题"],
        "命中实体应带出其全部观测"
    );

    // 无命中与非法查询一律返回空集且不报错
    for q in ["zzz-no-hit", "", "   ", "%", "\"", "*", "c2-nope"] {
        assert!(
            search(repo, q).await.is_empty(),
            "查询 {q:?} 应返回空集而非报错"
        );
    }
}

async fn phase_08_open_nodes_filters_and_orders(repo: &dyn MemoryRepository) {
    let got = repo
        .open_nodes(&["c2-bob".to_string(), "c2-alice".to_string()])
        .await
        .unwrap();
    assert_eq!(
        names(&got),
        vec!["c2-alice", "c2-bob"],
        "open_nodes 应按 name 升序返回"
    );

    assert!(
        repo.open_nodes(&["c8-ghost".to_string()]).await.unwrap().is_empty(),
        "未命中的名字应被跳过"
    );

    let mixed = repo
        .open_nodes(&["c2-alice".to_string(), "c8-ghost".to_string()])
        .await
        .unwrap();
    assert_eq!(names(&mixed), vec!["c2-alice"]);
}

async fn phase_09_delete_observations_syncs_search_index(repo: &dyn MemoryRepository) {
    let alice = open(repo, "c2-alice").await;
    let target = alice
        .observations
        .iter()
        .find(|o| o.content == "works at Acme")
        .expect("前置阶段应已写入该观测")
        .id;

    assert!(
        names(&search(repo, "acme").await).contains(&"c2-alice".to_string()),
        "删除前应可检索到"
    );

    repo.delete_observations(&[target]).await.unwrap();

    assert_eq!(
        sorted_names(&search(repo, "acme").await),
        vec!["c2-acme"],
        "删除观测后检索索引必须同步清理"
    );
    assert_eq!(
        contents(&open(repo, "c2-alice").await),
        vec!["偏好使用深色主题"],
        "同一实体的其余观测应保留"
    );
    assert_eq!(graph(repo).await.observation_count(), 2);

    // 不存在的 id：静默忽略，保证幂等
    repo.delete_observations(&[999_999]).await.unwrap();
}

async fn phase_10_delete_relations_by_id(repo: &dyn MemoryRepository) {
    let g = graph(repo).await;
    let target = g
        .relations
        .iter()
        .find(|r| r.from_name == "c2-alice" && r.relation_type == "works_at")
        .expect("前置阶段应已写入该关系")
        .id;

    repo.delete_relations(&[target]).await.unwrap();

    let g = graph(repo).await;
    assert_eq!(
        count_relations(&g, "c2-alice", "c2-acme", "works_at"),
        0,
        "被删除的关系不应残留"
    );
    assert_eq!(count_relations(&g, "c2-bob", "c2-alice", "knows"), 1);

    repo.delete_relations(&[999_999]).await.unwrap();
}

async fn phase_11_delete_entities_cascades(repo: &dyn MemoryRepository) {
    repo.delete_entities(&["c2-acme".to_string()]).await.unwrap();

    assert!(
        repo.open_nodes(&["c2-acme".to_string()]).await.unwrap().is_empty(),
        "实体应被删除"
    );
    assert!(
        search(repo, "记忆库服务").await.is_empty(),
        "实体级联删除应清掉其观测与检索索引"
    );

    let g = graph(repo).await;
    assert!(
        g.relations
            .iter()
            .all(|r| r.from_name != "c2-acme" && r.to_name != "c2-acme"),
        "实体级联删除应清掉其两端的关系"
    );
    assert_eq!(
        names(&g.entities),
        vec!["c2-alice", "c2-bob"],
        "其他实体不受影响"
    );
    assert_eq!(contents(&open(repo, "c2-alice").await), vec!["偏好使用深色主题"]);
    assert_eq!(
        sorted_names(&search(repo, "深色").await),
        vec!["c2-alice"],
        "其余实体的检索能力不受影响"
    );
}

async fn phase_12_delete_is_idempotent(repo: &dyn MemoryRepository) {
    repo.delete_entities(&["c12-ghost".to_string()]).await.unwrap();

    repo.delete_entities(&["c2-bob".to_string()]).await.unwrap();
    let g = graph(repo).await;
    assert_eq!(names(&g.entities), vec!["c2-alice"]);
    assert!(
        g.relations
            .iter()
            .all(|r| r.from_name != "c2-bob" && r.to_name != "c2-bob"),
        "删除实体应级联清掉其两端关系，不得只删一半"
    );
}

/// `source`（写入来源标识）必须持久化并随读路径完整返回。
///
/// @intent 支撑「事后按客户端审计」：写侧三个工具均可携带来源，读侧三个模型必须原样带回；
///         未携带来源的写入落为空串，不得默认成别的值。
async fn phase_13_sources_persist_and_round_trip(repo: &dyn MemoryRepository) {
    repo.create_entities(vec![EntityInput::new("c13-src", "note").with_source("workbuddy")])
        .await
        .unwrap();
    // 同名重复创建不得覆盖首次登记的来源（与 entity_type 同规则）
    repo.create_entities(vec![EntityInput::new("c13-src", "note").with_source("cursor")])
        .await
        .unwrap();

    repo.add_observations(vec![
        ObservationInput::new("c13-src", vec!["来自 claude-code 的事实".to_string()])
            .with_source("claude-code"),
        ObservationInput::new("c13-src", vec!["未声明来源的事实".to_string()]),
    ])
    .await
    .unwrap();

    repo.create_relations(vec![
        RelationInput::new("c13-src", "c2-alice", "mentions").with_source("cursor"),
        RelationInput::new("c13-src", "c2-alice", "refs"),
    ])
    .await
    .unwrap();

    let e = open(repo, "c13-src").await;
    assert_eq!(e.source, "workbuddy", "实体来源应持久化且首次登记优先");
    let by_content = |needle: &str| {
        e.observations
            .iter()
            .find(|o| o.content.contains(needle))
            .expect("前置写入的观测应存在")
            .source
            .clone()
    };
    assert_eq!(by_content("claude-code"), "claude-code", "观测来源应持久化");
    assert_eq!(by_content("未声明来源"), "", "未携带来源的观测应落为空串");

    let g = graph(repo).await;
    let rel = |rtype: &str| {
        g.relations
            .iter()
            .find(|r| r.from_name == "c13-src" && r.relation_type == rtype)
            .expect("前置写入的关系应存在")
            .source
            .clone()
    };
    assert_eq!(rel("mentions"), "cursor", "关系来源应持久化");
    assert_eq!(rel("refs"), "", "未携带来源的关系应落为空串");

    // 来源字段不得影响既有语义：重复三元组仍按三元组去重（来源不参与判重）
    repo.create_relations(vec![RelationInput::new("c13-src", "c2-alice", "refs").with_source("x")])
        .await
        .unwrap();
    assert_eq!(
        graph(repo)
            .await
            .relations
            .iter()
            .filter(|r| r.from_name == "c13-src" && r.relation_type == "refs")
            .count(),
        1,
        "重复三元组即使来源不同也应被跳过"
    );
}

// ---------------------------------------------------------------- 断言辅助

/// 断言结果为 `InvalidInput`。
fn assert_invalid_input<T: std::fmt::Debug>(result: Result<T, StorageError>) {
    match result {
        Err(StorageError::InvalidInput(_)) => {}
        other => panic!("应报 InvalidInput，实际为 {other:?}"),
    }
}

async fn graph(repo: &dyn MemoryRepository) -> crate::domain::Graph {
    repo.read_graph().await.unwrap()
}

async fn search(repo: &dyn MemoryRepository, q: &str) -> Vec<crate::domain::Entity> {
    repo.search_nodes(q).await.unwrap()
}

async fn open(repo: &dyn MemoryRepository, name: &str) -> crate::domain::Entity {
    let mut got = repo.open_nodes(&[name.to_string()]).await.unwrap();
    assert_eq!(got.len(), 1, "实体 {name} 应存在");
    got.remove(0)
}

fn names(entities: &[crate::domain::Entity]) -> Vec<String> {
    entities.iter().map(|e| e.name.clone()).collect()
}

fn sorted_names(entities: &[crate::domain::Entity]) -> Vec<String> {
    let mut v = names(entities);
    v.sort();
    v
}

fn contents(e: &crate::domain::Entity) -> Vec<String> {
    e.observations.iter().map(|o| o.content.clone()).collect()
}

fn count_relations(g: &crate::domain::Graph, from: &str, to: &str, rtype: &str) -> usize {
    g.relations
        .iter()
        .filter(|r| r.from_name == from && r.to_name == to && r.relation_type == rtype)
        .count()
}
