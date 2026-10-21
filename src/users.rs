//! 控制台账号：`_meta.db.users` 表的读写与凭据校验。
//!
//! @author yujinping
//! @intent 背景：控制台的登录凭据此前是**两个配置项**（`admin_user` +
//!         `admin_password_hash`）。这带来三个结构性限制：
//!         1. 只能有一个账号，「多人用同一个口令」是唯一形态；
//!         2. 增删账号必须改配置文件并重启服务；
//!         3. 配置文件的备份、截图、误提交都会扩散口令哈希——而它本可以只躺在库里。
//!         本模块把账号提升为**数据**：配置项退化为「首次启动时的种子」，之后以库为准。
//!
//! @intent 只存 Argon2id PHC 串，不存明文；明文只在「创建账号」这一次请求的内存里出现。
//!         与项目 token 的处置一致（[`crate::auth::sha256_hex`]），理由相同：
//!         凭据一旦落盘明文，泄漏面就从「一次」变成「一直」。
//!
//! @intent P8 起**有角色区分**（[`Role`]）：`admin` 可见全部项目与账号，
//!         `user` 只见自己名下的项目。P7 时期「凡能登录者皆管理员」的语义
//!         通过迁移回填保留（见 [`crate::meta::ensure_role_column`]），
//!         因此升级不会让任何既有账号突然失去权限。
//!
//! 与 [`crate::session`] 的分工：本模块只回答「这组凭据对不对、这个人是什么角色」，
//! 不碰 Cookie、不碰会话——那是会话模块的职责。

use crate::entity::user::{ActiveModel, Column, Entity as Users};
use anyhow::Context;
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::*;
use sea_orm::QueryOrder;
use std::time::{SystemTime, UNIX_EPOCH};

/// 登录名的最大长度（字符数）。
///
/// @intent 与 `validate_project_id` 同一量级：登录名会出现在日志、界面与配置里，
///         过长只会让日志换行。上限存在的意义是拦住误粘贴一整段文本。
pub const USERNAME_MAX_CHARS: usize = 64;

/// 账号角色。
///
/// @intent 只有两档，且**刻意不加第三档**（比如「只读审计」）：自托管场景下
///         真实存在的需求只有「这是我的东西」与「这是所有人的东西」两种，
///         中间态没有对应的人。多一档就要立刻回答「它能不能删项目 / 能不能
///         改别人口令」等一串没人有答案的问题。
///
/// @intent 数据范围的差别只有一条，其余全部相同：
///         - [`Role::Admin`]：可见并可操作**全部**项目与全部账号；
///         - [`Role::User`]：只见并可操作**自己名下**的项目，只能改自己的口令。
///         这条差异统一在 `admin` 模块的 `visible_project` / `require_admin` 处收口，
///         不在每个 handler 里各自判断——分散判断是越权的常见来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// 管理员：全部数据
    Admin,
    /// 普通账号（租户）：仅自己的数据
    User,
}

impl Role {
    /// 全部角色（穷尽性测试的锚点，也用于校验入参）。
    pub const ALL: [Role; 2] = [Role::Admin, Role::User];

    /// 入库 / 配置使用的字符串标识。
    ///
    /// @intent 取值与 `users.role` 列的历史值一一对应，不可随意变更：
    ///         改了它等于让存量行的语义漂移。
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }

    /// 从字符串标识解析。
    ///
    /// @intent 未知值返回 `None` 而不是**默认猜一个**：库里的角色值无法解析意味着
    ///         数据被外部改写或版本不匹配，此时按 `User` 处理会静默降权（用户
    ///         突然看不到自己的东西），按 `Admin` 处理则是提权。两种猜测都比
    ///         一次显式的失败更危险，故由调用方决定如何报告。
    ///
    /// @intent 容忍首尾空白与大小写，是为了**读路径**：`users.role` 列可能被
    ///         手工编辑过，配置项也可能带尾随空格，这些都不该让整个服务失效。
    ///         但**不要**把它当成 API 入参校验（见 `admin::parse_role`）：入参位置
    ///         上宽容的代价是把一个拼错的值升级成最高权限，而调用方毫无反馈。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "admin" => Some(Role::Admin),
            "user" => Some(Role::User),
            _ => None,
        }
    }

    /// 是否管理员。
    pub fn is_admin(&self) -> bool {
        matches!(self, Role::Admin)
    }
}

