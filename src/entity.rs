//! 数据模型实体（sea-orm）。
//!
//! @author yujinping

/// 项目元库实体：projects 表（全局 _meta.db）。
pub mod project {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "projects")]
    pub struct Model {
        /// 项目标识（主键，明文）
        #[sea_orm(primary_key, column_type = "Text")]
        pub project_id: String,
        /// Bearer Token 的 SHA-256 摘要（不存明文）
        #[sea_orm(column_type = "Text")]
        pub token_hash: String,
        /// 项目库 SQLite 相对路径
        #[sea_orm(column_type = "Text")]
        pub db_path: String,
        /// 创建时间（Unix 秒）
        #[sea_orm(column_type = "BigInteger")]
        pub created_at: i64,
        /// 该项目使用的存储后端标识（sqlite_file / in_mem / ...），见 docs §12.5
        #[sea_orm(column_type = "Text", default_value = "sqlite_file")]
        pub backend: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// 项目库（`data/{project_id}/mem.db`）实体：实体 / 观测 / 关系三元组。
///
/// @intent sea-orm 在该文件中仅作为**后端实现内部的** SQL 执行层与类型映射层，
///         不向上层泄漏（上层只认 `crate::domain` 的领域类型），见 docs §12.7。
pub mod memory {
    /// 实体表 `entities`。
    pub mod entities {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "entities")]
        pub struct Model {
            /// 实体名（主键，项目内唯一）
            #[sea_orm(primary_key, column_type = "Text")]
            pub name: String,
            /// 实体类型，缺省 unknown
            #[sea_orm(column_type = "Text", default_value = "unknown")]
            pub entity_type: String,
            /// 创建时间（Unix 秒）
            #[sea_orm(column_type = "BigInteger")]
            pub created_at: i64,
            /// 登记来源标识，空串 = 未声明
            #[sea_orm(column_type = "Text", default_value = "")]
            pub source: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// 观测表 `observations`。
    pub mod observations {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "observations")]
        pub struct Model {
            /// 自增主键
            #[sea_orm(primary_key, column_type = "BigInteger")]
            pub id: i64,
            /// 所属实体名
            #[sea_orm(column_type = "Text")]
            pub entity_name: String,
            /// 事实内容
            #[sea_orm(column_type = "Text")]
            pub content: String,
            /// 创建时间（Unix 秒）
            #[sea_orm(column_type = "BigInteger")]
            pub created_at: i64,
            /// 写入来源标识，空串 = 未声明
            #[sea_orm(column_type = "Text", default_value = "")]
            pub source: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// 关系表 `relations`。
    pub mod relations {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "relations")]
        pub struct Model {
            /// 自增主键
            #[sea_orm(primary_key, column_type = "BigInteger")]
            pub id: i64,
            /// 起点实体名
            #[sea_orm(column_type = "Text")]
            pub from_name: String,
            /// 终点实体名
            #[sea_orm(column_type = "Text")]
            pub to_name: String,
            /// 关系类型
            #[sea_orm(column_type = "Text")]
            pub relation_type: String,
            /// 创建时间（Unix 秒）
            #[sea_orm(column_type = "BigInteger")]
            pub created_at: i64,
            /// 写入来源标识，空串 = 未声明
            #[sea_orm(column_type = "Text", default_value = "")]
            pub source: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// 交接表 `handoffs`（P4.3）。
    ///
    /// @intent 交接与记忆是两种写语义（记忆幂等合并 / 交接追加修订），故独立成表。
    ///         主键为 `(task_id, revision)`：一次 save = 一条**不可变**修订，
    ///         旧修订永不改写，接手状态只落在最新修订上（见 docs §7.1 / §7.2）。
    ///         四个数组列存 JSON 文本、空数组序列化为 `[]`（不允许 NULL）——
    ///         「整条读取」是唯一读法，故不做子表（见 docs §3.3）。
    pub mod handoffs {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "handoffs")]
        pub struct Model {
            /// 任务标识（复合主键之一）
            #[sea_orm(primary_key, column_type = "Text")]
            pub task_id: String,
            /// 修订号，从 1 单调递增（复合主键之二）
            #[sea_orm(primary_key, column_type = "BigInteger")]
            pub revision: i64,
            /// 这活要达成什么
            #[sea_orm(column_type = "Text")]
            pub goal: String,
            /// 现在到哪了
            #[sea_orm(column_type = "Text")]
            pub state: String,
            /// JSON 数组：下一步
            #[sea_orm(column_type = "Text")]
            pub next_steps: String,
            /// JSON 数组：关键文件 / 位置
            #[sea_orm(column_type = "Text")]
            pub files: String,
            /// JSON 数组：已定事项
            #[sea_orm(column_type = "Text")]
            pub decisions: String,
            /// JSON 数组：待拍板事项
            #[sea_orm(column_type = "Text")]
            pub open_questions: String,
            /// 写入方标识，空串 = 未声明
            #[sea_orm(column_type = "Text", default_value = "")]
            pub client: String,
            /// 写入方会话标签，空串 = 未声明；仅作溯源展示，不参与寻址
            #[sea_orm(column_type = "Text", default_value = "")]
            pub session_id: String,
            /// 创建时间（Unix 秒）
            #[sea_orm(column_type = "BigInteger")]
            pub created_at: i64,
            /// 接手时间；NULL = 尚未被接手
            #[sea_orm(column_type = "BigInteger", nullable)]
            pub acked_at: Option<i64>,
            /// 接手方标识，空串 = 未声明
            #[sea_orm(column_type = "Text", default_value = "")]
            pub acked_by: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }
}
/// 单库（单 SQLite 文件 + 租户列）实体：表结构与 [`memory`] 一致，仅多一列 `project_id`。
///
/// @author yujinping
/// @intent P6 的 `sqlite_single` 后端把所有项目放进同一个库，租户隔离全靠 `project_id`
///         列过滤，故 `entities` 的主键由 `name` 扩为 `(project_id, name)`，
///         `handoffs` 的主键由 `(task_id, revision)` 扩为 `(project_id, task_id, revision)`；
///         观测 / 关系仍是全库自增 id，删除时必须附带租户条件（见 `storage::sqlite_single`）。
///         与 [`memory`] 一样，sea-orm 在此仅作为后端实现内部的 SQL 执行层，不向上层泄漏。
///
/// @intent P6（`sqlite_single` 多项目单库后端）的实体定义，由 `storage::sqlite_single`
///         接线使用；sea-orm 实体若无人引用会触发 dead_code，接线后豁免即移除。
pub mod single {
    /// 实体表 `entities`（主键为 `(project_id, name)`）。
    pub mod entities {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "entities")]
        pub struct Model {
            /// 租户（项目）标识
            #[sea_orm(primary_key, column_type = "Text")]
            pub project_id: String,
            /// 实体名（项目内唯一）
            #[sea_orm(primary_key, column_type = "Text")]
            pub name: String,
            /// 实体类型，缺省 unknown
            #[sea_orm(column_type = "Text", default_value = "unknown")]
            pub entity_type: String,
            /// 创建时间（Unix 秒）
            #[sea_orm(column_type = "BigInteger")]
            pub created_at: i64,
            /// 登记来源标识，空串 = 未声明
            #[sea_orm(column_type = "Text", default_value = "")]
            pub source: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// 观测表 `observations`（id 全库自增）。
    pub mod observations {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "observations")]
        pub struct Model {
            /// 自增主键（全库唯一，非项目内）
            #[sea_orm(primary_key, column_type = "BigInteger")]
            pub id: i64,
            /// 租户（项目）标识
            #[sea_orm(column_type = "Text")]
            pub project_id: String,
            /// 所属实体名
            #[sea_orm(column_type = "Text")]
            pub entity_name: String,
            /// 事实内容
            #[sea_orm(column_type = "Text")]
            pub content: String,
            /// 创建时间（Unix 秒）
            #[sea_orm(column_type = "BigInteger")]
            pub created_at: i64,
            /// 写入来源标识，空串 = 未声明
            #[sea_orm(column_type = "Text", default_value = "")]
            pub source: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// 关系表 `relations`（id 全库自增）。
    pub mod relations {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "relations")]
        pub struct Model {
            /// 自增主键（全库唯一，非项目内）
            #[sea_orm(primary_key, column_type = "BigInteger")]
            pub id: i64,
            /// 租户（项目）标识
            #[sea_orm(column_type = "Text")]
            pub project_id: String,
            /// 起点实体名
            #[sea_orm(column_type = "Text")]
            pub from_name: String,
            /// 终点实体名
            #[sea_orm(column_type = "Text")]
            pub to_name: String,
            /// 关系类型
            #[sea_orm(column_type = "Text")]
            pub relation_type: String,
            /// 创建时间（Unix 秒）
            #[sea_orm(column_type = "BigInteger")]
            pub created_at: i64,
            /// 写入来源标识，空串 = 未声明
            #[sea_orm(column_type = "Text", default_value = "")]
            pub source: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// 交接表 `handoffs`（主键为 `(project_id, task_id, revision)`）。
    ///
    /// @intent 列与 [`memory::handoffs`] 完全一致，仅主键加租户维度：
    ///         单库里不同项目可以有同名任务（含各自的 `current@<client>` 槽位），
    ///         修订号分配、解析与 ack 的 CAS 都必须带租户条件（见 `storage::sqlite_single`）。
    pub mod handoffs {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "handoffs")]
        pub struct Model {
            /// 租户（项目）标识（复合主键之一）
            #[sea_orm(primary_key, column_type = "Text")]
            pub project_id: String,
            /// 任务标识（复合主键之二）
            #[sea_orm(primary_key, column_type = "Text")]
            pub task_id: String,
            /// 修订号，从 1 单调递增（复合主键之三）
            #[sea_orm(primary_key, column_type = "BigInteger")]
            pub revision: i64,
            /// 这活要达成什么
            #[sea_orm(column_type = "Text")]
            pub goal: String,
            /// 现在到哪了
            #[sea_orm(column_type = "Text")]
            pub state: String,
            /// JSON 数组：下一步
            #[sea_orm(column_type = "Text")]
            pub next_steps: String,
            /// JSON 数组：关键文件 / 位置
            #[sea_orm(column_type = "Text")]
            pub files: String,
            /// JSON 数组：已定事项
            #[sea_orm(column_type = "Text")]
            pub decisions: String,
            /// JSON 数组：待拍板事项
            #[sea_orm(column_type = "Text")]
            pub open_questions: String,
            /// 写入方标识，空串 = 未声明
            #[sea_orm(column_type = "Text", default_value = "")]
            pub client: String,
            /// 写入方会话标签，空串 = 未声明；仅作溯源展示，不参与寻址
            #[sea_orm(column_type = "Text", default_value = "")]
            pub session_id: String,
            /// 创建时间（Unix 秒）
            #[sea_orm(column_type = "BigInteger")]
            pub created_at: i64,
            /// 接手时间；NULL = 尚未被接手
            #[sea_orm(column_type = "BigInteger", nullable)]
            pub acked_at: Option<i64>,
            /// 接手方标识，空串 = 未声明
            #[sea_orm(column_type = "Text", default_value = "")]
            pub acked_by: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }
}
