//! 实例记录与进程存活探测（原 `pidfile.rs`）。
//!
//! @author yujinping
//! @intent 自守护形态必须回答四个问题——「谁在跑」「还活着吗」「监听哪个端口」
//!         「怎么让它退」。原先只落一个裸 pid，于是 `status` 只能拿**重新推导的配置**
//!         去探活；一旦配置文件被改过而进程没重启，就会探错端口并报「unreachable」，
//!         把排障引向错误方向。故记录升级为 JSON **实例记录**：
//!         `pid` + `port` + `data_dir` + `started_at` + `version`，
//!         使 `status` / `stop` 以运行中实例的**实际事实**作答。
//!
//! @intent 记录锚在 `DATA_DIR` 下，与进程身份同生命周期：迁移或备份数据目录时
//!         控制面随之搬迁，不会留下指向旧机器的记录。
//!
//! @intent 兼容旧的裸整数 pid 文件（`port` 记为 `None`，由调用方回退到配置端口）：
//!         升级后残留的旧文件若被直接拒绝，`start` 会变成硬错误，
//!         对「原地升级」的用户不友好；而给它填一个默认端口则是静默撒谎。
//!
//! @intent 存活探测使用 `kill(pid, 0)` 而非 `/proc` 或 sysinfo 库：macOS 没有 `/proc`，
//!         而 `kill` 是两端都具备的 POSIX 原语且无需重依赖。**`EPERM` 必须判为存活**——
//!         它表示进程存在但不属于当前用户；若判为死亡，`start` 会覆盖仍在运行的服务
//!         的记录，此后 `stop` 将指向错误的进程。
//!
//! @intent `started_at` 自行按 RFC3339 渲染（`civil_from_days`）：为一行时间戳引入
//!         `chrono` / `time` 不划算（本项目「能不加依赖就不加」），
//!         而该换算是公历算法中的标准写法，可用已知时刻单测钉死。

use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 实例记录文件名（位于 `DATA_DIR` 下）。沿用旧名 `memora.pid`，升级后原地复用。
pub const INSTANCE_FILE_NAME: &str = "memora.pid";
/// 后台日志文件名（位于 `DATA_DIR` 下）。
pub const LOG_FILE_NAME: &str = "memora.log";
/// 退出码：服务未在运行（`status` / `stop` 使用，遵循 LSB init 脚本惯例）。
pub const EXIT_NOT_RUNNING: i32 = 3;

/// 运行中实例的事实记录。
///
/// @intent `port` 为 `Option`：旧版裸 pid 文件无法得知端口，此时显式记为「未知」
///         并让调用方回退到配置端口，比填一个默认 6789 更诚实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceRecord {
    /// 进程号
    pub pid: i32,
    /// 实际监听的端口；`None` 表示记录未包含（旧格式）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// 实际使用的数据目录
    #[serde(default)]
    pub data_dir: String,
    /// 启动时刻（RFC3339 / UTC）
    #[serde(default)]
    pub started_at: String,
    /// 启动该实例的二进制版本
    #[serde(default)]
    pub version: String,
}

/// `DATA_DIR` 下的实例记录文件。
#[derive(Debug, Clone)]
pub struct InstanceFile {
    /// 文件绝对/相对路径
    path: PathBuf,
    /// 所属数据目录（旧格式缺 `data_dir` 时用它补齐）
    data_dir: PathBuf,
}

