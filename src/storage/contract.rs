//! 记忆仓库共享契约测试集（仅测试编译）。
//!
//! @author yujinping
//! @intent P2 的核心价值在于「换后端不改业务代码」，因此后端之间必须行为等价。
//!          本模块把 9 个方法的语义固化为一份可复用的契约，`InMemBackend` 与
//!          `SqliteFileBackend` 各自跑一遍，任何一方行为漂移都会在测试中暴露。
//!          新增后端（P6 的单库 / Postgres）只需接入同一份契约即可获得同等保证。

use crate::domain::{
    EntityInput, HandoffInput, HandoffResolution, HandoffSummary, ObservationInput, RelationInput,
};
use crate::error::StorageError;
use crate::storage::repo::{HandoffRepository, MemoryRepository};
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
    bc_06_handoffs_are_isolated_per_project(backend).await;
    bc_07_db_path_label_is_declared(backend).await;
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

/// 交接同样必须按项目硬隔离：A 项目的待接手不得出现在 B 项目的列表里。
///
/// @intent 这条断言**不能**放进 `run_handoff_contract`——仓库级契约只拿得到一个项目的
///         仓库，测不了跨项目可见性。对「一项目一文件 / 一项目一 map 条目」的后端它是
///         平凡成立，但对接下来的 `sqlite_single`（租户列隔离）才是真正有意义的断言。
async fn bc_06_handoffs_are_isolated_per_project(backend: &dyn StorageBackend) {
    let a = backend.repositories_for("bc6-a").await.unwrap();
    let b = backend.repositories_for("bc6-b").await.unwrap();

    let saved = a
        .handoff
        .save_handoff(HandoffInput::new("a 项目的活", "进行中").with_client("zed"))
        .await
        .unwrap();

    let a_list = a.handoff.list_handoffs(None, true, usize::MAX).await.unwrap();
    assert_eq!(a_list.len(), 1, "A 项目应看到自己的交接");
    assert_eq!(a_list[0].task_id, saved.task_id);

    assert!(
        b.handoff.list_handoffs(None, true, usize::MAX).await.unwrap().is_empty(),
        "另一个项目不得看到本项目的交接"
    );
    assert_eq!(
        b.handoff.resolve_handoff(None).await.unwrap(),
        HandoffResolution::None,
        "另一个项目的零 ID 解析必须是「没有待接手」"
    );
    assert_eq!(
        b.handoff.resolve_handoff(Some("current@zed")).await.unwrap(),
        HandoffResolution::None,
        "即使知道 task_id，也不得跨项目读到别人的交接"
    );
}

