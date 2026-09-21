//! 日志：写到 `~/.litepet/logs/daemon.log`，并镜像到 stderr。
//!
//! 双击启动的 GUI 进程没有终端，stderr 会直接丢掉，**文件才是主通道**；
//! 镜像到 stderr 只是为了从终端启动时还能当场看到。
//!
//! 日志文件按大小轮转，只保留一代（`daemon.log.1`），避免长期挂机把磁盘写满。

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::config;

/// 主日志文件名。
const LOG_FILE: &str = "daemon.log";
/// 轮转文件名（只保留一代）。
const LOG_FILE_ROTATED: &str = "daemon.log.1";
/// 单个日志文件的字节上限（5 MiB）。
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// 时间戳格式：本地时间、毫秒精度、带时区偏移，便于 grep 与排序。
const TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f%:z";

/// 进程级 logger 实例。
///
/// 用 `OnceLock` 持有 `&'static` 引用而不是 `set_boxed_logger`：
/// 依赖树里某个传递依赖把 `log` 的 `alloc` feature 关掉了，`set_boxed_logger` 编不出来；
/// 况且 logger 本来就活得跟进程一样长，没必要做堆分配。
static LOGGER: OnceLock<FileLogger> = OnceLock::new();

/// 初始化日志并返回是否真的装上了。
///
/// 进程内只能装一次 logger，重复调用返回 `Ok(false)` 而不是报错——
/// 测试里多次调用 `init()` 不该炸。
pub fn init() -> Result<bool> {
    let dir = config::logs_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("创建日志目录失败：{}", dir.display()))?;
    let logger = LOGGER.get_or_init(|| FileLogger {
        file: RotatingFile::new(dir.join(LOG_FILE), dir.join(LOG_FILE_ROTATED)),
    });
    let installed = log::set_logger(logger).is_ok();
    if installed {
        log::set_max_level(LevelFilter::Info);
    }
    Ok(installed)
}

/// 当前日志文件路径，供启动时提示用户「日志在哪」。
pub fn log_path() -> Result<PathBuf> {
    Ok(config::logs_dir()?.join(LOG_FILE))
}

/// 拼一行日志：`时间 级别 正文`。
///
/// 时间放最前，`sort`/`grep` 都直接可用。
fn format_line(level: Level, body: &str, now: DateTime<Local>) -> String {
    format!("{} {:<5} {}\n", now.format(TIMESTAMP_FORMAT), level, body)
}

/// 写文件 + 镜像 stderr 的 logger。
struct FileLogger {
    /// 轮转写入器。
    file: RotatingFile,
}

impl Log for FileLogger {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format_line(record.level(), &record.args().to_string(), Local::now());
        // 镜像失败（例如 stderr 已关闭）不该影响日志落盘。
        let _ = std::io::stderr().write_all(line.as_bytes());
        self.file.write_line(&line);
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// 单个文件句柄与已写入字节数。
struct Sink {
    /// 打开的文件。
    file: File,
    /// 本代已写入的字节数，用来避免每行都做一次 `stat`。
    written: u64,
}

/// 按大小轮转的文件写入器。
struct RotatingFile {
    /// 当前文件。
    path: PathBuf,
    /// 轮转后的落点。
    rotated: PathBuf,
    /// 当前句柄；`None` 表示尚未打开或已到上限待重开。
    state: Mutex<Option<Sink>>,
}

impl RotatingFile {
    /// 构造（不打开文件，首次写入时才开）。
    fn new(path: PathBuf, rotated: PathBuf) -> Self {
        Self {
            path,
            rotated,
            state: Mutex::new(None),
        }
    }

    /// 追加一行。**任何失败都静默**：日志写不进去不该拖垮宠物。
    fn write_line(&self, line: &str) {
        let Ok(mut guard) = self.state.lock() else {
            // 锁中毒说明别的线程在持锁时 panic 了，此处没必要跟着崩。
            return;
        };
        if guard
            .as_ref()
            .is_some_and(|sink| sink.written >= MAX_LOG_BYTES)
        {
            guard.take();
        }
        if guard.is_none() {
            *guard = Self::open(&self.path, &self.rotated);
        }
        let Some(sink) = guard.as_mut() else {
            return;
        };
        if sink.file.write_all(line.as_bytes()).is_ok() {
            sink.written += line.len() as u64;
        }
    }