impl InstanceFile {
    /// 以数据目录为锚点构造。
    pub fn new(data_dir: &Path) -> Self {
        InstanceFile {
            path: data_dir.join(INSTANCE_FILE_NAME),
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// 后台日志路径（同样锚在数据目录下，与实例记录同生命周期）。
    pub fn log_path(data_dir: &Path) -> PathBuf {
        data_dir.join(LOG_FILE_NAME)
    }

    /// 实例记录路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取实例记录。
    ///
    /// @intent 返回 `Option` 而非布尔：调用方需要记录本身来取 pid 与端口。内容损坏时报错
    ///         而非视为「未运行」——否则会直接覆盖一个可能仍在运行的服务。
    pub fn read(&self) -> io::Result<Option<InstanceRecord>> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let trimmed = raw.trim();

        // 现行格式：JSON 对象
        if let Ok(record) = serde_json::from_str::<InstanceRecord>(trimmed) {
            if record.pid > 0 {
                return Ok(Some(record));
            }
            return Err(self.malformed(trimmed, "pid must be a positive integer"));
        }

        // 旧版格式：裸整数 pid（无端口信息）
        if let Ok(pid) = trimmed.parse::<i32>() {
            if pid > 0 {
                return Ok(Some(InstanceRecord {
                    pid,
                    port: None,
                    data_dir: self.data_dir.display().to_string(),
                    started_at: String::new(),
                    version: String::new(),
                }));
            }
        }

        Err(self.malformed(trimmed, "expected a JSON instance record or a bare pid"))
    }

    /// 写入实例记录（原子替换 + 权限 `0600`）。
    pub fn write(&self, record: &InstanceRecord) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(record)
            .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))?;

        // 同目录内的 rename 是原子的（POSIX 保证），故读者永远看到完整内容。
        // 权限在创建时即收紧为 0600（记录里可能含数据目录等部署信息），
        // 且 `rename` 保留源文件权限，故替换后仍是 0600。
        let tmp = self.path.with_extension("pid.tmp");
        {
            let mut options = fs::OpenOptions::new();
            options.create(true).write(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                // Windows 无 POSIX 权限位；私有性由用户目录 ACL 承担，此处只收紧 Unix 侧。
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(body.as_bytes())?;
            writeln!(file)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)
    }

    /// 删除实例记录；文件本就不存在时视为成功（幂等）。
    pub fn remove(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// 返回仍在运行的实例记录；若记录陈旧（进程已退出）则顺手清理并返回 `None`。
    ///
    /// @intent 把「读 + 判活 + 清理」收敛为一处，使 `start` / `stop` / `status` 三个
    ///         入口对「陈旧记录」的处理必然一致，不会各自实现出不同语义。
    pub fn live(&self) -> io::Result<Option<InstanceRecord>> {
        match self.read()? {
            None => Ok(None),
            Some(record) if is_alive(record.pid) => Ok(Some(record)),
            Some(_) => {
                self.remove()?;
                Ok(None)
            }
        }
    }

    /// 构造「内容损坏」错误。
    fn malformed(&self, raw: &str, expected: &str) -> io::Error {
        io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "instance file {} is malformed (found {raw:?}, {expected}); \
                 remove it manually if no memora process is running",
                self.path.display()
            ),
        )
    }
}