impl Default for Role {
    /// 缺省角色是 [`Role::User`]。
    ///
    /// @intent 缺省必须是**权限更小**的那个：任何「忘了指定角色」的路径都应该
    ///         得到一个权限受限的账号，而不是一个悄悄成为管理员的新用户。
    ///         注意播种管理员时**显式**指定 `Role::Admin`，不依赖这个缺省值。
    fn default() -> Self {
        Role::User
    }
}

/// 账号记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    /// 登录名
    pub username: String,
    /// 口令的 Argon2id PHC 串
    pub password_hash: String,
    /// 角色（决定数据范围）
    pub role: Role,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 最近一次口令变更时间（Unix 秒）
    pub updated_at: i64,
}

/// 登录名的合法性判定。
///
/// @intent 只禁掉「空白与控制字符」，而不像 `project_id` 那样收窄到
///         `[A-Za-z0-9_-]`：登录名是**人**的标识，中文名与邮箱地址都应当能用。
///         禁止空白是因为它会让「`alice ` 与 `alice` 是不是同一个账号」变成
///         一个永远答不清的问题（而作者本意通常是同一个）。
pub fn validate_username(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("登录名不能为空".to_string());
    }
    if name.chars().count() > USERNAME_MAX_CHARS {
        return Err(format!("登录名不得超过 {USERNAME_MAX_CHARS} 个字符"));
    }
    if let Some(ch) = name.chars().find(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("登录名不得包含空白或控制字符（发现 {ch:?}）"));
    }
    Ok(name.to_string())
}

/// 当前 Unix 秒。
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 首启引导的结果。
///
/// @intent 用枚举而不是 `bool`：启动日志需要区分三种情形，而它们对应的处置完全不同。
///         「库非空所以跳过」是**正常**的（重启时天天发生），若与「没有可用凭据」
///         共用一个 `false`，日志就只能在两者之间含糊其辞。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// 新建了种子管理员
    Created {
        /// 新建的登录名
        username: String,
    },
    /// 库里已有账号，种子被跳过（配置已不再是账号的真相源）
    Skipped {
        /// 库里现有的账号数
        existing: usize,
    },
    /// 配置里没有可用口令，无法建立种子账号
    NoSecret,
}

/// 首次启动时用配置中的 `admin_user` / `admin_password_hash` 播种管理员。
///
/// @intent **只在表为空时播种**。若每次启动都按配置覆写口令，那么「在控制台改了口令、
///         但配置文件还是旧哈希」的状态会在下一次重启时被静默回滚——用户会认为
///         改口令失效，而日志里什么都没有。让库成为唯一真相源，配置只做种子。
///
/// @intent 表非空但配置里换了用户名时**不新建**：配置已经表达不了「账号集合」这件事，
///         半信半疑地按它补一个账号，正是「同一件事两个来源」的典型后果。
///
/// @intent 种子账号**恒为管理员**：它是配置里那一对凭据的人化形态，而配置项本身
///         就是最高权限（`ADMIN_TOKEN` 也在同一个文件里）。若种子降级成普通账号，
///         一个全新的实例会在引导完成后立刻没有任何管理员，`users` 端点全部
///         不可达——那是一个无法自愈的状态。
pub async fn bootstrap_admin(
    db: &DatabaseConnection,
    username: &str,
    password_hash: &str,
) -> anyhow::Result<BootstrapOutcome> {
    let existing = count(db).await?;
    if existing > 0 {
        return Ok(BootstrapOutcome::Skipped { existing });
    }
    if password_hash.trim().is_empty() {
        return Ok(BootstrapOutcome::NoSecret);
    }

    let username = validate_username(username)
        .map_err(|reason| anyhow::anyhow!("admin_user is not a usable username: {reason}"))?;
    create_with_role(db, &username, password_hash, Role::Admin).await?;
    Ok(BootstrapOutcome::Created { username })
}

/// 账号总数。
pub async fn count(db: &DatabaseConnection) -> anyhow::Result<usize> {
    use sea_orm::PaginatorTrait;
    let total = Users::find().count(db).await?;
    Ok(total as usize)
}

