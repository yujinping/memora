//! PID 文件与进程存活探测。
//!
//! @author yujinping
//! @intent P4.2：自守护形态必须回答三个问题——「谁在跑」「还活着吗」「怎么让它退」。
//!         把 PID 文件锚在 `DATA_DIR` 下，使进程身份与数据同生命周期：迁移或备份
//!         数据目录时控制面随之搬迁，不会留下指向旧机器的 PID 文件。
//!
//! @intent 存活探测使用 `kill(pid, 0)` 而非 `/proc` 或 sysinfo 库：macOS 没有 `/proc`，
//!         而 `kill` 是两端都具备的 POSIX 原语且无需重依赖。**`EPERM` 必须判为存活**——
//!         它表示进程存在但不属于当前用户；若判为死亡，`start` 会覆盖仍在运行的服务
//!         的 PID 文件，此后 `stop` 将指向错误的进程。

use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

/// PID 文件名（位于 `DATA_DIR` 下）。
pub const PID_FILE_NAME: &str = "memora.pid";
/// 后台日志文件名（位于 `DATA_DIR` 下）。
pub const LOG_FILE_NAME: &str = "memora.log";
/// 退出码：服务未在运行（`status` / `stop` 使用，遵循 LSB init 脚本惯例）。
pub const EXIT_NOT_RUNNING: i32 = 3;

/// `DATA_DIR` 下的 PID 文件。
#[derive(Debug, Clone)]
pub struct PidFile {
    /// 文件绝对/相对路径
    path: PathBuf,
}

impl PidFile {
    /// 以数据目录为锚点构造。
    pub fn new(data_dir: &Path) -> Self {
        PidFile {
            path: data_dir.join(PID_FILE_NAME),
        }
    }

    /// 后台日志路径（同样锚在数据目录下，与 PID 文件同生命周期）。
    pub fn log_path(data_dir: &Path) -> PathBuf {
        data_dir.join(LOG_FILE_NAME)
    }

    /// PID 文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取 PID。
    ///
    /// @intent 返回 `Option` 而非布尔：调用方需要 PID 本身来发信号。内容损坏时报错
    ///         而非视为「未运行」——否则会直接覆盖一个可能仍在运行的服务。
    pub fn read(&self) -> io::Result<Option<i32>> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => {
                let trimmed = raw.trim();
                match trimmed.parse::<i32>() {
                    Ok(pid) if pid > 0 => Ok(Some(pid)),
                    _ => Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "PID file {} is malformed: {trimmed:?}; remove it manually if no \
                             memora process is running",
                            self.path.display()
                        ),
                    )),
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// 写入 PID（原子替换：先写临时文件再 rename，避免读到半截内容）。
    pub fn write(&self, pid: u32) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        // 同目录内的 rename 是原子的（POSIX 保证），故读者永远看到完整内容
        let tmp = self.path.with_extension("pid.tmp");
        {
            let mut file = fs::File::create(&tmp)?;
            writeln!(file, "{pid}")?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)
    }

    /// 删除 PID 文件；文件本就不存在时视为成功（幂等）。
    pub fn remove(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// 返回仍在运行的 PID；若文件陈旧（进程已退出）则顺手清理并返回 `None`。
    ///
    /// @intent 把「读 + 判活 + 清理」收敛为一处，使 `start` / `stop` / `status` 三个
    ///         入口对「陈旧 PID 文件」的处理必然一致，不会各自实现出不同语义。
    pub fn live_pid(&self) -> io::Result<Option<i32>> {
        match self.read()? {
            None => Ok(None),
            Some(pid) if is_alive(pid) => Ok(Some(pid)),
            Some(_) => {
                self.remove()?;
                Ok(None)
            }
        }
    }
}

/// 进程是否存活。
///
/// @intent `EPERM` 视为存活（见模块文档）；`ESRCH` 视为不存在；其余错误保守判为存活，
///         宁可让 `start` 报「已在运行」也不冒覆盖他人 PID 的风险。
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

/// 发送 SIGTERM（请求优雅退出）。
pub fn terminate(pid: i32) -> io::Result<()> {
    send_signal(pid, libc::SIGTERM)
}

/// 发送 SIGKILL（强杀；仅在 SIGTERM 超时后使用）。
pub fn force_kill(pid: i32) -> io::Result<()> {
    send_signal(pid, libc::SIGKILL)
}

/// 向指定进程发送信号。
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// 每个用例独立的临时数据目录。
    fn temp_data_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("memora_p4_pid_{}_{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = temp_data_dir();
        let pid = PidFile::new(&dir);
        assert_eq!(pid.read().unwrap(), None, "初始无 PID 文件");

        pid.write(4321).unwrap();
        assert_eq!(pid.read().unwrap(), Some(4321));
        assert!(pid.path().ends_with(PID_FILE_NAME));
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = temp_data_dir();
        let pid = PidFile::new(&dir);
        pid.write(1).unwrap();

        pid.remove().unwrap();
        assert_eq!(pid.read().unwrap(), None);
        // 重复删除不报错，便于 stop 流程无脑收尾
        pid.remove().unwrap();
        assert!(!pid.path().exists());
    }

    /// 内容损坏必须报错而非静默当作「未运行」——否则会覆盖仍在运行的服务。
    #[test]
    fn malformed_content_is_an_error() {
        for raw in ["", "abc", "-1", "12x", "99999999999999999999"] {
            let dir = temp_data_dir();
            let pid = PidFile::new(&dir);
            fs::write(pid.path(), raw).unwrap();
            assert!(
                pid.read().is_err(),
                "PID 内容 `{raw}` 应被判为损坏并报错"
            );
            assert!(
                pid.live_pid().is_err(),
                "损坏的 PID 文件不得被当作未运行处理"
            );
        }
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
    fn stale_pid_file_is_cleaned_up() {
        let dir = temp_data_dir();
        let pid = PidFile::new(&dir);
        pid.write(i32::MAX as u32).unwrap();

        assert_eq!(pid.live_pid().unwrap(), None);
        assert!(!pid.path().exists(), "陈旧 PID 文件应在判定后被清理");
    }

    #[test]
    fn live_pid_reports_running_process() {
        let dir = temp_data_dir();
        let pid = PidFile::new(&dir);
        pid.write(std::process::id()).unwrap();

        assert_eq!(pid.live_pid().unwrap(), Some(std::process::id() as i32));
        assert!(pid.path().exists(), "存活进程的 PID 文件不得被清理");
    }

    /// `stop` 的真实语义：SIGTERM 必须能终止子进程，且进程随即不再被判定为存活。
    #[test]
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

    #[test]
    fn log_path_lives_beside_the_pid_file() {
        let dir = temp_data_dir();
        assert_eq!(PidFile::log_path(&dir), dir.join(LOG_FILE_NAME));
    }
}
