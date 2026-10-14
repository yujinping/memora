//! 网页控制台的会话：内存会话表、Cookie 解析与签发、显式登出。
//!
//! @author yujinping
//! @intent 背景：管理面此前只有 Bearer token 一条路径（`ADMIN_TOKEN` 与项目 token），
//!         两者都要求调用方自己在请求头里带上凭据——这对脚本是优点，对浏览器是负担。
//!         本模块为网页控制台引入**会话**，使登录一次后由浏览器自动携带。
//!
//! @intent 用**内存会话表**而不是签名 Cookie，是这里最重要的一个取舍：
//!         - 签名 Cookie 需要一把长期密钥，于是「密钥怎么生成、存哪、怎么轮换」
//!           立刻变成三个新的运维问题，而落盘的密钥又是一个必须保护的秘密；
//!         - 内存表不需要任何密钥：会话 id 本身就是 256 bit 随机量，服务端持有对应关系，
//!           客户端无法自证身份，因而也不存在「伪造一个签名 Cookie」这条攻击路径。
//!         代价是重启后所有会话失效（对管理面是正向属性，等于强制重新认证），
//!         以及多实例之间不共享会话——而 memora 是单实例模型（见 `config` 模块文档），
//!         这个代价当前为零。
//!
//! @intent 会话有效期是**绝对**的（不滑动）：一直开着页面不会让会话永不过期。
//!         滑动过期会让一个被窃取的 id 只要持续被使用就永不失效。
//!
//! @intent 与 `/mcp` 的凭据**严格隔离**：会话 Cookie 不是项目 token，也不参与 `/mcp`
//!         鉴权。这条由 `routes` 的装配顺序保证——`/mcp` 的中间件只认 Bearer。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue};

use crate::auth;

/// 会话 Cookie 名。
pub const COOKIE_NAME: &str = "memora_session";

/// 会话有效期（12 小时）。
///
/// @intent 取「一个工作日」量级：短到被窃取的 id 在隔天后失效，长到不需要一天内
///         反复登录。绝对过期，不因活动而续期。
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// 一条会话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// 登录用户名（用于界面上显示「当前以谁的身份登录」）
    pub user: String,
    /// 绝对过期时刻
    pub expires_at: Instant,
}

/// 内存会话表。
///
/// @intent `Mutex` 而非 `RwLock`：临界区只有一次哈希表读写，无任何 `.await`，
///         读多写少的收益抵不过多一处锁类型选择的心智成本。
#[derive(Debug, Default)]
pub struct SessionStore {
    /// 会话 id → 会话
    inner: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    /// 新建空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 以默认有效期创建会话，返回会话 id。
    pub fn create(&self, user: &str) -> anyhow::Result<String> {
        self.create_with_ttl(user, SESSION_TTL)
    }

    /// 以指定有效期创建会话。
    ///
    /// @intent 有效期可注入使过期逻辑能被**确定性**验证：测试传入零时长即可让会话
    ///         立即过期，不必 `sleep` 等待——基于真实时钟的等待型断言在负载高的机器上
    ///         必然出现偶发失败。
    pub fn create_with_ttl(&self, user: &str, ttl: Duration) -> anyhow::Result<String> {
        let id = auth::random_hex(auth::TOKEN_BYTES)?;
        let session = Session {
            user: user.to_string(),
            expires_at: Instant::now() + ttl,
        };

        let mut guard = self.lock();
        // 顺手清理：会话表只在登录时增长，把清理放在这里即可避免为它单独起一个后台任务。
        guard.retain(|_, s| s.expires_at > Instant::now());
        guard.insert(id.clone(), session);
        Ok(id)
    }

    /// 查询会话；已过期则顺手移除并返回 `None`。
    ///
    /// @intent 「过期即移除」而非「读取时判过期」：后者会让过期条目一直占着内存，
    ///         直到下次登录才被清理。
    pub fn get(&self, id: &str) -> Option<Session> {
        let mut guard = self.lock();
        match guard.get(id) {
            Some(session) if session.expires_at > Instant::now() => Some(session.clone()),
            Some(_) => {
                guard.remove(id);
                None
            }
            None => None,
        }
    }

    /// 移除会话，返回它此前是否存在。
    pub fn remove(&self, id: &str) -> bool {
        self.lock().remove(id).is_some()
    }

    /// 当前会话数（含尚未被清理的过期条目）。
    ///
    /// @intent 仅供测试观察 `create` / `remove` 对表的实际影响。刻意不叫 `len`：
    ///         `len` 会立刻招来「怎么没有 `is_empty`」的约定，而本类型既不需要
    ///         这对方法，也不需要给外部一个「数会话」的入口——会话数不是任何决策的输入，
    ///         真要对外可见应从日志出而非接口出。
    #[cfg(test)]
    pub fn count_sessions(&self) -> usize {
        self.lock().len()
    }