/// 角色为 `admin` 的账号数。
///
/// @intent 「最后一个管理员」的保护需要它：删除 / 降级一个管理员之前，
///         必须能回答「这是不是最后一个」。用 `count` 会算错——普通账号
///         也是账号，但它们登不进账号管理页，删掉所有管理员的后果是永久锁死。
pub async fn count_admins(db: &DatabaseConnection) -> anyhow::Result<usize> {
    use sea_orm::PaginatorTrait;
    let total = Users::find()
        .filter(Column::Role.eq(Role::Admin.as_str()))
        .count(db)
        .await?;
    Ok(total as usize)
}

/// 首个管理员（按登录名升序，确定性）。
///
/// @intent 供 [`crate::meta::claim_unowned_projects`] 定位「存量项目归谁」。
///         取登录名升序而非创建时间：`created_at` 只到秒，同一批建的账号会同值，
///         排序因此不稳定，而认领结果必须与执行时机无关（它要可重入）。
pub async fn first_admin(db: &DatabaseConnection) -> anyhow::Result<Option<String>> {
    let row = Users::find()
        .filter(Column::Role.eq(Role::Admin.as_str()))
        .order_by_asc(Column::Username)
        .one(db)
        .await?;
    Ok(row.map(|model| model.username))
}

/// 按登录名取账号；不存在返回 `None`。
pub async fn find(db: &DatabaseConnection, username: &str) -> anyhow::Result<Option<UserRecord>> {
    let row = Users::find()
        .filter(Column::Username.eq(username))
        .one(db)
        .await?;
    match row {
        Some(model) => Ok(Some(record_of(model)?)),
        None => Ok(None),
    }
}

/// 全部账号，按登录名升序。
///
/// @intent 排序取 `username` 而非 `created_at`：账号数量是人工量级，稳定顺序比
///         「谁先建的」更有用——后者会让每次刷新列表时的行序都可能变，
///         而人的点击目标正落在那些行上。
pub async fn list(db: &DatabaseConnection) -> anyhow::Result<Vec<UserRecord>> {
    let rows = Users::find().order_by_asc(Column::Username).all(db).await?;
    rows.into_iter().map(record_of).collect()
}

/// 新建账号。
///
/// @intent 角色是**显式参数**，不提供「省略则取某默认值」的重载。建账号时
///         授权级别必须由调用方写下来：默认成管理员会让「多建一个测试账号」
///         变成一次提权；默认成普通账号则让播种管理员那条路径靠「记得多传一个
///         参数」来成立。两种默认都在把一次显式决定偷换成隐式约定。
///         调用方各写一行 `Role::User` / `Role::Admin`，读代码的人一眼看到权限。
///
/// @intent 调用方必须先算出 `password_hash`（见 [`crate::password::hash_password`]）：
///         本函数刻意不接受明文，否则「明文进数据库层」这条通道会一直存在，
///         而它只会在某次重构里被误用。
pub async fn create_with_role(
    db: &DatabaseConnection,
    username: &str,
    password_hash: &str,
    role: Role,
) -> anyhow::Result<()> {
    let now = now_secs();
    Users::insert(ActiveModel {
        username: Set(username.to_string()),
        password_hash: Set(password_hash.to_string()),
        role: Set(role.as_str().to_string()),
        created_at: Set(now),
        updated_at: Set(now),
    })
    .exec(db)
    .await
    .with_context(|| format!("cannot create console user `{username}`"))?;
    Ok(())
}

/// 删除账号，返回它此前是否存在。
///
/// @intent 返回 `bool` 而非 `Result<()>`：与 `meta::delete_project` 同一取向——
///         调用方需要区分「本来就没了」（幂等成功）与「确实删掉了一个」。
///         **最后一个管理员的保护不在这里**：它属于业务规则（「不得把管理面锁死」），
///         由 handler 在事务性语义外统一判定，避免存储层偷偷拒绝一次合法删除。
///         注意判据是**管理员数**而非账号数——剩一群普通账号时删掉最后一个管理员，
///         管理面同样锁死。
pub async fn delete(db: &DatabaseConnection, username: &str) -> anyhow::Result<bool> {
    let result = Users::delete_by_id(username.to_string()).exec(db).await?;
    Ok(result.rows_affected > 0)
}