/// 元库登记值 `db_path` 必须由后端显式给出且非空（元库列为 NOT NULL）。
async fn bc_07_db_path_label_is_declared(backend: &dyn StorageBackend) {
    let label = backend.db_path_label("bc7");
    assert!(
        !label.trim().is_empty(),
        "db_path 登记值必须非空（元库列为 NOT NULL），实际：{label:?}"
    );
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

// ---------------------------------------------------------------- 交接契约用例

/// 执行交接仓库契约用例（每个后端各跑一遍，行为必须完全一致）。
///
/// @intent 用例共享同一仓库实例、按阶段顺序推进，故每个阶段**只动自己前缀下的任务**
///         或明确断言全局状态，避免相互耦合（与 `run_all` 同一风格）。
///         三条最该被盯住的规则：`resolve(None)` 的 0/1/≥2 分支（H4/H5/H6/H15）、
///         多写入方互不遮蔽（H8）、列表截断必须发生在排序之后（H11）。
pub async fn run_handoff_contract(repo: &dyn HandoffRepository) {
    h01_empty_repo_resolves_to_none(repo).await;
    h02_first_save_lands_in_sharded_slot(repo).await;
    h03_same_writer_appends_monotonic_revisions(repo).await;
    h04_untargeted_resolve_finds_the_only_pending(repo).await;
    h05_acked_is_no_longer_pending(repo).await;
    h06_untargeted_resolve_is_ambiguous_for_two_tasks(repo).await;
    h07_explicit_id_is_precise(repo).await;
    h08_writers_do_not_shadow_each_other(repo).await;
    h09_list_semantics(repo).await;
    h10_list_summary_is_recognizable(repo).await;
    h11_list_truncates_after_ordering(repo).await;
    h12_list_prefix_filters_without_changing_order(repo).await;
    h13_ack_wins_once(repo).await;
    h14_ack_is_idempotent(repo).await;
    h15_ack_without_id_never_guesses(repo).await;
    h16_save_resets_ack_state(repo).await;
    h17_delete_is_idempotent(repo).await;
    h18_invalid_input_is_atomic(repo).await;
}

/// H1：空仓库上「查不到」是**正常返回**，不是错误。
async fn h01_empty_repo_resolves_to_none(repo: &dyn HandoffRepository) {
    assert_eq!(
        repo.resolve_handoff(Some("nope")).await.unwrap(),
        HandoffResolution::None,
        "显式 ID 不存在应返回 None（而不是报错）"
    );
    assert_eq!(
        repo.resolve_handoff(None).await.unwrap(),
        HandoffResolution::None,
        "空仓库的无 ID 解析应返回 None"
    );
    assert!(pending(repo).await.is_empty());
}

/// H2：无 ID 的 save 落到按写入方分片的槽位，六个正文字段完整回读。
async fn h02_first_save_lands_in_sharded_slot(repo: &dyn HandoffRepository) {
    let saved = repo
        .save_handoff(
            HandoffInput::new("  把 handoff 并入 memora  ", "  设计完成  ")
                .with_client("zed")
                .with_session("A1")
                .with_next_steps(vec!["实现 repo".into(), "接 MCP".into()])
                .with_files(vec!["src/storage/repo.rs".into()])
                .with_decisions(vec!["不复用记忆图谱".into()])
                .with_open_questions(vec!["是否同期做 P5".into()]),
        )
        .await
        .unwrap();

    assert_eq!(saved.task_id, "current@zed", "无 ID 应落分片槽位");
    assert_eq!(saved.revision, 1, "首个修订号必须是 1");
    assert!(saved.is_pending(), "新修订必为待接手");
    assert_eq!(saved.goal, "把 handoff 并入 memora");
    assert_eq!(saved.state, "设计完成");
    assert_eq!(
        saved.next_steps,
        vec!["实现 repo".to_string(), "接 MCP".to_string()],
        "数组必须保序"
    );
    assert_eq!(saved.files, vec!["src/storage/repo.rs".to_string()]);
    assert_eq!(saved.decisions, vec!["不复用记忆图谱".to_string()]);
    assert_eq!(saved.open_questions, vec!["是否同期做 P5".to_string()]);
    assert_eq!(saved.session_id, "A1");
    assert!(saved.created_at > 0);

    // 空数组回读必须仍是空数组（而不是 NULL / 缺字段）
    let plain = repo
        .save_handoff(HandoffInput::new("空数组", "无").with_task("empty-arrays"))
        .await
        .unwrap();
    assert!(plain.next_steps.is_empty());
    assert!(plain.files.is_empty());
    assert!(plain.decisions.is_empty());
    assert!(plain.open_questions.is_empty());
    assert_eq!(plain.client, "", "未声明 client 应落空串");
    assert_eq!(plain.session_id, "", "未声明 session 应落空串");

    // 夹具收尾：empty-arrays 只是「空数组回读」的验证品，必须显式接手，
    // 否则它会一直留在待接手集合里，破坏 H4「唯一待接手」与 H5「ack(None)
    // 命中 current@zed」的前提（各 H 阶段共享同一 repo，状态会向后泄漏）。
    let acked = repo.ack_handoff(Some("empty-arrays"), "contract").await.unwrap();
    assert!(acked.won, "夹具接手应当抢到");
}

/// H3：同一写入方连续 save = 追加修订，修订号单调，且每个任务在列表里只出现一次。
async fn h03_same_writer_appends_monotonic_revisions(repo: &dyn HandoffRepository) {
    let second = repo
        .save_handoff(HandoffInput::new("把 handoff 并入 memora", "实现中").with_client("zed"))
        .await
        .unwrap();
    assert_eq!(second.task_id, "current@zed");
    assert_eq!(second.revision, 2, "同一槽位的第二次 save 应为修订 2");

    let third = repo
        .save_handoff(HandoffInput::new("把 handoff 并入 memora", "实现中 v3").with_client("zed"))
        .await
        .unwrap();
    assert_eq!(third.revision, 3);

    match repo.resolve_handoff(Some("current@zed")).await.unwrap() {
        HandoffResolution::Found(h) => {
            assert_eq!(h.revision, 3, "显式 ID 应取该任务的最新修订");
            assert_eq!(h.state, "实现中 v3");
        }
        other => panic!("应命中 current@zed，实际 {other:?}"),
    }

    // 三次 save 只产生一个任务：列表里不得出现三个同名条目
    let listed = repo.list_handoffs(None, false, usize::MAX).await.unwrap();
    assert_eq!(
        listed.iter().filter(|s| s.task_id == "current@zed").count(),
        1,
        "每任务在列表里只能出现一次（只给最新修订）"
    );
    assert_eq!(
        listed
            .iter()
            .find(|s| s.task_id == "current@zed")
            .unwrap()
            .revision,
        3
    );
    // 注：「修订 1/2 仍然物理存在」不通过 trait 暴露（没有「读历史修订」的接口），
    //     因此那条断言落在 sqlite_file.rs 的后端专属用例里直接查表。
}

/// H4：唯一一条待接手时，零 ID 也能接手（`Found`）。
async fn h04_untargeted_resolve_finds_the_only_pending(repo: &dyn HandoffRepository) {
    let found = expect_found(repo.resolve_handoff(None).await.unwrap());
    assert_eq!(found.task_id, "current@zed", "零 ID 应命中唯一的待接手");
    assert_eq!(found.revision, 3);
}

/// H5：ack 之后，该任务不再出现在无 ID 解析里（但它仍可被显式 ID 读到）。
async fn h05_acked_is_no_longer_pending(repo: &dyn HandoffRepository) {
    let outcome = repo.ack_handoff(None, "workbuddy").await.unwrap();
    assert!(outcome.won, "首次接手应当抢到");
    let acked = expect_found(outcome.resolution);
    assert_eq!(acked.task_id, "current@zed");
    assert_eq!(acked.acked_by, "workbuddy");
    assert!(acked.acked_at.is_some());

    assert_eq!(
        repo.resolve_handoff(None).await.unwrap(),
        HandoffResolution::None,
        "已接手的任务不再是零 ID 候选"
    );
    assert_eq!(
        expect_found(repo.resolve_handoff(Some("current@zed")).await.unwrap()).revision,
        3,
        "显式 ID 仍可读到已接手的任务（接手状态是可见数据，不是删除）"
    );
}

/// H6：两条同时待接手 → `Ambiguous`，绝不替调用方挑；排序规则必须成立。
async fn h06_untargeted_resolve_is_ambiguous_for_two_tasks(repo: &dyn HandoffRepository) {
    repo.save_handoff(HandoffInput::new("alpha 的活", "起步").with_task("alpha"))
        .await
        .unwrap();
    repo.save_handoff(HandoffInput::new("beta 的活", "起步").with_task("beta"))
        .await
        .unwrap();

    match repo.resolve_handoff(None).await.unwrap() {
        HandoffResolution::Ambiguous(candidates) => {
            let mut got = ids(&candidates);
            got.sort();
            assert_eq!(
                got,
                vec!["alpha".to_string(), "beta".to_string()],
                "候选应恰好是这两个待接手任务"
            );
            assert_ordering(&candidates);
        }
        other => panic!("两条待接手时必须返回 Ambiguous，实际 {other:?}"),
    }
}

/// H7：显式 ID 精确寻址——歧义时调用方挑选后的那条路必须通。
async fn h07_explicit_id_is_precise(repo: &dyn HandoffRepository) {
    let beta = expect_found(repo.resolve_handoff(Some("beta")).await.unwrap());
    assert_eq!(beta.task_id, "beta");
    assert_eq!(beta.goal, "beta 的活");

    let alpha = expect_found(repo.resolve_handoff(Some("alpha")).await.unwrap());
    assert_eq!(alpha.task_id, "alpha");
    assert_eq!(alpha.goal, "alpha 的活");
}

/// H8：**多写入方互不遮蔽**——这是「多个会话同时存在」在契约层的保障。
///
/// @intent 若默认槽位是全局 `current`，第二个写入方无 ID 的 save 会变成同一任务的
///         新修订，于是第一条工作流**静默消失**（接手方永远看不到它）。分片槽位
///         `current@<client>` 正是为此而生；本用例是它不再复发的唯一硬约束。
async fn h08_writers_do_not_shadow_each_other(repo: &dyn HandoffRepository) {
    let zed = repo
        .save_handoff(HandoffInput::new("zed 的活", "第二轮").with_client("zed"))
        .await
        .unwrap();
    let buddy = repo
        .save_handoff(HandoffInput::new("workbuddy 的活", "第一轮").with_client("workbuddy"))
        .await
        .unwrap();

    assert_ne!(
        zed.task_id, buddy.task_id,
        "不同写入方必须落进不同槽位，否则后者会遮蔽前者"
    );
    assert_eq!(zed.task_id, "current@zed");
    assert_eq!(buddy.task_id, "current@workbuddy");
    assert_eq!(
        buddy.revision, 1,
        "workbuddy 的首次 save 不得变成 zed 槽位的下一个修订"
    );
    assert_eq!(
        expect_found(repo.resolve_handoff(Some("current@workbuddy")).await.unwrap()).goal,
        "workbuddy 的活",
        "workbuddy 槽位里必须是它自己的内容"
    );

    match repo.resolve_handoff(None).await.unwrap() {
        HandoffResolution::Ambiguous(candidates) => {
            let mut got = ids(&candidates);
            got.sort();
            for expected in ["alpha", "beta", "current@workbuddy", "current@zed"] {
                assert!(
                    got.contains(&expected.to_string()),
                    "候选里应包含 {expected}，实际 {got:?}"
                );
            }
        }
        other => panic!("多个写入方并行时必须返回 Ambiguous，实际 {other:?}"),
    }
}

/// H9：列表语义——每任务一条、排序稳定、`unacked_only` 生效、接手态反映最新修订。
async fn h09_list_semantics(repo: &dyn HandoffRepository) {
    repo.save_handoff(HandoffInput::new("gamma 的活", "已交接").with_task("gamma"))
        .await
        .unwrap();
    let acked = repo.ack_handoff(Some("gamma"), "cursor").await.unwrap();
    assert!(acked.won);

    let all = repo.list_handoffs(None, false, usize::MAX).await.unwrap();
    let mut seen = std::collections::HashSet::new();
    for s in &all {
        assert!(seen.insert(s.task_id.clone()), "任务 {} 在列表里重复出现", s.task_id);
    }
    assert_ordering(&all);

    let gamma = all.iter().find(|s| s.task_id == "gamma").expect("gamma 应在列表里");
    assert!(gamma.acked, "已接手的条目在 all 列表里应标记 acked");
    assert_eq!(gamma.acked_by, "cursor", "acked_by 应反映接手方");

    let unacked = pending(repo).await;
    assert!(
        unacked.iter().all(|s| s.task_id != "gamma"),
        "unacked_only=true 不得包含已接手的任务"
    );
    assert!(
        unacked.iter().any(|s| s.task_id == "current@zed"),
        "待接手列表应包含未接手的 current@zed"
    );
}

/// H10：摘要必须「认得出是哪一条」——这正是列表选择界面的可用性来源。
async fn h10_list_summary_is_recognizable(repo: &dyn HandoffRepository) {
    let long_state = format!("{}\n第二行不该进摘要", "起".repeat(200));
    repo.save_handoff(
        HandoffInput::new("摘要可辨认性", long_state)
            .with_task("excerpt")
            .with_client("zed")
            .with_session("A1")
            .with_next_steps(vec!["一".into(), "二".into(), "三".into()]),
    )
    .await
    .unwrap();

    let listed = repo.list_handoffs(Some("excerpt"), true, usize::MAX).await.unwrap();
    assert_eq!(listed.len(), 1);
    let s = &listed[0];
    assert_eq!(s.goal, "摘要可辨认性");
    assert_eq!(s.session_id, "A1", "会话标签必须进摘要（它是人认得出的那一列）");
    assert_eq!(s.client, "zed");
    assert_eq!(s.next_step_count, 3);
    assert!(s.created_at > 0);
    assert!(!s.acked);
    assert!(
        s.state_excerpt.chars().count() <= 120,
        "状态片段必须 ≤ 120 字符，实际 {}",
        s.state_excerpt.chars().count()
    );
    assert!(
        !s.state_excerpt.contains("第二行"),
        "摘要只取首行，不应把整段状态塞进来"
    );
    assert!(
        s.state_excerpt.ends_with('…'),
        "截断必须可见：{}",
        s.state_excerpt
    );
}

/// H11：**截断必须发生在排序之后**——先在 SQL 里 LIMIT 再排序会返回最旧的几条。
///
/// @intent 断言方式是「限定列表 == 全量列表的前 N 项」，与具体排序键无关，因此
///         不依赖两次 save 是否跨过一秒（时钟不可控，硬编码顺序会变成不稳定用例）。
///         插入顺序刻意与排序结果错开：若实现是「先截断后排序」，返回集合必然不同。
async fn h11_list_truncates_after_ordering(repo: &dyn HandoffRepository) {
    for id in ["lim-3", "lim-1", "lim-6", "lim-2", "lim-5", "lim-4"] {
        repo.save_handoff(HandoffInput::new(format!("{id} 的活"), "排队").with_task(id))
            .await
            .unwrap();
    }

    let full = repo.list_handoffs(Some("lim-"), true, usize::MAX).await.unwrap();
    assert_eq!(full.len(), 6);
    assert_ordering(&full);

    let limited = repo.list_handoffs(Some("lim-"), true, 3).await.unwrap();
    assert_eq!(limited.len(), 3, "limit=3 应恰好返回 3 条");
    assert_eq!(
        ids(&limited),
        ids(&full[..3]),
        "截断必须发生在排序之后：返回的应是排序后的前 3 条"
    );

    assert!(
        repo.list_handoffs(Some("lim-"), true, 0).await.unwrap().is_empty(),
        "limit=0 必须返回空集（而不是被当作「不限」）"
    );
}

/// H12：前缀过滤只影响「选哪些任务」，不影响排序与 limit 口径。
async fn h12_list_prefix_filters_without_changing_order(repo: &dyn HandoffRepository) {
    let lim = repo.list_handoffs(Some("lim-"), false, usize::MAX).await.unwrap();
    assert_eq!(lim.len(), 6, "前缀应恰好选中 6 条");
    assert!(lim.iter().all(|s| s.task_id.starts_with("lim-")));

    let none = repo.list_handoffs(Some("nope-"), false, usize::MAX).await.unwrap();
    assert!(none.is_empty(), "无匹配前缀应返回空集");

    let two = repo.list_handoffs(Some("lim-"), false, 2).await.unwrap();
    assert_eq!(ids(&two), ids(&lim[..2]), "前缀过滤后 limit 仍截断在排序之后");

    // 空前缀行为等同不过滤
    let all_empty_prefix = repo.list_handoffs(Some(""), false, usize::MAX).await.unwrap();
    let all = repo.list_handoffs(None, false, usize::MAX).await.unwrap();
    assert_eq!(ids(&all_empty_prefix), ids(&all));
}

/// H13：首次 ack 抢到接手权，接手方与时间落库。
async fn h13_ack_wins_once(repo: &dyn HandoffRepository) {
    repo.save_handoff(HandoffInput::new("ack 目标", "待接").with_task("ack-1"))
        .await
        .unwrap();
    let outcome = repo.ack_handoff(Some("ack-1"), "zed").await.unwrap();
    assert!(outcome.won, "首次 ack 应抢到");
    let h = expect_found(outcome.resolution);
    assert_eq!(h.task_id, "ack-1");
    assert!(h.acked_at.is_some());
    assert_eq!(h.acked_by, "zed");
}

/// H14：重复 ack 是幂等空操作，**不覆盖首次的接手方与时间**。
async fn h14_ack_is_idempotent(repo: &dyn HandoffRepository) {
    let before = expect_found(repo.resolve_handoff(Some("ack-1")).await.unwrap());
    let outcome = repo.ack_handoff(Some("ack-1"), "cursor").await.unwrap();
    assert!(!outcome.won, "已接手时不得再次抢到");
    let after = expect_found(outcome.resolution);
    assert_eq!(after.acked_at, before.acked_at, "接手时间不得被后到者改写");
    assert_eq!(after.acked_by, "zed", "接手方不得被后到者覆盖");
}

/// H15：无 ID 且存在多条候选时，ack **绝不替调用方猜**，且不产生任何副作用。
async fn h15_ack_without_id_never_guesses(repo: &dyn HandoffRepository) {
    repo.save_handoff(HandoffInput::new("ack-a 的活", "待接").with_task("ack-a"))
        .await
        .unwrap();
    repo.save_handoff(HandoffInput::new("ack-b 的活", "待接").with_task("ack-b"))
        .await
        .unwrap();

    let before = pending(repo).await;
    let outcome = repo.ack_handoff(None, "zed").await.unwrap();
    assert!(!outcome.won, "歧义时不得抢到接手权");
    match outcome.resolution {
        HandoffResolution::Ambiguous(candidates) => {
            assert!(candidates.len() >= 2);
        }
        other => panic!("歧义时 ack(None) 必须返回候选，实际 {other:?}"),
    }
    assert_eq!(
        ids(&pending(repo).await),
        ids(&before),
        "歧义时不得有任何一条被标记接手（这是「猜错了」的可观测后果）"
    );
}

/// H16：已接手的任务再 save → 新修订回到待接手（内容变了，上一次接手不算数）。
async fn h16_save_resets_ack_state(repo: &dyn HandoffRepository) {
    repo.save_handoff(HandoffInput::new("reset 的活", "第一版").with_task("reset-1"))
        .await
        .unwrap();
    assert!(repo.ack_handoff(Some("reset-1"), "zed").await.unwrap().won);
    assert!(!ids(&pending(repo).await).contains(&"reset-1".to_string()));

    let revised = repo
        .save_handoff(HandoffInput::new("reset 的活", "第二版").with_task("reset-1"))
        .await
        .unwrap();
    assert_eq!(revised.revision, 2);
    assert!(revised.is_pending(), "新修订必须重新回到待接手");
    assert_eq!(revised.acked_by, "", "新修订的接手方应为未声明");
    assert!(
        ids(&pending(repo).await).contains(&"reset-1".to_string()),
        "修订后应重新出现在待接手列表里"
    );
}

/// H17：删除整个任务（全部修订），且重复删除幂等。
async fn h17_delete_is_idempotent(repo: &dyn HandoffRepository) {
    repo.delete_handoff("ack-1").await.unwrap();
    assert_eq!(
        repo.resolve_handoff(Some("ack-1")).await.unwrap(),
        HandoffResolution::None
    );
    assert!(
        repo.list_handoffs(Some("ack-1"), false, usize::MAX).await.unwrap().is_empty(),
        "删除后不得再出现在列表里"
    );
    repo.delete_handoff("ack-1").await.unwrap();
    repo.delete_handoff("从未存在过").await.unwrap();
}

/// H18：非法输入一律拒绝，且**不产生任何修订**（整批先校验、再落库）。
async fn h18_invalid_input_is_atomic(repo: &dyn HandoffRepository) {
    let before = repo.list_handoffs(None, false, usize::MAX).await.unwrap();
    let alpha_before = expect_found(repo.resolve_handoff(Some("alpha")).await.unwrap()).revision;

    let invalid: Vec<HandoffInput> = vec![
        HandoffInput::new("", "空目标"),
        HandoffInput::new("目标", "   "),
        HandoffInput::new("非法 ID", "x").with_task("bad/id"),
        HandoffInput::new("非法 ID", "x").with_task("a b"),
        HandoffInput::new("超长", "x").with_next_steps(vec!["i".repeat(4 * 1024 + 1)]),
        HandoffInput::new("空元素", "x").with_files(vec!["ok".into(), "  ".into()]),
        // 同一个合法任务 + 非法正文：必须整体拒绝，不得产生修订 2
        HandoffInput::new("", "空目标").with_task("alpha"),
    ];
    for input in invalid {
        let err = repo.save_handoff(input).await.unwrap_err();
        assert!(
            matches!(err, StorageError::InvalidInput(_)),
            "应报 InvalidInput，实际 {err:?}"
        );
    }

    assert_eq!(
        ids(&repo.list_handoffs(None, false, usize::MAX).await.unwrap()),
        ids(&before),
        "非法输入不得产生任何任务"
    );
    assert_eq!(
        expect_found(repo.resolve_handoff(Some("alpha")).await.unwrap()).revision,
        alpha_before,
        "非法输入不得给已存在的任务追加修订"
    );
    assert_eq!(
        repo.resolve_handoff(Some("bad/id")).await.unwrap(),
        HandoffResolution::None
    );
}

// ---------------------------------------------------------------- 交接断言辅助

/// 断言排序规则：`created_at` 降序，同秒时 `task_id` 升序。
fn assert_ordering(items: &[HandoffSummary]) {
    for pair in items.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let ok = a.created_at > b.created_at
            || (a.created_at == b.created_at && a.task_id < b.task_id);
        assert!(
            ok,
            "排序应为 created_at 降序、同秒 task_id 升序，但 {} (t={}) 排在 {} (t={}) 之前",
            a.task_id, a.created_at, b.task_id, b.created_at
        );
    }
}

/// 取所有待接手的摘要。
async fn pending(repo: &dyn HandoffRepository) -> Vec<HandoffSummary> {
    repo.list_handoffs(None, true, usize::MAX).await.unwrap()
}

/// 摘要列表的 `task_id` 序列。
fn ids(items: &[HandoffSummary]) -> Vec<String> {
    items.iter().map(|s| s.task_id.clone()).collect()
}

/// 断言寻址命中并取出完整记录。
fn expect_found(resolution: HandoffResolution) -> crate::domain::Handoff {
    match resolution {
        HandoffResolution::Found(h) => *h,
        other => panic!("应命中唯一一条，实际 {other:?}"),
    }
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