    /// 打开文件；必要时先把上一代挪走。
    fn open(path: &Path, rotated: &Path) -> Option<Sink> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok()?;
        }
        let existing = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        if existing >= MAX_LOG_BYTES {
            // 覆盖上一代：只留最近的一份，磁盘占用有上限。
            fs::rename(path, rotated).ok();
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Some(Sink { file, written })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    /// 每个用例一个独立临时目录，避免互相干扰。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("litepet-log-{tag}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("应能建临时目录");
        dir
    }

    #[test]
    fn line_starts_with_timestamp_then_level() {
        let now = Local.with_ymd_and_hms(2026, 2, 14, 9, 30, 0).unwrap();
        let line = format_line(Level::Warn, "宠物包告警", now);
        assert!(
            line.starts_with("2026-02-14T09:30:00"),
            "时间戳在最前：{line}"
        );
        assert!(line.ends_with('\n'), "整行以换行结尾：{line:?}");

        // 级别字段定宽 5 列，所以 `WARN`（4 字符）后面有两个空格。
        let after_stamp = line.find(' ').expect("有时间戳") + 1;
        assert!(
            line[after_stamp..].starts_with("WARN  宠物包告警\n"),
            "级别后紧接正文：{line}"
        );

        // 定宽的意义：正文总是从同一列开始，不同级别的行能对齐。
        let error_line = format_line(Level::Error, "出错了", now);
        let body_at = |text: &str| text.find('出').or_else(|| text.find('宠')).expect("有正文");
        assert_eq!(
            body_at(&line),
            body_at(&error_line),
            "正文列应一致：{line}{error_line}"
        );
    }

    /// 目录不存在时要自动建出来（首次启动就是这种情况）。
    #[test]
    fn creates_missing_directory_and_appends() {
        let dir = temp_dir("append");
        let path = dir.join("nested").join(LOG_FILE);
        let writer = RotatingFile::new(path.clone(), dir.join("nested").join(LOG_FILE_ROTATED));

        writer.write_line("第一行\n");
        writer.write_line("第二行\n");

        let body = fs::read_to_string(&path).expect("应能读日志");
        assert_eq!(body, "第一行\n第二行\n");
        fs::remove_dir_all(&dir).ok();
    }

    /// 超过上限要轮转，且只保留一代。
    #[test]
    fn rotates_once_limit_exceeded() {
        let dir = temp_dir("rotate");
        let path = dir.join(LOG_FILE);
        let rotated = dir.join(LOG_FILE_ROTATED);
        let writer = RotatingFile::new(path.clone(), rotated.clone());

        // 直接写一个大块把上限顶过去，再让下一次写入触发轮转。
        let chunk = "x".repeat(1024);
        let times = (MAX_LOG_BYTES / chunk.len() as u64) + 1;
        for _ in 0..times {
            writer.write_line(&format!("{chunk}\n"));
        }
        writer.write_line("轮转后的第一行\n");

        assert!(rotated.is_file(), "应留下上一代文件");
        let current = fs::read_to_string(&path).expect("应能读当前日志");
        assert!(
            current.len() < MAX_LOG_BYTES as usize,
            "当前文件应已重开：{} 字节",
            current.len()
        );
        assert!(current.ends_with("轮转后的第一行\n"), "新内容应在新文件里");

        // 上一代不该被无限堆积。
        let generations: Vec<_> = fs::read_dir(&dir)
            .expect("应能列目录")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(generations.len(), 2, "只保留两代：{generations:?}");
        fs::remove_dir_all(&dir).ok();
    }

    /// 路径失败不能 panic——日志写不进去也不该影响宠物。
    #[test]
    fn unwritable_path_is_silent() {
        let dir = temp_dir("unwritable");
        // 把「父目录」占成一个普通文件，`create_dir_all` 必然失败。
        let blocker = dir.join("blocker");
        fs::write(&blocker, "not a dir").expect("应能建占位文件");
        let writer = RotatingFile::new(blocker.join(LOG_FILE), blocker.join(LOG_FILE_ROTATED));
        writer.write_line("这行会被丢弃\n");
        fs::remove_dir_all(&dir).ok();
    }
}
