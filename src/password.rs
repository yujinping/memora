//! 管理员口令：哈希、校验与随机口令生成。
//!
//! @author yujinping
//! @intent 背景：管理面此前只有 `admin_token` 一条凭据。token 是 64 位十六进制串，
//!         给脚本用没问题，给人用则是「从终端抄到浏览器」的负担。本模块为网页控制台
//!         引入**用户名 + 口令**这一人类可用的凭据，而口令绝不能以明文落盘。
//!
//! @intent 选 Argon2id 而非 PBKDF2 / bcrypt：它是 memory-hard 的，显著抬高 GPU 与
//!         ASIC 离线爆破的成本；且 PHC 字符串把算法与参数（m / t / p）内联在哈希里，
//!         将来提升强度时旧哈希仍可校验，不必改配置结构或做数据迁移。
//!         依赖侧只取 `alloc + getrandom + password-hash`，仍遵循「只取系统熵、
//!         不引入 rand 生态」的项目约定。
//!
//! @intent 参数不自行调参，直接用 argon2 的 `Params::DEFAULT`（m=19456 KiB、t=2、p=1）。
//!         自行调参的常见后果是把强度调到 OWASP 基线以下却毫无察觉；默认值即基线。
//!         代价是单次校验约需 19 MiB 内存与数十毫秒，而登录是低频人工操作，可接受。
//!
//! 不做的事：本模块**不含**会话、Cookie 与任何 HTTP 概念——那属于 [`crate::session`]。

use anyhow::{bail, Context, Result};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};

/// 口令的最小长度。
///
/// @intent 只对**人工指定**的口令设下限（`init --admin-password`），随机生成的口令
///         远超此值。下限存在的意义是拦住 `--admin-password 123` 这类输入：
///         它不会导致启动失败，只会在某天被爆破，属于「能启动但危险」的配置。
pub const MIN_LENGTH: usize = 8;

/// 生成随机口令时使用的字符集。
///
/// @intent 刻意剔除字形易混的 `0/O/o` 与 `1/l/I`：随机口令的典型用法是**从终端
///         抄进浏览器**，而 `l` 与 `1` 在等宽字体下几乎不可分辨，抄错后得到的只是
///         一次费解的 401。字符集大小 56 也刻意留出「可按字节拒绝采样」的余量
///         （见 [`generate_password`]）。
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";

/// 随机口令的默认长度。
///
/// @intent 20 个字符、字符集 56 约合 116 bit 熵，远超任何在线爆破的可行范围；
///         同时短到能一次抄完。
pub const GENERATED_LENGTH: usize = 20;

/// 计算口令的 PHC 哈希串（形如 `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`）。
///
/// @intent 盐由 argon2 内部经 `getrandom` 生成，本模块不自行取熵——盐的长度与编码
///         由算法库决定，自己生成只会多一处可能与库约定不符的地方。
///         空口令一律拒绝：`hash_password("")` 会产出一个「任何人可登录」的哈希，
///         而这种配置不会让任何检查失败，只会在某天被登录进来。
pub fn hash_password(password: &str) -> Result<String> {
    if password.is_empty() {
        bail!("refusing to hash an empty password");
    }
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .context("failed to hash the password")
}

/// 校验口令是否与 PHC 哈希串匹配。
///
/// @intent 一律返回 `bool` 而不返回 `Result`：调用方（登录接口）对「口令错」与
///         「哈希串损坏」必须给出**同一个** 401，否则错误响应本身就泄漏了
///         「这个用户名存在且已配置口令」这一信息。哈希串损坏只记日志。
///         比较由 argon2 内部以常量时间完成。
pub fn verify_password(password: &str, phc: &str) -> bool {
    let parsed = match PasswordHash::new(phc.trim()) {
        Ok(parsed) => parsed,
        Err(err) => {
            // 空串是「未配置」的正常情形，不必刷日志；其余属配置损坏，需要可见。
            if !phc.trim().is_empty() {
                tracing::warn!(error = %err, "stored admin password hash is malformed");
            }
            return false;
        }
    };

    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => true,
        Err(err) => {
            tracing::debug!(error = %err, "password verification failed");
            false
        }
    }
}

/// 该哈希串是否构成一份可用的口令配置。
pub fn is_configured(phc: &str) -> bool {
    !phc.trim().is_empty()
}