    /// 取锁；中毒的锁按「无会话」处理，而不是让整个服务 panic。
    ///
    /// @intent 会话表中毒意味着某个持有者在临界区内 panic 了——但临界区只有哈希表
    ///         读写，不可能 panic。真发生了也不该把「登录不了」升级成「服务崩溃」。
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            tracing::error!("session store mutex was poisoned; recovering");
            poisoned.into_inner()
        })
    }
}

/// 从 `Cookie` 头里取出指定名字的值。
///
/// @intent 只做最小的 RFC 6265 解析（分号分隔的 `name=value`，两侧空白可忽略）：
///         引号包裹值与 `$` 前缀属性在本场景不会出现，为一个用不上的分支引入
///         依赖或复杂解析不划算。畸形片段**跳过**而不是整条失败——
///         浏览器可能同时带上本域其它 Cookie，它们不该影响本服务的鉴权。
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.trim() == name {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// 构造会话 Cookie 的 `Set-Cookie` 值。
///
/// @intent 三个属性各有明确职责，缺一不可：
///         - `HttpOnly`：脚本读不到会话 id，XSS 无法直接偷走它；
///         - `SameSite=Strict`：跨站请求不携带该 Cookie，这是 CSRF 的**主防线**；
///         - `Path=/`：控制台与 `/api/v1` 都要带上它。
///
/// @intent `Secure` 只在确认走 HTTPS 时添加。自托管的常见形态是
///         `http://localhost:6789`，此时加了 `Secure` 会让浏览器**丢弃**该 Cookie，
///         表现为「登录成功但立刻又变回未登录」。判断依据取反向代理约定的
///         `X-Forwarded-Proto`（Caddy 默认发送）。
///         这一点值得写下来：它不影响安全边界（`SameSite=Strict` 与 `HttpOnly` 仍生效），
///         但漏掉会导致公网部署下的会话可被明文链路窃听。
pub fn build_session_cookie(id: &str, max_age: Duration, secure: bool) -> String {
    let mut cookie = format!(
        "{COOKIE_NAME}={id}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        max_age.as_secs()
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// 构造「立即失效」的会话 Cookie（登出用）。
///
/// @intent 登出必须真的让浏览器丢掉 Cookie：仅删除服务端会话时，浏览器仍会带着
///         那个 id 反复请求，直到收到 401 才罢休，界面上表现为「点了登出还在登录态」。
pub fn expired_session_cookie(secure: bool) -> String {
    build_session_cookie("", Duration::ZERO, secure)
}

/// 把 `Set-Cookie` 挂到响应头上。
///
/// @intent 构造失败的唯一可能是字符串含控制字符，而值全部来自本模块（id 是十六进制、
///         属性是常量），故此处 `expect` 不可达；用 `expect` 而非静默忽略，
///         是为了万一将来有人把用户输入拼进来时立刻炸掉。
pub fn attach_cookie(headers: &mut HeaderMap, cookie: &str) {
    headers.append(
        SET_COOKIE,
        HeaderValue::from_str(cookie).expect("cookie value is built from hex ids and constants"),
    );
}

/// 请求是否经由 HTTPS 抵达（据反向代理约定的 `X-Forwarded-Proto`）。
///
/// @intent 判断「是否为 HTTPS」而不是直接读 URI scheme，是因为 TLS 通常在 Caddy
///         处终止，服务自身只见到明文 HTTP。取不到该头时返回 `false`——
///         宁可不加 `Secure`（Cookie 仍受 `SameSite=Strict` 保护），
///         也不要在 `http://localhost` 下加上它而让登录静默失效。
pub fn request_is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|proto| proto.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// 判断一次「用 Cookie 鉴权的写请求」是否可能来自跨站。
///
/// @intent 这是 CSRF 的**第二道**防线（第一道是 `SameSite=Strict`）。三条判据：
///         1. 没有 `Origin` → 不算跨站。判据刻意宽松，避免误伤：`curl` 带 Cookie 手工
///            调试、或某些代理剥离 `Origin` 时不该被当成攻击。而真正的跨站表单提交，
///            现代浏览器**一定**会带上 `Origin`，所以宽松不会漏掉真实攻击。
///         2. 有 `Origin` 但没有 `Host` → **判为跨站**。这一条是失败关闭（fail-closed）：
///            HTTP/1.1 的请求必定带 `Host`，而浏览器发出的请求更是必然带它；
///            于是「有 Origin 却无 Host」只可能来自畸形或人为构造的请求。
///            若在这里放行，就等于给攻击者留了一条「删掉 Host 头即可绕过」的通道——
///            实测中发现过这个漏洞：`oneshot` 不产生 `Host`，跨站 DELETE 因此被放行。
///         3. 两者都在 → 比较 host，不一致即跨站。
pub fn is_cross_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };

    // `Origin` 形如 `scheme://host[:port]`；取 `//` 之后的部分与 Host 比对，
    // 从而不把 scheme 与端口写法差异误判为跨站。
    let origin_host = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin)
        .trim_end_matches('/');

    !origin_host.eq_ignore_ascii_case(host.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            let name: axum::http::HeaderName = name.parse().unwrap();
            headers.insert(name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    // ------------------------------------------------------------ 会话表

    /// 创建后立刻可查到，且带出用户名。
    #[test]
    fn created_session_is_retrievable() {
        let store = SessionStore::new();
        let id = store.create("admin").unwrap();

        let session = store.get(&id).expect("新建会话应可查到");
        assert_eq!(session.user, "admin");
        assert!(session.expires_at > Instant::now(), "有效期应尚未到");
        assert_eq!(store.count_sessions(), 1, "表里应恰好一条会话");
    }

    /// 不存在的 id 必须查不到（而不是返回某个默认会话）。
    #[test]
    fn unknown_session_id_is_not_found() {
        let store = SessionStore::new();
        store.create("admin").unwrap();

        assert!(store.get("deadbeef").is_none());
        assert!(store.get("").is_none());
    }

    /// 会话 id 必须是 256 bit 随机、不重复，且不可猜测。
    #[test]
    fn session_ids_are_unique_and_full_length() {
        let store = SessionStore::new();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let id = store.create("admin").unwrap();
            assert_eq!(id.len(), auth::TOKEN_BYTES * 2);
            assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(seen.insert(id), "会话 id 出现重复");
        }
    }

    /// 过期会话必须被拒绝，并从表中移除。
    ///
    /// @intent 用「零时长」而非 `sleep` 制造过期：单调时钟保证 `now + 0 <= now` 之后的
    ///         任意时刻都判为过期，因此这条断言是**确定性**的，不会在负载高的机器上偶发失败。
    #[test]
    fn expired_session_is_rejected_and_purged() {
        let store = SessionStore::new();
        let id = store.create_with_ttl("admin", Duration::ZERO).unwrap();

        assert!(store.get(&id).is_none(), "零时长会话应立即过期");
        assert_eq!(store.count_sessions(), 0, "过期条目应在查询时被顺手移除");
    }

    /// 登出必须让会话立即失效：再查即为未登录。
    #[test]
    fn removed_session_can_no_longer_authenticate() {
        let store = SessionStore::new();
        let id = store.create("admin").unwrap();
        assert!(store.get(&id).is_some());

        assert!(store.remove(&id), "首次移除应返回 true");
        assert!(store.get(&id).is_none(), "登出后会话必须不可用");
        assert!(!store.remove(&id), "重复移除应返回 false");
    }

    /// 过期条目不得在表中累积：每次创建会话都应顺手清掉已过期的。
    ///
    /// @intent 这条用例的正确写法是「创建 N 个已过期会话，断言表中不累积」，
    ///         而**不能**写成「先塞满过期条目，再创建一条清除它们」——后者在当前实现下
    ///         观察不到：`create_with_ttl` 在插入前就 `retain` 了一次，
    ///         已过期的条目根本活不到下一次创建。测试必须匹配实现里真实存在的
    ///         可观测行为，否则断言的是一个永远不会出现的中间状态。
    #[test]
    fn expired_entries_do_not_accumulate() {
        let store = SessionStore::new();
        for _ in 0..10 {
            store.create_with_ttl("admin", Duration::ZERO).unwrap();
        }
        assert_eq!(
            store.count_sessions(),
            1,
            "零时长会话每次创建即过期并在下次创建时被清掉，表中只应剩最后一条"
        );

        // 长期会话（默认 TTL）则正常累积，说明清理只针对已过期条目。
        for _ in 0..5 {
            store.create("admin").unwrap();
        }
        assert_eq!(store.count_sessions(), 5, "未过期的会话应全部保留");
    }

    /// 多个会话可以并存（同一账号在两台浏览器上登录）。
    #[test]
    fn multiple_sessions_coexist() {
        let store = SessionStore::new();
        let a = store.create("admin").unwrap();
        let b = store.create("admin").unwrap();

        assert_ne!(a, b);
        assert!(store.get(&a).is_some());
        assert!(store.get(&b).is_some());

        store.remove(&a);
        assert!(store.get(&a).is_none());
        assert!(store.get(&b).is_some(), "登出其中一个不应影响另一个");
    }

    // ------------------------------------------------------------ Cookie

    /// 正常与畸形的 Cookie 头都要能解析出正确结果。
    #[test]
    fn cookie_value_parses_and_skips_malformed_segments() {
        let headers = headers_with(&[("cookie", "other=1; memora_session=abc123; theme=dark")]);
        assert_eq!(cookie_value(&headers, COOKIE_NAME).as_deref(), Some("abc123"));

        // 前后空白、多余分号、无值片段都不得影响目标项
        let messy = headers_with(&[("cookie", " ; ; memora_session = xyz ; ;; break")]);
        assert_eq!(cookie_value(&messy, COOKIE_NAME).as_deref(), Some("xyz"));

        // 空值等同于未携带，避免把「空 id」拿去查表
        let empty = headers_with(&[("cookie", "memora_session=")]);
        assert_eq!(cookie_value(&empty, COOKIE_NAME), None);

        // 无 Cookie 头
        assert_eq!(cookie_value(&HeaderMap::new(), COOKIE_NAME), None);
    }

    /// 相似名字不得被误命中（`memora_session_x` 不是 `memora_session`）。
    #[test]
    fn cookie_lookup_matches_the_exact_name() {
        let headers = headers_with(&[("cookie", "memora_session_backup=nope; xs=1")]);
        assert_eq!(cookie_value(&headers, COOKIE_NAME), None);
    }

    /// 会话 Cookie 的三个属性缺一不可。
    #[test]
    fn session_cookie_carries_the_required_attributes() {
        let cookie = build_session_cookie("abc", SESSION_TTL, false);

        assert!(cookie.starts_with("memora_session=abc"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"), "必须禁止脚本读取");
        assert!(cookie.contains("SameSite=Strict"), "必须是 CSRF 主防线");
        assert!(cookie.contains("Max-Age=43200"));
        assert!(
            !cookie.contains("Secure"),
            "明文链路下不得加 Secure，否则浏览器会丢弃该 Cookie 并表现为登录无效"
        );

        assert!(build_session_cookie("abc", SESSION_TTL, true).contains("; Secure"));
    }

    /// 登出 Cookie 必须立即失效。
    #[test]
    fn logout_cookie_expires_immediately() {
        let cookie = expired_session_cookie(false);
        assert!(cookie.contains("memora_session="));
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("HttpOnly"));
    }

    /// `X-Forwarded-Proto` 的识别：只认 https（大小写不敏感），其余一律视为明文。
    #[test]
    fn secure_detection_follows_the_forwarded_proto() {
        assert!(request_is_secure(&headers_with(&[(
            "x-forwarded-proto",
            "https"
        )])));
        assert!(request_is_secure(&headers_with(&[(
            "x-forwarded-proto",
            "HTTPS"
        )])));
        assert!(!request_is_secure(&headers_with(&[(
            "x-forwarded-proto",
            "http"
        )])));
        assert!(
            !request_is_secure(&HeaderMap::new()),
            "缺少该头时必须保守地按明文处理"
        );
    }

    // ------------------------------------------------------------ CSRF

    /// 跨站判定：同源放行，异源拦截，缺头不拦截。
    #[test]
    fn cross_origin_detection() {
        // 同源：scheme 与端口写法差异不算跨站
        assert!(!is_cross_origin(&headers_with(&[
            ("origin", "http://localhost:6789"),
            ("host", "localhost:6789"),
        ])));
        assert!(!is_cross_origin(&headers_with(&[
            ("origin", "https://mem.example.com"),
            ("host", "mem.example.com"),
        ])));
        assert!(!is_cross_origin(&headers_with(&[
            ("origin", "http://localhost"),
            ("host", "localhost"),
        ])));

        // 异源：真实攻击形态
        assert!(is_cross_origin(&headers_with(&[
            ("origin", "https://evil.example.com"),
            ("host", "localhost:6789"),
        ])));
        assert!(
            is_cross_origin(&headers_with(&[
                ("origin", "http://127.0.0.1:6789"),
                ("host", "localhost:6789"),
            ])),
            "端口相同但主机不同也属跨源"
        );

        // 缺 Origin：按「非跨站」处理，交由 SameSite=Strict 兜底
        assert!(
            !is_cross_origin(&headers_with(&[("host", "localhost")])),
            "没有 Origin 的请求不判为跨站"
        );
        assert!(!is_cross_origin(&HeaderMap::new()));

        // 有 Origin 但缺 Host：失败关闭，判为跨站。
        // 这条是实测中发现漏洞后补的：`oneshot` 不产生 Host，若此处放行，
        // 跨站 DELETE 会被放过（HTTP/1.1 与浏览器请求都必带 Host，故无正常场景）。
        assert!(
            is_cross_origin(&headers_with(&[("origin", "https://mem.example.com")])),
            "有 Origin 却无 Host 必须判为跨站（失败关闭）"
        );
    }
}
