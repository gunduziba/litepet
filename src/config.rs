//! `~/.litepet/config.json` 的读写。
//!
//! 家目录约定见 `docs/PET-PACK.md` §2：`~/.litepet/` 同时装配置与 `pets/`。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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
/// 日志子目录名。
const LOGS_DIR: &str = "logs";
/// 运行期对接信息文件名。
///
/// 与 `config.json`／`pets/` 同处家目录，受 `LITEPET_HOME` 控制。
/// 它是**运行期状态**而非配置：每次启动重写，正常退出时删除。
const ENDPOINT_FILE: &str = "daemon.json";
/// 默认 HTTP 端口。
pub const DEFAULT_PORT: u16 = 4590;
/// 含密钥文件的权限位。
const PRIVATE_MODE: u32 = 0o600;
/// 默认窗口边长（像素）。
const DEFAULT_SIZE: u32 = 220;

/// 窗口边长的可用范围。比 64 更小就看不出一只宠物，比 512 更大也谈不上「待在角落」。
const MIN_SIZE: u32 = 64;
const MAX_SIZE: u32 = 512;
/// 默认音量。
const DEFAULT_SOUND_VOLUME: f32 = 0.35;
/// 默认推送服务。
const DEFAULT_PUSH_PROVIDER: &str = "bark";

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
    /// 通知（声音／系统通知／手机推送）。
    #[serde(default)]
    pub notify: NotifyConfig,
    /// 鉴权（`POST /rpc` 的 token 校验）。
    #[serde(default)]
    pub auth: AuthConfig,
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
            notify: NotifyConfig::default(),
            auth: AuthConfig::default(),
        }
    }
}

impl Config {
    /// 把读来的值夹进可用范围。
    ///
    /// 配置是可以被手改的（现在也由配置页写），而越界的值不会报错、只会让现象变得
    /// 莫名其妙：`size: 0` 是一只看不见的宠物，`volume: 5` 是一声被削平的噪声，
    /// `port: 0` 会让端点文件里写一个没人监听的端口号、宿主连上来才发现不对。
    /// 所以每次读出来先夹一遍再往下传。
    pub fn normalize(&mut self) {
        if !(MIN_SIZE..=MAX_SIZE).contains(&self.size) {
            let clamped = self.size.clamp(MIN_SIZE, MAX_SIZE);
            log::warn!(
                "配置 size={} 超出 {}..={}，按 {} 处理",
                self.size,
                MIN_SIZE,
                MAX_SIZE,
                clamped
            );
            self.size = clamped;
        }
        if !(0.0..=1.0).contains(&self.notify.sound.volume) {
            let clamped = self.notify.sound.volume.clamp(0.0, 1.0);
            log::warn!(
                "配置 volume={} 超出 0..=1，按 {} 处理",
                self.notify.sound.volume,
                clamped
            );
            self.notify.sound.volume = clamped;
        }
        if self.port == 0 {
            log::warn!("配置 port=0 没有可监听的含义，按 {} 处理", DEFAULT_PORT);
            self.port = DEFAULT_PORT;
        }
    }