/// 生成指定长度的随机口令。
///
/// @intent 随机源与采样刻意分离：熵由 `getrandom` 取，映射由 [`sample`] 做。
///         这样「映射是否无偏」可以脱离随机源被**确定性**地验证，而不是靠统计断言
///         ——统计断言对 ±9% 这一量级的偏斜只有约 2σ 的判别力，会随机翻绿。
pub fn generate_password(length: usize) -> Result<String> {
    if length == 0 {
        bail!("password length must be greater than zero");
    }

    let mut out = String::with_capacity(length);
    while out.len() < length {
        let needed = length - out.len();
        // 每次多取一倍字节：单次采样留下空结果的概率是 0.125^needed，取不到就再取一轮；
        // 这样循环的期望轮数有界（约 1.14 轮），不会出现长时间重试。
        let mut buf = vec![0u8; needed.saturating_mul(2).max(MIN_SAMPLE_BYTES)];
        // getrandom 0.3 的 Error 不实现 std::error::Error（no_std 友好的代价），
        // 故不能直接用 `context`；与 `auth::random_hex` 保持同一种包装方式。
        getrandom::fill(&mut buf)
            .map_err(|e| anyhow::anyhow!("system entropy unavailable: {e}"))?;
        out.push_str(&sample(&buf, needed));
    }
    Ok(out)
}

/// 单轮取样的最小字节数。
const MIN_SAMPLE_BYTES: usize = 16;

/// 对给定字节流做拒绝采样，产出至多 `length` 个字符。
///
/// @intent **两个动作缺一不可**：先丢弃 `>= 224`（即 `4×56`）的字节，再对留下的字节
///         取模映射到下标。只取模不去尾，则前 32 个字符的出现概率是其余字符的 1.25 倍
///         （256 = 4×56 + 32，多出的 32 个字节全落在前 32 个下标上），把 116 bit 的熵
///         悄悄削掉一截；只去尾不取模则会直接越界。
///         抽成接收字节流的纯函数，是为了让这条性质可被精确断言——
///         见 `sample_distributes_all_bytes_exactly_evenly`。
fn sample(bytes: &[u8], length: usize) -> String {
    // 提前返回：`length == 0` 时下面的 `out.len() == length` 永不成立，会一路填满整个
    // 字节流。`generate_password` 已拒绝长度为 0，但本函数是独立的纯函数，须自守边界。
    if length == 0 {
        return String::new();
    }

    let per_index = (u8::MAX as usize + 1) / ALPHABET.len();
    let limit = (per_index * ALPHABET.len()) as u8;

    let mut out = String::with_capacity(length.min(bytes.len()));
    for byte in bytes {
        if *byte < limit {
            out.push(ALPHABET[(*byte as usize) % ALPHABET.len()] as char);
            if out.len() == length {
                break;
            }
        }
    }
    out
}

