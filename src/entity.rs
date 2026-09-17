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
}