/// 更新口令哈希，返回账号是否存在。
pub async fn update_password(
    db: &DatabaseConnection,
    username: &str,
    password_hash: &str,
) -> anyhow::Result<bool> {
    let affected = Users::update_many()
        .col_expr(Column::PasswordHash, Expr::value(password_hash.to_string()))
        .col_expr(Column::UpdatedAt, Expr::value(now_secs()))
        .filter(Column::Username.eq(username))
        .exec(db)
        .await?
        .rows_affected;
    Ok(affected > 0)
}

/// 更新角色，返回账号是否存在。
///
/// @intent 与 `update_password` 分开而不是合成一个「改账号」接口：两者的授权规则
///         不同（改自己的口令人人可为，改角色只有管理员可为），合成一个接口
///         就必须在 handler 里按字段分别判权，而「只传了一半字段」的请求
///         会让那个判断变得含糊。
pub async fn update_role(
    db: &DatabaseConnection,
    username: &str,
    role: Role,
) -> anyhow::Result<bool> {
    let affected = Users::update_many()
        .col_expr(Column::Role, Expr::value(role.as_str().to_string()))
        .col_expr(Column::UpdatedAt, Expr::value(now_secs()))
        .filter(Column::Username.eq(username))
        .exec(db)
        .await?
        .rows_affected;
    Ok(affected > 0)
}

/// 实体模型 → 账号记录。
///
/// @intent 角色解析失败是**错误**而非降级成默认值：`users.role` 只可能由本程序写入，
///         出现无法解析的值意味着库被外部改过或版本不匹配。此时静默当成普通账号
///         会让管理员在下次登录时突然失去权限，而日志里什么都没有。
fn record_of(model: crate::entity::user::Model) -> anyhow::Result<UserRecord> {
    let role = Role::parse(&model.role).ok_or_else(|| {
        anyhow::anyhow!(
            "user `{}` has an unrecognised role {:?}; expected one of {:?}",
            model.username,
            model.role,
            Role::ALL.map(|r| r.as_str())
        )
    })?;
    Ok(UserRecord {
        username: model.username,
        password_hash: model.password_hash,
        role,
        created_at: model.created_at,
        updated_at: model.updated_at,
    })
}

/// 校验一组凭据，成功时返回账号记录。
///
/// @intent **账号不存在时也要跑完一次口令校验**。否则「账号存在」与「不存在」会在
///         响应时间上差出一个数量级（Argon2id 是数十毫秒级），形成可利用的账号枚举
///         信道。这一条的代价是每次失败登录都烧掉一次哈希计算，而登录是低频人工
///         操作，这个代价换来的确定性远比它值钱（与 `console::login` 的旧实现同源，
///         此处只是把「固定比较用户名」换成了「固定计算一次哈希」）。
///
/// @intent 返回 `Option` 而不是错误枚举：调用方（登录接口）对「账号不存在」与
///         「口令错」必须给出**同一个** 401，任何更细的区分都会泄漏信息。
pub async fn authenticate(
    db: &DatabaseConnection,
    username: &str,
    password: &str,
) -> anyhow::Result<Option<UserRecord>> {
    let username = username.trim();
    let found = find(db, username).await?;

    // 借出 `record` 而不是移动它：函数末尾可能要把 `found` 原样返回，
    // 若在此处移走 `password_hash`，`found` 就成了「部分已移动」的值。
    let (hit, hash) = match &found {
        Some(record) => (true, record.password_hash.clone()),
        None => (false, dummy_hash()),
    };

    // 无论命中与否都执行：见本函数的 @intent。
    let ok = crate::password::verify_password(password, &hash);
    if ok && hit {
        Ok(found)
    } else {
        Ok(None)
    }
}

