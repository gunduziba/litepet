//! `~/.litepet/config.json` 的读写。
//!
//! 家目录约定见 `docs/PET-PACK.md` §2：`~/.litepet/` 同时装配置与 `pets/`。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// 家目录环境变量名。
const HOME_ENV: &str = "LITEPET_HOME";
/// 家目录默认路径（相对用户 home）。
const DEFAULT_HOME_DIR: &str = ".litepet";
/// 配置文件文件名。
const CONFIG_FILE: &str = "config.json";
/// 宠物包子目录名。
const PETS_DIR: &str = "pets";
/// socket 文件名。
///
/// 与 `config.json`／`pets/` 同处家目录（`SPEC.md` §传输）：受 `LITEPET_HOME` 控制，
/// 路径长度远低于 macOS 的 104 字节 `sun_path` 上限，也不受 `$TMPDIR` 清理影响。
const SOCKET_FILE: &str = "daemon.sock";
/// 默认窗口边长（像素）。
const DEFAULT_SIZE: u32 = 220;

/// daemon 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// 当前宠物包 id；`None` 表示取 `pets/` 下第一个可用包。
    #[serde(default)]
    pub pet: Option<String>,
    /// 窗口横向位置（屏幕坐标）。
    #[serde(default)]
    pub x: Option<i32>,
    /// 窗口纵向位置（屏幕坐标）。
    #[serde(default)]
    pub y: Option<i32>,
    /// 窗口边长（像素）。
    #[serde(default = "default_size")]
    pub size: u32,
    /// 是否置顶。
    #[serde(default = "default_true")]
    pub always_on_top: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pet: None,
            x: None,
            y: None,
            size: DEFAULT_SIZE,
            always_on_top: true,
        }
    }
}

/// `serde` 缺省值：窗口边长。
fn default_size() -> u32 {
    DEFAULT_SIZE
}

/// `serde` 缺省值：置顶开关。
fn default_true() -> bool {
    true
}

/// 家目录，受 `LITEPET_HOME` 覆盖；否则为 `~/.litepet`。
pub fn home_dir() -> Result<PathBuf> {
    if let Some(raw) = std::env::var_os(HOME_ENV) {
        return Ok(PathBuf::from(raw));
    }
    let home = std::env::var_os("HOME").context("环境变量 HOME 缺失，无法定位家目录")?;
    Ok(PathBuf::from(home).join(DEFAULT_HOME_DIR))
}

/// 宠物包根目录 `~/.litepet/pets`。
pub fn pets_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(PETS_DIR))
}

/// 配置文件路径 `~/.litepet/config.json`。
pub fn config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(CONFIG_FILE))
}

/// daemon 的 Unix socket 路径 `~/.litepet/daemon.sock`。
pub fn socket_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(SOCKET_FILE))
}

/// 读取配置；文件不存在时创建家目录并写入默认配置。
///
/// 返回 (配置, 是否为新建)。
pub fn load_or_init() -> Result<(Config, bool)> {
    let path = config_path()?;
    if path.is_file() {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("读取配置失败：{}", path.display()))?;
        let cfg: Config = serde_json::from_str(&raw)
            .with_context(|| format!("解析配置失败：{}", path.display()))?;
        return Ok((cfg, false));
    }
    let parent = path.parent().context("配置路径无父目录")?;
    fs::create_dir_all(parent).with_context(|| format!("创建家目录失败：{}", parent.display()))?;
    // 顺带建好 pets/，避免首次启动时目录缺失
    let pets = pets_dir()?;
    fs::create_dir_all(&pets).with_context(|| format!("创建宠物目录失败：{}", pets.display()))?;
    let cfg = Config::default();
    save(&cfg)?;
    Ok((cfg, true))
}

/// 写回配置。
pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建家目录失败：{}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(cfg).context("序列化配置失败")?;
    fs::write(&path, body).with_context(|| format!("写入配置失败：{}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三个路径都必须落在同一个家目录下。
    #[test]
    fn paths_share_one_home() {
        let home = home_dir().expect("应能定位家目录");
        assert_eq!(pets_dir().expect("pets"), home.join("pets"));
        assert_eq!(config_path().expect("config"), home.join("config.json"));
        assert_eq!(socket_path().expect("socket"), home.join("daemon.sock"));
    }

    /// socket 路径必须短于 macOS `sun_path` 的 104 字节上限。
    #[test]
    fn socket_path_fits_sun_path() {
        let path = socket_path().expect("应能定位 socket");
        let bytes = path.as_os_str().as_encoded_bytes().len();
        assert!(bytes < 104, "路径长 {bytes} 字节，超出 sun_path 上限：{path:?}");
    }
}