/// 进程是否存活。
///
/// @intent `EPERM` 视为存活（见模块文档）；`ESRCH` 视为不存在；其余错误保守判为存活，
///         宁可让 `start` 报「已在运行」也不冒覆盖他人记录的风险。
#[cfg(unix)]
pub fn is_alive(pid: i32) -> bool {
    // pid <= 0 有特殊含义：0 表示「当前进程组」，负数表示整个进程组。
    // 用作探测会波及无关进程，故直接判为「不存活」。
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` 是纯 FFI 调用，参数均为整型，无指针与生命周期问题。
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => false,
        // EPERM = 进程存在但无权操作，属于「活着但不归我管」
        _ => true,
    }
}

/// Windows 版探活：`OpenProcess` + `GetExitCodeProcess`（退出码 `STILL_ACTIVE` 即存活）。
///
/// @intent 打开失败（进程不存在 / 权限不足）一律判不存活：同用户自托管场景下
///         「进程存在但打不开」几乎不存在，简单判死即可让陈旧记录被及时清理。
#[cfg(windows)]
pub fn is_alive(pid: i32) -> bool {
    // pid <= 0 有特殊含义（与 Unix 一致），直接判为「不存活」。
    if pid <= 0 {
        return false;
    }
    let handle = unsafe {
        crate::process::OpenProcess(
            crate::process::PROCESS_QUERY_LIMITED_INFORMATION,
            crate::process::FALSE,
            pid as u32,
        )
    };
    if handle.is_null() {
        return false;
    }
    let mut exit_code: u32 = 0;
    let ok = unsafe { crate::process::GetExitCodeProcess(handle, &mut exit_code) };
    let alive = ok != crate::process::FALSE && exit_code == crate::process::STILL_ACTIVE;
    unsafe { crate::process::CloseHandle(handle) };
    alive
}

/// 发送 SIGTERM（请求优雅退出）。
#[cfg(unix)]
pub fn terminate(pid: i32) -> io::Result<()> {
    send_signal(pid, libc::SIGTERM)
}

/// 发送 SIGKILL（强杀；仅在 SIGTERM 超时后使用）。
#[cfg(unix)]
pub fn force_kill(pid: i32) -> io::Result<()> {
    send_signal(pid, libc::SIGKILL)
}

/// Windows 版进程终止。
///
/// @intent Windows 没有可移植的「优雅退出」信号（WM_CLOSE 只对 GUI 窗口有效），
///         `TerminateProcess` 是唯一通用手段，语义上接近 SIGKILL。因此 terminate 与
///         force_kill 在 Windows 上都走同一实现；需要干净退出时由进程自身配合退出通道。
#[cfg(windows)]
pub fn terminate(pid: i32) -> io::Result<()> {
    send_signal(pid, 0)
}

#[cfg(windows)]
pub fn force_kill(pid: i32) -> io::Result<()> {
    send_signal(pid, 0)
}

/// 当前时刻的 RFC3339 表示（UTC，秒级精度）。
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    rfc3339_from_unix(secs)
}

/// Unix 秒 → RFC3339（UTC）。
pub fn rfc3339_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// 自 Unix 元年起的天数 → 公历年月日（Howard Hinnant 的 `civil_from_days`）。
///
/// @intent 该换算用整数算术表达「闰年 400 年一循环」的历法规律，无查表、无分支特例，
///         故可直接用已知时刻单测钉死（如 `1_000_000_000` → 2001-09-09T01:46:40Z）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // 把纪元起点从 1970-01-01 移到 0000-03-01，使闰日落在年末，简化后续取模
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let month_index = (5 * day_of_year + 2) / 153; // [0, 11]，3 月为 0
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32; // [1, 31]
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    // 1、2 月被移到了「年末」，故年份要加回一年
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// 向指定进程发送信号。
#[cfg(unix)]
fn send_signal(pid: i32, signal: i32) -> io::Result<()> {
    if pid <= 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("refusing to signal pid {pid}: only positive pids are accepted"),
        ));
    }
    // SAFETY: 同 `is_alive`，纯 FFI 调用。
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Windows 版：`TerminateProcess`（无信号语义，`signal` 参数仅为占位）。
#[cfg(windows)]
fn send_signal(pid: i32, _signal: i32) -> io::Result<()> {
    if pid <= 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("refusing to signal pid {pid}: only positive pids are accepted"),
        ));
    }
    let handle = unsafe {
        crate::process::OpenProcess(crate::process::PROCESS_TERMINATE, crate::process::FALSE, pid as u32)
    };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let ok = unsafe { crate::process::TerminateProcess(handle, 1) };
    unsafe { crate::process::CloseHandle(handle) };
    if ok != crate::process::FALSE {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// 每个用例独立的临时数据目录。
    fn temp_data_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("memora_instance_{}_{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 一条完整的示例记录。
    fn sample(pid: i32, port: u16) -> InstanceRecord {
        InstanceRecord {
            pid,
            port: Some(port),
            data_dir: "/var/lib/memora".to_string(),
            started_at: "2026-09-17T08:30:00Z".to_string(),
            version: "0.1.0".to_string(),
        }
    }

    #[test]
    fn record_round_trips() {
        let dir = temp_data_dir();
        let file = InstanceFile::new(&dir);
        assert_eq!(file.read().unwrap(), None, "初始无实例记录");

        let record = sample(4321, 6789);
        file.write(&record).unwrap();

        assert_eq!(file.read().unwrap(), Some(record));
        assert!(file.path().ends_with(INSTANCE_FILE_NAME));
    }

    /// 记录含数据目录与端口，不应被同组用户或他人读到。
    #[test]
    #[cfg(unix)]
    fn record_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_data_dir();
        let file = InstanceFile::new(&dir);
        file.write(&sample(1, 6789)).unwrap();

        let mode = fs::metadata(file.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "实例记录权限应为 0600，实际 {mode:o}");
    }

    /// 旧格式（裸整数 pid）必须被接受：否则原地升级的用户会撞上「start 直接失败」。
    #[test]
    fn legacy_bare_pid_is_accepted() {
        let dir = temp_data_dir();
        let file = InstanceFile::new(&dir);
        fs::write(file.path(), "4321\n").unwrap();

        let record = file.read().unwrap().expect("旧格式应被接受");
        assert_eq!(record.pid, 4321);
        assert_eq!(
            record.port, None,
            "旧格式无端口信息，必须是「未知」而非默认值"
        );
        assert_eq!(
            record.data_dir,
            dir.display().to_string(),
            "旧格式缺 data_dir，应用记录所在目录补齐"
        );
    }

    /// 内容损坏必须报错而非静默当作「未运行」——否则会覆盖仍在运行的服务。
    #[test]
    fn malformed_content_is_an_error() {
        for raw in [
            "",
            "   \n",
            "abc",
            "-1",
            "12x",
            "99999999999999999999",
            "{",
            "{}",
            r#"{"pid": 0}"#,
            r#"{"pid": -5}"#,
            r#"{"port": 6789}"#,
        ] {
            let dir = temp_data_dir();
            let file = InstanceFile::new(&dir);
            fs::write(file.path(), raw).unwrap();
            assert!(file.read().is_err(), "记录内容 {raw:?} 应被判为损坏并报错");
            assert!(file.live().is_err(), "损坏的记录不得被当作未运行处理");
        }
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = temp_data_dir();
        let file = InstanceFile::new(&dir);
        file.write(&sample(1, 6789)).unwrap();

        file.remove().unwrap();
        assert_eq!(file.read().unwrap(), None);
        // 重复删除不报错，便于 stop 流程无脑收尾
        file.remove().unwrap();
        assert!(!file.path().exists());
    }

    #[test]
    fn current_process_is_alive() {
        assert!(is_alive(std::process::id() as i32));
    }

    /// 超出内核 pid 上限的值必然不存在（Linux pid_max 默认 4194304，macOS 为 99998）。
    #[test]
    fn impossible_pid_is_not_alive() {
        assert!(!is_alive(i32::MAX));
    }

    #[test]
    fn stale_record_is_cleaned_up() {
        let dir = temp_data_dir();
        let file = InstanceFile::new(&dir);
        file.write(&sample(i32::MAX, 6789)).unwrap();

        assert_eq!(file.live().unwrap(), None);
        assert!(!file.path().exists(), "陈旧记录应在判定后被清理");
    }

    #[test]
    fn live_reports_running_process() {
        let dir = temp_data_dir();
        let file = InstanceFile::new(&dir);
        let record = sample(std::process::id() as i32, 6799);
        file.write(&record).unwrap();

        assert_eq!(file.live().unwrap(), Some(record));
        assert!(file.path().exists(), "存活进程的记录不得被清理");
    }

    /// `save` 覆盖旧内容后不得残留旧 pid（原子替换 + truncate 的组合）。
    #[test]
    fn write_replaces_previous_record() {
        let dir = temp_data_dir();
        let file = InstanceFile::new(&dir);
        file.write(&sample(1111, 6789)).unwrap();
        file.write(&sample(2222, 7000)).unwrap();

        let record = file.read().unwrap().unwrap();
        assert_eq!(record.pid, 2222);
        assert_eq!(record.port, Some(7000));
    }

    #[test]
    fn log_path_lives_beside_the_record() {
        let dir = temp_data_dir();
        assert_eq!(InstanceFile::log_path(&dir), dir.join(LOG_FILE_NAME));
    }

    /// `stop` 的真实语义：SIGTERM 必须能终止子进程，且进程随即不再被判定为存活。
    #[test]
    #[cfg(unix)]
    fn terminate_actually_stops_a_child_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("测试依赖 POSIX `sleep`");
        let child_pid = child.id() as i32;
        assert!(is_alive(child_pid));

        terminate(child_pid).unwrap();
        let status = child.wait().unwrap();

        assert!(!status.success(), "被 SIGTERM 终止的进程不应返回成功");
        assert!(!is_alive(child_pid));
    }

    #[test]
    fn signalling_a_nonexistent_process_is_an_error() {
        assert!(terminate(i32::MAX).is_err());
        assert!(force_kill(i32::MAX).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn force_kill_stops_a_child_that_ignores_sigterm() {
        // `sh` 捕获并忽略 TERM，模拟不响应优雅退出的进程
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 30")
            .spawn()
            .expect("测试依赖 POSIX `sh`");
        let child_pid = child.id() as i32;

        terminate(child_pid).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(is_alive(child_pid), "忽略 SIGTERM 的进程应仍然存活");

        force_kill(child_pid).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    /// 时间戳换算用已知时刻钉死：写错历法不会在本机表现为「看起来正常」。
    #[test]
    fn rfc3339_conversion_matches_known_instants() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(rfc3339_from_unix(1_700_000_000), "2023-11-14T22:13:20Z");
        // 闰日：2020-02-29
        assert_eq!(rfc3339_from_unix(1_582_934_400), "2020-02-29T00:00:00Z");
        // 非闰年的 3 月 1 日，紧邻上面那一天的下一年
        assert_eq!(rfc3339_from_unix(1_614_556_800), "2021-03-01T00:00:00Z");
        // 纪元之前（闰年规则中的负年分支）
        assert_eq!(rfc3339_from_unix(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn now_is_a_plausible_rfc3339() {
        let now = now_rfc3339();
        assert_eq!(now.len(), 20, "形如 2026-09-17T08:30:00Z，实际 {now}");
        assert!(now.ends_with('Z'));
        assert!(now.starts_with("20"), "本机时钟应在 21 世纪：{now}");
    }
}
