//! `~/.litepet/config.json` 的读写。
//!
//! 家目录约定见 `docs/PET-PACK.md` §2：`~/.litepet/` 同时装配置与 `pets/`。

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// 家目录环境变量名。
const HOME_ENV: &str = "LITEPET_HOME";
/// 家目录默认路径（相对用户 home）。
const DEFAULT_HOME_DIR: &str = ".litepet";
/// 配置文件文件名。
const CONFIG_FILE: &str = "config.json";
/// 宠物包子目录名。
const PETS_DIR: &str = "pets";
/// 运行期对接信息文件名。
///
/// 与 `config.json`／`pets/` 同处家目录，受 `LITEPET_HOME` 控制。
/// 它是**运行期状态**而非配置：每次启动重写，正常退出时删除。
const ENDPOINT_FILE: &str = "daemon.json";
/// 默认 HTTP 端口。
pub const DEFAULT_PORT: u16 = 4590;
/// 生成 token 的字节数（128 位）。
const TOKEN_BYTES: usize = 16;
/// 含密钥文件的权限位。
const PRIVATE_MODE: u32 = 0o600;
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
    /// 监听端口；改这里的同时要把宿主也改成同一个值。
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pet: None,
            x: None,
            y: None,
            size: DEFAULT_SIZE,
            always_on_top: true,
            port: DEFAULT_PORT,
        }
    }
}

/// `serde` 缺省值：窗口边长。
fn default_size() -> u32 {
    DEFAULT_SIZE
}

/// `serde` 缺省值：监听端口。
fn default_port() -> u16 {
    DEFAULT_PORT
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

/// 运行中的 daemon 的对接信息，写在 `~/.litepet/daemon.json`。
///
/// 宿主只要读这个文件就知道该往哪个端口、带哪个 token 发 JSON-RPC，
/// 因此端口改到非默认值也不会让宿主迷失。
///
/// token 每次启动重新生成，所以这是运行期状态：异常退出可能残留，
/// 那时里面的端口已经没人监听，宿主连不上自然会重新拉起 daemon。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    /// 协议版本。
    pub protocol_version: u32,
    /// HTTP 端口。
    pub port: u16,
    /// 访问密钥（32 位十六进制）。
    pub token: String,
}

/// 对接信息路径 `~/.litepet/daemon.json`。
pub fn endpoint_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(ENDPOINT_FILE))
}

/// 写入对接信息并返回其路径。
///
/// 文件含密钥，以 `0600` 写入。
pub fn write_endpoint(protocol_version: u32, port: u16, token: &str) -> Result<PathBuf> {
    let path = endpoint_path()?;
    let endpoint = Endpoint {
        protocol_version,
        port,
        token: token.to_string(),
    };
    let body = serde_json::to_string_pretty(&endpoint).context("序列化对接信息失败")?;
    write_private(&path, &body)?;
    Ok(path)
}

/// 删除对接信息（正常退出时调用）。
pub fn remove_endpoint() -> Result<()> {
    let path = endpoint_path()?;
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(&path).with_context(|| format!("删除对接信息失败：{}", path.display()))
}

/// 生成一个 128 位随机 token（32 位小写十六进制）。
///
/// 复用依赖树里已有的 `getrandom`，不自造随机数。
pub fn random_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    // `getrandom::Error` 没有实现 `std::error::Error`，只能手接错误。
    getrandom::fill(&mut bytes).map_err(|err| anyhow!("获取系统随机数失败：{err}"))?;
    let mut token = String::with_capacity(TOKEN_BYTES * 2);
    for byte in bytes {
        // 往 `String` 里写不可能失败。
        write!(token, "{byte:02x}").expect("写入 String 不会失败");
    }
    Ok(token)
}

/// 以 `0600` 写文件：对接信息里有密钥，不能让同机其他用户读到。
///
/// `mode()` 只在创建时生效，因此已有文件额外补一次 `set_permissions`，
/// 避免「上一次残留了权限过宽的文件」继承下来。
fn write_private(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建家目录失败：{}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(PRIVATE_MODE)
        .open(path)
        .with_context(|| format!("写入失败：{}", path.display()))?;
    file.write_all(body.as_bytes())
        .with_context(|| format!("写入失败：{}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_MODE))
        .with_context(|| format!("设置权限失败：{}", path.display()))?;
    Ok(())
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
        assert_eq!(
            endpoint_path().expect("endpoint"),
            home.join("daemon.json")
        );
    }

    #[test]
    fn random_token_is_hex_and_unique() {
        let first = random_token().expect("应能生成 token");
        let second = random_token().expect("应能生成 token");
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second, "两次生成不应撞车");
    }

    #[test]
    fn endpoint_survives_a_round_trip() {
        let endpoint = Endpoint {
            protocol_version: 1,
            port: DEFAULT_PORT,
            token: "deadbeefdeadbeefdeadbeefdeadbeef".to_string(),
        };
        let body = serde_json::to_string(&endpoint).expect("应能序列化");
        assert!(body.contains("\"protocolVersion\""), "字段名用 camelCase：{body}");
        assert_eq!(
            serde_json::from_str::<Endpoint>(&body).expect("应能反序列化"),
            endpoint
        );
    }

    /// 含密钥的文件必须是 0600，否则同机其他用户能直接拿去驱动别人的宠物。
    #[test]
    fn private_file_is_owner_only() {
        let dir = std::env::temp_dir().join(format!(
            "litepet-config-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("应能建临时目录");
        let path = dir.join("daemon.json");

        write_private(&path, "{}").expect("应能写入");
        let mode = fs::metadata(&path).expect("应能读元数据").permissions().mode();
        assert_eq!(mode & 0o777, PRIVATE_MODE, "实际权限 {:o}", mode & 0o777);

        // 已存在但权限过宽的文件也要被收窄。
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("应能改权限");
        write_private(&path, "{}").expect("应能重写");
        let mode = fs::metadata(&path).expect("应能读元数据").permissions().mode();
        assert_eq!(mode & 0o777, PRIVATE_MODE, "重写后应收窄权限");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_defaults_include_the_documented_port() {
        let cfg = Config::default();
        assert_eq!(cfg.port, DEFAULT_PORT);
        // 旧配置文件没有 port 字段时也要能读出来。
        let legacy = r#"{"pet":"xunjian-miao","size":220,"alwaysOnTop":true}"#;
        let parsed: Config = serde_json::from_str(legacy).expect("应能读取旧配置");
        assert_eq!(parsed.port, DEFAULT_PORT);
    }
}
