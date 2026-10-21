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
        /// 归属账号（`users.username`）；空串 = 历史遗留的无主项目
        ///
        /// @intent 归属落在**项目**这一层，而不是给每条记忆 / 交接加 owner 列：
        ///         一块记忆、一条交接本就只属于一个项目，而项目库已按 project_id
        ///         物理隔离（`data/{project_id}/mem.db`）。在记录级再记一遍归属，
        ///         等于把同一事实存两处，且必须保证两处永不冲突——那是纯粹的
        ///         一致性负担，换不来任何额外的隔离。
        ///
        /// @intent 空串是一个**显式的迁移中间态**而非错误：P7 及更早的库没有这一列，
        ///         `ADD COLUMN ... DEFAULT ''` 会把存量行留成空串。启动时的
        ///         [`crate::meta::claim_unowned_projects`] 会把它认领给管理员；
        ///         若此时系统里还没有任何账号（只用 ADMIN_TOKEN 的部署），
        ///         空串会一直保留——此类项目对管理员可见、对租户不可见。
        ///
        /// @intent 不加外键约束：SQLite 的外键默认关闭（`PRAGMA foreign_keys`），
        ///         开了也只是把「删账号时项目怎么办」从业务决策变成一次级联失败。
        ///         归属的有效性由 `admin` 模块在写入时保证，读时按字符串比较即可。
        #[sea_orm(column_type = "Text", default_value = "")]
        pub owner: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// 控制台账号实体：`users` 表（全局 `_meta.db`）。
///
/// @intent 在此之前「控制台凭据」是一对配置项（`admin_user` + `admin_password_hash`），
///         只能有一个账号，且换账号必须改文件、重启服务。有了本表之后账号是**数据**：
///         可增删、可改口令，而配置文件退化为「初始管理员的种子」。
pub mod user {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "users")]
    pub struct Model {
        /// 登录名（主键；大小写敏感，与展示名一致）
        ///
        /// @intent 不做归一化（例如强制小写）：登录名同时是日志里的主体标识，
        ///         静默改写会让「我明明建的是 Alice」变成一次无效排查。
        #[sea_orm(primary_key, column_type = "Text")]
        pub username: String,
        /// 口令的 Argon2id PHC 串（**不存明文**）
        #[sea_orm(column_type = "Text")]
        pub password_hash: String,
        /// 角色标识：`admin` 或 `user`
        ///
        /// @intent 存字符串而非整数枚举：与 `projects.backend` 同一取向——库里的值
        ///         必须能被人直接读懂，`sqlite3 _meta.db 'select * from users'`
        ///         一眼就能回答「谁是管理员」。取值不可随意变更（写入即成为契约）。
        ///
        /// @intent 存量回填为 `admin`：P7 时期「能登录控制台的都是管理员」，
        ///         迁移若默认成 `user`，会让线上唯一的账号在升级瞬间失去管理权，
        ///         而它此刻连管理员都还不是——一个不可自愈的死锁。
        #[sea_orm(column_type = "Text", default_value = "admin")]
        pub role: String,
        /// 创建时间（Unix 秒）
        #[sea_orm(column_type = "BigInteger")]
        pub created_at: i64,
        /// 最近一次口令变更时间（Unix 秒）
        #[sea_orm(column_type = "BigInteger")]
        pub updated_at: i64,
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

    /// 会话表 `conversations`（P5.1；主键为 `(session_id, turn_index)`，单项目库内无租户列）。
    ///
    /// @intent 逐轮 transcript 追加表：`turn_index` 由后端在会话内从 0 单调分配，
    ///         无自增列——顺序即语义，任何排序都必须走主键序。
    pub mod conversations {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "conversations")]
        pub struct Model {
            /// 会话标识（复合主键之一）
            #[sea_orm(primary_key, column_type = "Text")]
            pub session_id: String,
            /// 轮次序号，会话内从 0 单调递增（复合主键之二）
            #[sea_orm(primary_key, column_type = "BigInteger")]
            pub turn_index: i64,
            /// 说话角色：user / assistant / system
            #[sea_orm(column_type = "Text")]
            pub role: String,
            /// 该轮正文
            #[sea_orm(column_type = "Text")]
            pub content: String,
            /// 写入时间（Unix 秒）
            #[sea_orm(column_type = "BigInteger")]
            pub created_at: i64,
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

    /// 会话表 `conversations`（P5.1；主键为 `(project_id, session_id, turn_index)`）。
    ///
    /// @intent 列与 [`memory::conversations`] 一致，主键加租户维度：单库里不同项目
    ///         可以有同名会话，轮次分配与拉取都必须带租户条件（见 `storage::sqlite_single`）。
    pub mod conversations {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "conversations")]
        pub struct Model {
            /// 租户（项目）标识（复合主键之一）
            #[sea_orm(primary_key, column_type = "Text")]
            pub project_id: String,
            /// 会话标识（复合主键之二）
            #[sea_orm(primary_key, column_type = "Text")]
            pub session_id: String,
            /// 轮次序号，会话内从 0 单调递增（复合主键之三）
            #[sea_orm(primary_key, column_type = "BigInteger")]
            pub turn_index: i64,
            /// 说话角色：user / assistant / system
            #[sea_orm(column_type = "Text")]
            pub role: String,
            /// 该轮正文
            #[sea_orm(column_type = "Text")]
            pub content: String,
            /// 写入时间（Unix 秒）
            #[sea_orm(column_type = "BigInteger")]
            pub created_at: i64,
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