    /// 解析本次启动的准入策略。
    ///
    /// **有 token 就按 token 校验，token 为空就不鉴权**——只有这两条分支，
    /// 因为它们靠同一个字段就能区分开，不需要额外的开关（见 [`AuthConfig`]）。
    ///
    /// token 配得不对只锁接口，**不阻止启动**：理由见 [`AuthGate::Locked`]。
    pub fn auth_gate(&self) -> AuthGate {
        if self.auth.token.is_empty() {
            return AuthGate::Open;
        }
        // 判在 `auth.token` 原值上，而不是 trim 后的值上：两头的空白会让它永远配不上
        // 任何请求（校验时两边的空白都会被 `trim` 掉），所以直接算不可用。
        // 非可见 ASCII 同样不可用：那种头值会在 HTTP 解析层就把连接掐断，
        // 请求连 401 都拿不到（实测 curl 退出码 52），永远不可能匹配成功。
        if self.auth.token.chars().all(|c| c.is_ascii_graphic()) {
            return AuthGate::Token(self.auth.token.clone());
        }
        AuthGate::Locked
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

/// `POST /rpc` 的准入策略，由 `config.json` 的 `auth` 段解析而来（见 [`Config::auth_gate`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthGate {
    /// 配置里**没有** token：不要求任何头。
    ///
    /// 接口仍然只绑回环，但**同机上任何程序都能连**，包括你正在浏览的网页
    /// （浏览器向 `localhost` 发简单请求可以绕过 CORS 预检）。
    Open,
    /// 配了 token，但那个 token 根本用不了：**一个请求都不放进来**。
    ///
    /// 既不是「拒绝启动」，也不是「退回不鉴权」。
    /// 不拒绝启动：鉴权只管 `POST /rpc` 的准入，跟窗口、托盘、宠物渲染无关，
    /// 不该因为一个填错的 token 让人连宠物都看不见。
    /// 不退回不鉴权：那是静悄悄地把接口敲开，而用户会以为自己已经设了防，
    /// 比大声报错难发现得多。空 token 走的是 [`AuthGate::Open`]，这里不含这一种。
    Locked,
    /// 校验 `Authorization: Bearer <token>`。
    Token(String),
}

/// 鉴权配置：`config.json` 的 `auth` 段。
///
/// **只有 token 一个字段。** 早先还有一个 `enabled` 开关，删掉了：
/// 「要不要鉴权」看 token 空不空就已经能表达，多一个开关就多出一种自相矛盾的摆法
/// （开着鉴权却没填 token），还得额外规定那种摆法算哪边。
///
/// **token 由用户自己定，daemon 不生成、不轮换。** 更早的版本每次启动随机生成一个，
/// 那等于要求宿主每次重启都回去重读端点文件；用户配好的值重启一次就没了。
///
/// `Default` 是 derive 出来的，即**缺省 token 为空 = 缺省不鉴权**。这是有意定的缺省方向，
/// 不是遗漏：旧配置里没有 `auth` 段时也走这条路，所以「不设防」会在启动日志里
/// `warn` 出来，让人知道它现在是这个状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AuthConfig {
    /// 访问密钥，用户自定。**留空 = 不鉴权**；含空白或非 ASCII 字符时接口会被锁死（见 [`Config::auth_gate`]）。
    pub token: String,
}

/// 通知配置：`config.json` 的 `notify` 段（全部字段可缺省）。
///
/// **只放全局开关、凭据与阈值**：哪个事件该响哪一声、该不该推手机，
/// 由宠物包的 `litepet.behavior` 规则表决定（见 `docs/PET-PACK.md` §4.2）。
/// 两边各管一份映射只会互相矛盾，所以这里故意不提供逐事件覆盖。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyConfig {
    /// 总开关；关掉后声音、系统通知、推送都不发。
    pub enabled: bool,
    /// 声音通道。
    pub sound: SoundConfig,
    /// 系统通知通道。
    pub desktop: DesktopConfig,
    /// 手机推送通道。
    pub push: PushConfig,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sound: SoundConfig::default(),
            desktop: DesktopConfig::default(),
            push: PushConfig::default(),
        }
    }
}

/// 声音通道配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SoundConfig {
    /// 声音开关。
    pub enabled: bool,
    /// 音量，`0.0..=1.0`；超出范围会被夹到边界。
    pub volume: f32,
}

impl Default for SoundConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: DEFAULT_SOUND_VOLUME,
        }
    }
}

/// 系统通知通道配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DesktopConfig {
    /// 系统通知开关。
    pub enabled: bool,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 手机推送通道配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PushConfig {
    /// 推送开关。
    pub enabled: bool,
    /// 推送服务标识；目前提供 `bark`。
    pub provider: String,
    /// 设备密钥。按约定以**明文**存在配置里，因此 `config.json` 以 `0600` 写入。
    pub device_key: String,
    /// 自定义推送端点；`None` 用 provider 的默认端点。
    pub endpoint: Option<String>,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: DEFAULT_PUSH_PROVIDER.to_string(),
            device_key: String::new(),
            endpoint: None,
        }
    }
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

/// 日志目录 `~/.litepet/logs`。
pub fn logs_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(LOGS_DIR))
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
/// token 来自 `config.json` 的 `auth.token`（用户自定，不随启动变化）；
/// 关掉鉴权时它是空串。这个文件本身仍是运行期状态：正常退出时删除，
/// 异常退出可能残留——那时里面的端口已经没人监听，宿主连不上自然会重新拉起 daemon。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    /// 协议版本。
    pub protocol_version: u32,
    /// HTTP 端口。
    pub port: u16,
    /// 访问密钥；关掉鉴权时为空串。
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