/// 生成一条人类可读的口令强度提示；满足要求时返回 `None`。
///
/// @intent 只判长度，不判「是否含大小写数字符号」：后者会诱导用户产出 `Passw0rd!`
///         这类可预测口令，而长度才是抵抗离线爆破的主要变量。
pub fn weak_password_reason(password: &str) -> Option<String> {
    if password.is_empty() {
        return Some("password must not be empty".to_string());
    }
    let len = password.chars().count();
    if len < MIN_LENGTH {
        return Some(format!(
            "password is too short ({len} characters); use at least {MIN_LENGTH}"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 哈希与校验必须构成闭环，且错误口令必须失败。
    #[test]
    fn hash_then_verify_roundtrip() {
        let phc = hash_password("correct horse battery staple").unwrap();

        assert!(verify_password("correct horse battery staple", &phc));
        assert!(
            !verify_password("correct horse battery stapl", &phc),
            "差一个字符也必须失败"
        );
        assert!(!verify_password("", &phc));
    }

    /// 同一口令两次哈希必须不同（盐随机），但都能校验通过。
    ///
    /// @intent 这条挡的是「哈希前忘了加盐」——那会让相同口令在配置里呈现为相同串，
    ///         从哈希值即可判断两个实例是否用了同一口令。
    #[test]
    fn hashes_are_salted_and_thus_unique() {
        let a = hash_password("same-password").unwrap();
        let b = hash_password("same-password").unwrap();

        assert_ne!(a, b, "同一口令的两次哈希不应相同");
        assert!(verify_password("same-password", &a));
        assert!(verify_password("same-password", &b));
    }

    /// PHC 串必须自述算法与参数，将来提升强度时旧哈希仍可校验。
    #[test]
    fn phc_string_declares_argon2id_and_parameters() {
        let phc = hash_password("whatever").unwrap();

        assert!(phc.starts_with("$argon2id$"), "应为 Argon2id：{phc}");
        assert!(phc.contains("v=19"), "应含算法版本：{phc}");
        assert!(
            phc.contains("m=19456,t=2,p=1"),
            "应内联 OWASP 基线的默认参数：{phc}"
        );
    }

    /// 损坏或空的哈希串只能判为「不匹配」，绝不能 panic。
    ///
    /// @intent 配置被手工改坏时，panic 会把「登录失败」升级成「服务崩溃」。
    #[test]
    fn malformed_or_empty_hash_is_rejected_without_panicking() {
        for phc in [
            "",
            "   ",
            "not-a-phc-string",
            "$argon2id$v=19$m=19456,t=2,p=1$",
            "$unknown$v=19$m=1,t=1,p=1$c2FsdA$aGFzaA",
        ] {
            assert!(
                !verify_password("anything", phc),
                "非法哈希串 {phc:?} 应判为不匹配"
            );
        }
    }

    /// 空口令不得被哈希。
    #[test]
    fn empty_password_cannot_be_hashed() {
        assert!(hash_password("").is_err());
        assert!(weak_password_reason("").unwrap().contains("empty"));
    }

    /// 非 ASCII 口令必须往返一致（长度按字符计，不按字节）。
    #[test]
    fn unicode_passwords_round_trip() {
        let phc = hash_password("忆庐的管理口令").unwrap();
        assert!(verify_password("忆庐的管理口令", &phc));
        assert!(!verify_password("忆庐的管理口令 ", &phc));

        assert!(
            weak_password_reason("忆庐忆庐忆庐忆庐").is_none(),
            "8 个汉字应满足长度下限"
        );
        assert!(weak_password_reason("忆庐").is_some(), "2 个汉字应被判为过短");
    }

    /// 生成的口令必须落在字符集内、长度准确、且每次不同。
    #[test]
    fn generated_passwords_use_the_safe_alphabet() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            let pw = generate_password(GENERATED_LENGTH).unwrap();
            assert_eq!(pw.len(), GENERATED_LENGTH);
            assert!(
                pw.bytes().all(|b| ALPHABET.contains(&b)),
                "出现字符集之外的字符：{pw}"
            );
            for ambiguous in ['0', 'O', 'o', '1', 'l', 'I'] {
                assert!(
                    !pw.contains(ambiguous),
                    "不得含易混字符 {ambiguous}：{pw}"
                );
            }
            assert!(
                weak_password_reason(&pw).is_none(),
                "生成的口令不应被自己的强度检查判为过弱：{pw}"
            );
            assert!(seen.insert(pw), "生成的口令出现重复");
        }
    }

    /// 长度 1 与 0 的边界：0 必须拒绝，1 必须精确产出 1 个字符。
    #[test]
    fn generated_password_length_boundaries() {
        assert!(generate_password(0).is_err());
        assert_eq!(generate_password(1).unwrap().len(), 1);
    }

    /// 拒绝采样必须**精确**均匀：喂入 0..=255 全部字节值，每个字符的出现次数必须完全相等。
    ///
    /// @intent 这条是 `sample` 的哨兵，且刻意做成确定性断言而非统计断言。
    ///         字符集 56 不整除 256，朴素取模会让下标 0..32 各命中 5 次、其余各命中 4 次
    ///         （5 : 4 的偏斜）；统计断言对 ±9% 这一量级只有约 2σ 判别力，会随机放行，
    ///         而本用例对同一种错误是**必然失败**的。
    ///
    /// @intent 此处请求 256 个字符（而非恰好 224）是刻意的：偏斜只发生在第 224..255 个
    ///         字节上——若只请求 224 个，只消费前 224 个字节，而任意长度 224 的连续区间
    ///         对 56 取模都恰好各 4 次，偏斜看不见，用例会**假绿**（第一版正是这样写的）。
    ///         请求 256 后，两个断言同时约束「确实丢弃了 32 个字节」与「分布精确均匀」。
    #[test]
    fn sample_distributes_all_bytes_exactly_evenly() {
        let bytes: Vec<u8> = (0..=u8::MAX).collect();
        let out = sample(&bytes, 256);
        assert_eq!(
            out.len(),
            224,
            "应恰好接受 224 个字节（0..=223）并丢弃其余 32 个"
        );

        let mut counts = std::collections::HashMap::new();
        for b in out.bytes() {
            *counts.entry(b).or_insert(0usize) += 1;
        }
        assert_eq!(counts.len(), ALPHABET.len(), "应覆盖整个字符集");

        for byte in ALPHABET {
            assert_eq!(
                counts.get(byte).copied().unwrap_or(0),
                4,
                "字符 {} 的出现次数偏离精确值：{counts:?}",
                *byte as char
            );
        }
    }

    /// 超出 `limit` 的字节必须被丢弃而不是被映射到某个下标。
    #[test]
    fn sample_discards_bytes_beyond_the_limit() {
        let rejected: Vec<u8> = (224..=u8::MAX).collect();
        assert_eq!(rejected.len(), 32);
        assert!(
            sample(&rejected, 10).is_empty(),
            ">= 224 的字节应被全部丢弃，不得产出任何字符"
        );

        // 边界：223 是最后一个被接受的字节值
        assert!(!sample(&[223], 1).is_empty(), "223 必须被接受");
        assert!(sample(&[224], 1).is_empty(), "224 必须被拒绝");
    }

    /// 产出数量受 `length` 限制，不会因输入过长而超额。
    #[test]
    fn sample_stops_at_the_requested_length() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(sample(&bytes, 3).len(), 3);
        assert_eq!(sample(&bytes, 0).len(), 0);
        assert_eq!(sample(&[], 5).len(), 0);
    }
}