/// 「账号不存在」时用于耗散时间的一次性哈希串。
///
/// @intent 现场算一次并缓存，而不是在源码里写死一个 PHC 字面量：写死意味着它的
///         参数（m/t/p）从此与 `password::hash_password` 的实际参数脱钩，
///         将来调参后这条路径的耗时会对不上，枚举信道悄悄复活而没有任何提示。
fn dummy_hash() -> String {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        crate::password::hash_password("memora-timing-equalizer-not-a-real-credential")
            .unwrap_or_default()
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::init_meta_db;
    use sea_orm::Database;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 临时元库（含 users 表）。
    async fn temp_db() -> DatabaseConnection {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "memora_users_{}_{}.db",
            std::process::id(),
            n
        ));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let db = Database::connect(&url).await.unwrap();
        init_meta_db(&db).await.unwrap();
        db
    }

    fn phc(plain: &str) -> String {
        crate::password::hash_password(plain).unwrap()
    }

    /// 测试内的普通账号创建快捷方式。
    ///
    /// @intent 绝大多数用例关心的不是角色，而是「账号能不能登、口令能不能改」。
    ///         让它们每次写一遍 `Role::User` 只会把视线从被测行为上引开；
    ///         而真正测角色的用例显式调用 `create_with_role`，两者意图分明。
    async fn create(
        db: &DatabaseConnection,
        username: &str,
        password_hash: &str,
    ) -> anyhow::Result<()> {
        create_with_role(db, username, password_hash, Role::User).await
    }

    // ------------------------------------------------------------ 登录名校验

    /// 中文名、邮箱都能用；空、含空白、超长都必须被拒。
    #[test]
    fn username_validation_accepts_humans_and_rejects_ambiguous_forms() {
        assert_eq!(validate_username("  alice  ").unwrap(), "alice");
        assert_eq!(validate_username("于金平").unwrap(), "于金平");
        assert_eq!(validate_username("a@b.com").unwrap(), "a@b.com");

        assert!(validate_username("").is_err());
        assert!(validate_username("   ").is_err());
        assert!(validate_username("two words").is_err(), "内部空白会让账号寻址歧义");
        assert!(validate_username("tab\there").is_err());
        assert!(validate_username(&"a".repeat(USERNAME_MAX_CHARS + 1)).is_err());
        assert!(validate_username(&"a".repeat(USERNAME_MAX_CHARS)).is_ok());
    }

    // ------------------------------------------------------------ 引导

    /// 空库 + 有口令 → 建种子管理员；再跑一次必须跳过（幂等）。
    #[tokio::test]
    async fn bootstrap_seeds_once_and_then_skips() {
        let db = temp_db().await;
        let hash = phc("bootstrap-secret");

        match bootstrap_admin(&db, "root", &hash).await.unwrap() {
            BootstrapOutcome::Created { username } => assert_eq!(username, "root"),
            other => panic!("首次引导应建账号，实际 {other:?}"),
        }
        let record = find(&db, "root").await.unwrap().expect("种子账号应存在");
        assert!(crate::password::verify_password("bootstrap-secret", &record.password_hash));
        assert_eq!(
            record.role,
            Role::Admin,
            "种子账号必须是管理员，否则全新实例会没有任何管理员"
        );

        // 再跑一次：即使配置换了用户名也不得新增
        match bootstrap_admin(&db, "someone-else", &hash).await.unwrap() {
            BootstrapOutcome::Skipped { existing } => assert_eq!(existing, 1),
            other => panic!("库非空时应跳过，实际 {other:?}"),
        }
        assert!(
            find(&db, "someone-else").await.unwrap().is_none(),
            "配置不能再创造第二个账号"
        );
    }

    /// 没有配置口令时不建账号——而不是建一个「空口令可登录」的账号。
    #[tokio::test]
    async fn bootstrap_without_secret_creates_nothing() {
        let db = temp_db().await;
        assert_eq!(
            bootstrap_admin(&db, "root", "   ").await.unwrap(),
            BootstrapOutcome::NoSecret
        );
        assert_eq!(count(&db).await.unwrap(), 0);
    }

    /// 非法用户名必须让引导失败并说明原因，而不是悄悄落一个坏名字。
    #[tokio::test]
    async fn bootstrap_rejects_an_unusable_username() {
        let db = temp_db().await;
        let err = bootstrap_admin(&db, "two words", &phc("whatever-secret"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("admin_user"), "{err}");
    }

    // ------------------------------------------------------------ 凭据校验

    /// 正确口令返回记录；错误口令、不存在的账号都返回 `None`。
    #[tokio::test]
    async fn authenticate_accepts_only_the_exact_credentials() {
        let db = temp_db().await;
        create(&db, "alice", &phc("correct-horse")).await.unwrap();

        let ok = authenticate(&db, "alice", "correct-horse").await.unwrap();
        assert_eq!(ok.map(|r| r.username), Some("alice".to_string()));

        assert!(
            authenticate(&db, "alice", "wrong").await.unwrap().is_none(),
            "错误口令必须被拒"
        );
        assert!(
            authenticate(&db, "bob", "correct-horse").await.unwrap().is_none(),
            "不存在的账号必须被拒"
        );
        // 前后空白在用户名上容忍（与 validate_username 的 trim 一致）
        assert!(authenticate(&db, "  alice  ", "correct-horse")
            .await
            .unwrap()
            .is_some());
    }

    /// 「账号不存在」与「口令错」的耗时必须同量级：两者都要跑 argon2。
    ///
    /// @intent 断言的是**耗时量级**而不是精确值：只要缺失账号的路径没有跳过哈希，
    ///         它就不可能快出一个数量级。用「不小于单次哈希耗时的 1/4」做下界，
    ///         既不会被机器负载抖动打翻，又足以在「忘了跑哈希」时立刻失败
    ///         （那种情况下耗时会掉到微秒级）。
    #[tokio::test]
    async fn missing_account_still_pays_the_password_hash_cost() {
        let db = temp_db().await;
        create(&db, "alice", &phc("correct-horse")).await.unwrap();

        // 先算一次真实校验耗时作为基准（同时预热 dummy hash 的 OnceLock）
        let start = std::time::Instant::now();
        let _ = authenticate(&db, "alice", "wrong").await.unwrap();
        let known = start.elapsed();

        let start = std::time::Instant::now();
        let _ = authenticate(&db, "nobody", "wrong").await.unwrap();
        let unknown = start.elapsed();

        assert!(
            unknown * 4 >= known,
            "缺失账号路径不得跳过口令哈希：known={known:?} unknown={unknown:?}"
        );
    }

    // ------------------------------------------------------------ 增删改

    /// 列表按用户名升序，且删除 / 改口令都如实报告影响行数。
    #[tokio::test]
    async fn crud_reports_what_really_happened() {
        let db = temp_db().await;
        for name in ["zeta", "alpha"] {
            create(&db, name, &phc("secret-for-test")).await.unwrap();
        }

        let names: Vec<String> = list(&db).await.unwrap().into_iter().map(|u| u.username).collect();
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);

        assert!(update_password(&db, "alpha", &phc("new-secret-for-test")).await.unwrap());
        assert!(
            authenticate(&db, "alpha", "new-secret-for-test").await.unwrap().is_some(),
            "改口令后新口令应可用"
        );
        assert!(
            authenticate(&db, "alpha", "secret-for-test").await.unwrap().is_none(),
            "改口令后旧口令必须立刻失效"
        );
        assert!(
            !update_password(&db, "ghost", &phc("whatever-secret")).await.unwrap(),
            "不存在的账号应返回 false"
        );

        assert!(delete(&db, "zeta").await.unwrap());
        assert!(!delete(&db, "zeta").await.unwrap(), "重复删除应返回 false");
        assert_eq!(count(&db).await.unwrap(), 1);
    }

    /// 同一用户名重复创建必须失败（主键冲突），而不是静默覆盖掉原口令。
    ///
    /// @intent 静默覆盖是本项目反复拒绝的失败形态：它会让「新建一个同名账号」
    ///         看起来成功，实际把别人的口令换成了自己给的那个。
    #[tokio::test]
    async fn creating_a_duplicate_username_is_an_error() {
        let db = temp_db().await;
        create(&db, "alice", &phc("first-secret")).await.unwrap();
        assert!(create(&db, "alice", &phc("second-secret")).await.is_err());

        assert!(
            authenticate(&db, "alice", "first-secret").await.unwrap().is_some(),
            "原口令不得被后到的创建请求改写"
        );
    }

    // ------------------------------------------------------------ 角色（P8）

    /// 角色标识必须能往返，且未知值**不猜**。
    #[test]
    fn role_parse_roundtrip_and_rejects_unknown() {
        for role in Role::ALL {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("ADMIN"), Some(Role::Admin), "大小写不敏感");
        assert_eq!(Role::parse(" user "), Some(Role::User), "容忍首尾空白");
        assert_eq!(Role::parse("root"), None, "未知角色不得被猜成某个权限");
        assert_eq!(Role::parse(""), None);

        // 缺省是权限更小的那个：任何「忘了传角色」的路径都不该造出管理员
        assert_eq!(Role::default(), Role::User);
        assert!(!Role::default().is_admin());
    }

    /// 缺省 `create` 造普通账号；`create_with_role` 才显式给权限。
    ///
    /// @intent 这条守的是「多建一个测试账号 = 一次提权」这种事故：
    ///         `create` 是被复用最多的入口，它的缺省值必须是最小权限。
    #[tokio::test]
    async fn create_defaults_to_a_plain_tenant() {
        let db = temp_db().await;
        create(&db, "tenant", &phc("tenant-secret")).await.unwrap();
        create_with_role(&db, "boss", &phc("boss-secret"), Role::Admin)
            .await
            .unwrap();

        assert_eq!(find(&db, "tenant").await.unwrap().unwrap().role, Role::User);
        assert_eq!(find(&db, "boss").await.unwrap().unwrap().role, Role::Admin);
    }

    /// `authenticate` 必须把角色带出来——鉴权后的授权全靠它。
    #[tokio::test]
    async fn authenticate_returns_the_role() {
        let db = temp_db().await;
        create_with_role(&db, "boss", &phc("boss-secret"), Role::Admin)
            .await
            .unwrap();
        create(&db, "tenant", &phc("tenant-secret")).await.unwrap();

        assert_eq!(
            authenticate(&db, "boss", "boss-secret")
                .await
                .unwrap()
                .unwrap()
                .role,
            Role::Admin
        );
        assert_eq!(
            authenticate(&db, "tenant", "tenant-secret")
                .await
                .unwrap()
                .unwrap()
                .role,
            Role::User
        );
    }

    /// 管理员计数（含 0）与「首个管理员」的确定性。
    #[tokio::test]
    async fn admin_counts_and_first_admin_are_deterministic() {
        let db = temp_db().await;
        assert_eq!(count_admins(&db).await.unwrap(), 0);
        assert_eq!(first_admin(&db).await.unwrap(), None);

        // 故意倒序创建，断言取的是登录名序而非创建顺序
        create_with_role(&db, "zoe", &phc("zoe-secret"), Role::Admin)
            .await
            .unwrap();
        create_with_role(&db, "adam", &phc("adam-secret"), Role::Admin)
            .await
            .unwrap();
        create(&db, "tenant", &phc("tenant-secret")).await.unwrap();

        assert_eq!(count_admins(&db).await.unwrap(), 2, "普通账号不计入");
        assert_eq!(count(&db).await.unwrap(), 3);
        assert_eq!(first_admin(&db).await.unwrap().as_deref(), Some("adam"));
    }

    /// 改角色要如实报告影响行数；降级后 `count_admins` 立刻反映。
    #[tokio::test]
    async fn update_role_reports_what_really_happened() {
        let db = temp_db().await;
        create_with_role(&db, "boss", &phc("boss-secret"), Role::Admin)
            .await
            .unwrap();

        assert!(update_role(&db, "boss", Role::User).await.unwrap());
        assert_eq!(find(&db, "boss").await.unwrap().unwrap().role, Role::User);
        assert_eq!(count_admins(&db).await.unwrap(), 0);

        assert!(update_role(&db, "boss", Role::Admin).await.unwrap());
        assert_eq!(count_admins(&db).await.unwrap(), 1);

        assert!(
            !update_role(&db, "ghost", Role::Admin).await.unwrap(),
            "不存在的账号应返回 false"
        );
    }

    /// 库里角色值无法解析时必须报错，而不是静默降级或提权。
    ///
    /// @intent 这是「数据被外部改过 / 版本不匹配」的唯一可检测时刻。静默降级会让
    ///         管理员莫名失去权限，静默提权则更糟——两者都不会在日志里留痕。
    #[tokio::test]
    async fn unrecognised_role_in_the_database_is_an_error() {
        let db = temp_db().await;
        create(&db, "alice", &phc("alice-secret")).await.unwrap();

        db.execute(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "UPDATE users SET role = 'superuser' WHERE username = 'alice'",
        ))
        .await
        .unwrap();

        let err = find(&db, "alice").await.unwrap_err();
        assert!(err.to_string().contains("superuser"), "{err}");
        assert!(
            authenticate(&db, "alice", "alice-secret").await.is_err(),
            "角色损坏时登录必须失败，而不是按某个默认角色放行"
        );
    }
}