/// 删除对接信息，但**只在它确实指向 `port` 时**。
///
/// 退出清理必须带这个判断：`--port` 不同时允许多个实例同时跑，
/// 否则先退出的那个会把还在跑的那个的对接文件删掉，宿主就再也找不到它了。
pub fn remove_endpoint_for(port: u16) -> Result<()> {
    let path = endpoint_path()?;
    let Ok(body) = fs::read_to_string(&path) else {
        return Ok(());
    };
    // 文件坏了也不能删：它可能正是另一个实例写的，留给下一次启动自己覆盖。
    let ours = endpoint_belongs_to(&body, port);
    if !ours {
        return Ok(());
    }
    fs::remove_file(&path).with_context(|| format!("删除对接信息失败：{}", path.display()))
}

/// 一份对接信息是否属于端口 `port`。
///
/// 读不了或字段不全都算「不属于」——宁可留着不删：
/// 删错了会让另一个还在跑的实例从磁盘上消失，留着最多多覆盖一次。
fn endpoint_belongs_to(body: &str, port: u16) -> bool {
    serde_json::from_str::<Endpoint>(body).is_ok_and(|endpoint| endpoint.port == port)
}

/// 在 `path` 旁边造一个 `<文件名><后缀>` 的兄弟路径。
///
/// 用拼接而不是 `with_extension`：后者会把原扩展名换掉，
/// `config.json` 得到的名字读起来像是另一种文件格式。
fn sibling(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = path.file_name().context("配置路径无文件名")?.to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

/// 以 `0600` 原子写文件：对接信息里有密钥，不能让同机其他用户读到。
///
/// **先写 `.tmp` 再 `rename`**：就地截断重写一旦中途失败（断电、被 kill、
/// 磁盘满），留在盘上的是一份残缺的 JSON，而下次启动就再也读不出配置了。
/// `rename` 在同一文件系统内是原子的，所以任何时刻读到的要么是完整旧文件、
/// 要么是完整新文件。
fn write_private(path: &Path, body: &str) -> Result<()> {
    let tmp = sibling(path, ".tmp")?;
    write_private_body(&tmp, body)?;
    if let Err(err) = fs::rename(&tmp, path) {
        // 别把半成品留在目录里让用户困惑；下次写入本来也会覆盖它。
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("替换失败：{}", path.display()));
    }
    Ok(())
}

/// 单文件写入（`0600` 且落盘），不做原子处理，只被 `write_private` 用来写临时文件。
///
/// `mode()` 只在创建时生效，因此已有文件额外补一次 `set_permissions`，
/// 避免「上一次残留了权限过宽的文件」继承下来。
fn write_private_body(path: &Path, body: &str) -> Result<()> {
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
    // 不落盘的话，`rename` 可能先于数据到达磁盘：崩溃后留下一个名字对、
    // 内容空的文件——比截断更难查。
    file.sync_all()
        .with_context(|| format!("落盘失败：{}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_MODE))
        .with_context(|| format!("设置权限失败：{}", path.display()))?;
    Ok(())
}

/// 从 `path` 读配置；文件不存在就建目录并写入默认配置。
///
/// 收路径参数是为了能在临时目录里测「配置损坏」那条分支：它靠环境变量
/// `LITEPET_HOME` 测不稳（并发跑的测试会互相改环境）。
fn load_config_at(path: &Path) -> Result<(Config, bool)> {
    if path.is_file() {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("读取配置失败：{}", path.display()))?;
        match serde_json::from_str::<Config>(&raw) {
            Ok(mut cfg) => {
                cfg.normalize();
                return Ok((cfg, false));
            }
            // 读不懂的配置不该把 daemon 拦在门外：「用默认值起来」和「起不来」
            // 对用户是两种完全不同的体验。坏文件挪到一旁留证，然后用默认值继续。
            Err(err) => {
                let broken = sibling(path, ".broken")?;
                log::warn!(
                    "配置解析失败（{err}），已备份到 {}，本次用默认值启动",
                    broken.display()
                );
                if let Err(err) = fs::rename(path, &broken) {
                    log::warn!("备份坏配置失败：{err}");
                }
                let cfg = Config::default();
                save_at(path, &cfg)?;
                return Ok((cfg, true));
            }
        }
    }
    let parent = path.parent().context("配置路径无父目录")?;
    fs::create_dir_all(parent).with_context(|| format!("创建家目录失败：{}", parent.display()))?;
    // 顺带建好 pets/，避免首次启动时目录缺失
    let pets = pets_dir()?;
    fs::create_dir_all(&pets).with_context(|| format!("创建宠物目录失败：{}", pets.display()))?;
    let cfg = Config::default();
    save_at(path, &cfg)?;
    Ok((cfg, true))
}

/// 读取配置；文件不存在时创建家目录并写入默认配置。
///
/// 返回 (配置, 是否为新建)。
pub fn load_or_init() -> Result<(Config, bool)> {
    load_config_at(&config_path()?)
}

/// 写回配置。
///
/// 走私有写入：配置里有明文推送密钥，不能让同机其他用户读到。
pub fn save(cfg: &Config) -> Result<()> {
    save_at(&config_path()?, cfg)
}

/// 往指定路径写回配置（`load_config_at` 与 `save` 共用）。
fn save_at(path: &Path, cfg: &Config) -> Result<()> {
    let body = serde_json::to_string_pretty(cfg).context("序列化配置失败")?;
    write_private(path, &body)
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
        assert_eq!(endpoint_path().expect("endpoint"), home.join("daemon.json"));
        assert_eq!(logs_dir().expect("logs"), home.join("logs"));
    }

    /// 退出清理只能删自己写的对接信息。
    ///
    /// `--port` 不同时允许多个实例同时跑，删错了会让另一个实例从磁盘上消失。
    #[test]
    fn endpoint_ownership_is_by_port() {
        let body = r#"{"protocolVersion":1,"port":4590,"token":"ab"}"#;
        assert!(endpoint_belongs_to(body, 4590), "同端口应当认领");
        assert!(!endpoint_belongs_to(body, 4591), "别的实例的文件不能删");
    }

    /// 对接信息读不了时一律不认领。删掉它风险更大：它可能正是另一个实例写的。
    #[test]
    fn endpoint_ownership_rejects_unreadable_file() {
        assert!(!endpoint_belongs_to("", 4590));
        assert!(!endpoint_belongs_to("不是 JSON", 4590));
        assert!(
            !endpoint_belongs_to(r#"{"port":4590}"#, 4590),
            "缺字段不算自己的"
        );
    }

    /// 旧的配置文件里没有 `notify` 段，读出来必须是一份可用的默认值。
    #[test]
    fn notify_section_defaults_when_absent() {
        let legacy = r#"{"pet":"xunjian-miao"}"#;
        let cfg: Config = serde_json::from_str(legacy).expect("应能读取旧配置");
        assert!(cfg.notify.enabled, "通知默认开着");
        assert!(cfg.notify.sound.enabled);
        assert_eq!(cfg.notify.sound.volume, DEFAULT_SOUND_VOLUME);
        assert!(cfg.notify.desktop.enabled);
        assert!(!cfg.notify.push.enabled, "推送要用户自己开");
        assert_eq!(cfg.notify.push.provider, DEFAULT_PUSH_PROVIDER);
    }

    /// 只写了一部分字段的 `notify` 段，其余字段补齐而不是整段报错。
    #[test]
    fn notify_section_fills_partial_input() {
        let partial = r#"{"notify":{"push":{"enabled":true,"deviceKey":"abc"}}}"#;
        let cfg: Config = serde_json::from_str(partial).expect("应能解析部分配置");
        assert!(cfg.notify.push.enabled);
        assert_eq!(cfg.notify.push.device_key, "abc");
        assert!(cfg.notify.sound.enabled, "未提到的通道保持默认");
    }

    /// 落盘后的 `notify` 段字段名是外部契约（`docs/PET-PACK.md` §4.5.2 的样例
    /// 与用户手写的配置都靠它），所以用 camelCase 锁住，不能被结构体重命名悄悄改掉。
    #[test]
    fn notify_section_serializes_camel_case() {
        let json = serde_json::to_value(Config::default()).expect("应能序列化");
        let notify = &json["notify"];
        assert_eq!(notify["enabled"], serde_json::json!(true));
        assert_eq!(notify["sound"]["enabled"], serde_json::json!(true));
        assert_eq!(
            notify["sound"]["volume"],
            serde_json::json!(DEFAULT_SOUND_VOLUME)
        );
        assert_eq!(notify["desktop"]["enabled"], serde_json::json!(true));
        assert_eq!(notify["push"]["enabled"], serde_json::json!(false));
        assert_eq!(
            notify["push"]["provider"],
            serde_json::json!(DEFAULT_PUSH_PROVIDER)
        );
        assert_eq!(notify["push"]["deviceKey"], serde_json::json!(""));
        assert!(notify["push"]["endpoint"].is_null());

        // 旧的「人在不在」判定参数已整体移除。字段一旦回流，这里会先报错，
        // 而不是等用户发现自己的开关被默默忽略。
        assert!(notify.get("presence").is_none());
        assert!(notify["desktop"].get("whenFocused").is_none());
        assert!(notify["push"].get("onlyWhenAway").is_none());
    }

    /// 配置里有明文推送密钥，所以必须 0600 落盘。
    #[test]
    fn config_file_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("litepet-cfg-mode-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("应能建临时目录");
        let path = dir.join("config.json");
        write_private(&path, "{}").expect("应能写入");
        let mode = fs::metadata(&path)
            .expect("应能读元数据")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, PRIVATE_MODE, "实际权限 {:o}", mode & 0o777);
        fs::remove_dir_all(&dir).ok();
    }

    /// **没填 token 就是不鉴权**，不看任何头。
    #[test]
    fn empty_token_means_no_auth() {
        let cfg = Config {
            auth: AuthConfig {
                token: String::new(),
            },
            ..Config::default()
        };
        assert_eq!(cfg.auth_gate(), AuthGate::Open);
    }

    /// 非 ASCII 或带空白的 token 一律算不可用：**锁接口，但不拦启动**。
    ///
    /// 前一组行钉住一个实测到的现象：把中文字写进 `Authorization` 头，
    /// tiny_http 会直接掐掉连接，客户端拿到的是「空响应」而不是 401——
    /// 那时人会去查端口、查网络，唯独不会想到是 token 里那个字。
    /// 纯空白也在这一组：它不是「没填」，是填了个配不上的东西。
    ///
    /// 后一半行钉住一个真实争论：先前这里写的是「直接 `exit(1)`」。那不对——
    /// 鉴权只管 `POST /rpc` 的准入，跟窗口、托盘、宠物渲染无关，
    /// 一个填错的 token 不该让人连宠物都看不见。
    #[test]
    fn unusable_token_locks_the_gate_instead_of_blocking_startup() {
        for token in ["我自己的口令", " has-space", "has space", "tab\there", "   "] {
            let cfg = Config {
                auth: AuthConfig {
                    token: token.to_string(),
                },
                ..Config::default()
            };
            assert_eq!(cfg.auth_gate(), AuthGate::Locked, "token={token:?}");
        }
    }

    /// 用户填了可用的 token 就照它校验。
    #[test]
    fn configured_token_resolves_to_that_token() {
        let cfg = Config {
            auth: AuthConfig {
                token: "my-own-secret-abc123".to_string(),
            },
            ..Config::default()
        };
        assert_eq!(
            cfg.auth_gate(),
            AuthGate::Token("my-own-secret-abc123".to_string())
        );
    }

    /// 旧配置文件里没有 `auth` 段 → token 为空 → **不鉴权**。
    ///
    /// 这是用户明确要的语义（「token 为空则默认不用鉴权」），不是漏配的意外：
    /// 缺省值就是空 token，升级上来的老配置也一样。代价是**不设防时不拦人**，
    /// 所以那个状态会在启动日志里 `warn` 出来，设置页也当场写着「本机谁都能连」。
    #[test]
    fn legacy_config_without_auth_means_no_auth() {
        let cfg: Config = serde_json::from_str("{}").expect("空配置应能解析");
        assert_eq!(cfg.auth.token, "", "缺省的 token 应当是空串");
        assert_eq!(cfg.auth_gate(), AuthGate::Open);
    }

    #[test]
    fn endpoint_survives_a_round_trip() {
        let endpoint = Endpoint {
            protocol_version: 1,
            port: DEFAULT_PORT,
            token: "deadbeefdeadbeefdeadbeefdeadbeef".to_string(),
        };
        let body = serde_json::to_string(&endpoint).expect("应能序列化");
        assert!(
            body.contains("\"protocolVersion\""),
            "字段名用 camelCase：{body}"
        );
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
        let mode = fs::metadata(&path)
            .expect("应能读元数据")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, PRIVATE_MODE, "实际权限 {:o}", mode & 0o777);

        // 已存在但权限过宽的文件也要被收窄。
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("应能改权限");
        write_private(&path, "{}").expect("应能重写");
        let mode = fs::metadata(&path)
            .expect("应能读元数据")
            .permissions()
            .mode();
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

    /// 造一个本次调用独占的临时目录（测试并发跑，不能撞名）。
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        let dir =
            std::env::temp_dir().join(format!("litepet-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("应能建临时目录");
        dir
    }

    /// 越界的值要被夹回可用范围：这些字段现在会由配置页写，也可能被手改。
    #[test]
    fn normalize_clamps_out_of_range() {
        let mut cfg = Config {
            size: 0,
            ..Config::default()
        };
        cfg.notify.sound.volume = 5.0;
        cfg.port = 0;
        cfg.normalize();
        assert_eq!(cfg.size, MIN_SIZE, "size=0 要抬到下限");
        assert_eq!(cfg.notify.sound.volume, 1.0, "音量上限是 1");
        assert_eq!(cfg.port, DEFAULT_PORT, "port=0 无法监听，要回退默认端口");

        let mut big = Config {
            size: 99_999,
            ..Config::default()
        };
        big.notify.sound.volume = -1.0;
        big.normalize();
        assert_eq!(big.size, MAX_SIZE, "过大的尺寸要压到上限");
        assert_eq!(big.notify.sound.volume, 0.0, "音量下限是 0");
    }

    /// 范围内的值不得被 `normalize` 动过，否则它会变成一个「每次读都改配置」的东西。
    #[test]
    fn normalize_keeps_values_in_range() {
        let mut cfg = Config {
            size: 300,
            port: 4700,
            ..Config::default()
        };
        cfg.notify.sound.volume = 0.6;
        cfg.normalize();
        assert_eq!(cfg.size, 300);
        assert_eq!(cfg.port, 4700);
        assert_eq!(cfg.notify.sound.volume, 0.6);
    }

    /// 配置被写坏时不能把 daemon 拦在门外：备份坏文件后用默认值继续。
    #[test]
    fn corrupt_config_falls_back_to_defaults() {
        let dir = temp_dir("corrupt");
        let path = dir.join(CONFIG_FILE);
        let garbage = r#"{"pet":"xunjian-miao",,,"#;
        fs::write(&path, garbage).expect("应能写入坏配置");

        let (cfg, created) = load_config_at(&path).expect("坏配置不该导致失败");
        assert!(created, "重建过配置就算新建");
        assert_eq!(cfg.size, DEFAULT_SIZE, "应拿到一份默认配置");

        // 坏文件必须留着：用户可能想自己看一眼到底哪里写错了。
        let broken = sibling(&path, ".broken").expect("应能算出备份路径");
        assert_eq!(fs::read_to_string(&broken).expect("应能读备份"), garbage);
        // 而原位置应该已经是一份能读的配置。
        let (again, created) = load_config_at(&path).expect("重读要成功");
        assert!(!created, "第二次不是新建");
        assert_eq!(again.size, DEFAULT_SIZE);
        fs::remove_dir_all(&dir).ok();
    }

    /// 原子写不得在目录里留下 `.tmp`，否则用户会看到一个来历不明的文件。
    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = temp_dir("atomic");
        let path = dir.join(CONFIG_FILE);
        let cfg = Config::default();

        save_at(&path, &cfg).expect("应能写入");
        // 再写一次，覆盖已存在的文件（rename 覆盖这条路）。
        save_at(&path, &cfg).expect("应能覆盖写入");

        let tmp = sibling(&path, ".tmp").expect("应能算出临时路径");
        assert!(!tmp.exists(), "写完不该留下 {}", tmp.display());
        let (loaded, _) = load_config_at(&path).expect("应能读回");
        assert_eq!(loaded.port, cfg.port);
        fs::remove_dir_all(&dir).ok();
    }
}
