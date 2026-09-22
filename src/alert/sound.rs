//! 音效通道：用 rodio 出声，并且绝不阻塞会话线程。
//!
//! 两个现实约束决定了这里的形状：
//!
//! 1. **cpal 的输出流不是 `Send`，而且一 drop 就断音。** 所以它不能躺在
//!    某个随时可能被移动或丢弃的句柄里，必须待在一个专用线程上。顺带的好处是
//!    「打开音频设备」被推到了第一次真要出声的时刻，而不是每次启动都敲一次 CoreAudio。
//! 2. **系统音效的名字不跨平台。** macOS 是 `Glass`，Windows 是 `Media\Windows Notify.wav`，
//!    Linux 是 `bell`，而 Windows 那份甚至对不上我们写的候选名。所以除了「原样写名字」
//!    之外，还认 `@done` 这类语义名；语义名走**用户自选 → 应用自带兜底 → 系统音效**
//!    这条链（见 [`resolve`]），前两层是文件、随安装包走，所以两个平台上听感一致。
//! 3. **裸名字先当包内文件。** `"sound": "done.wav"` 是包作者最自然的写法，
//!    所以不含 `/` 的名字先到宠物包里找，找不到再当系统音效名——顺序见 [`resolve`]。

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::{anyhow, Result};
use rodio::stream::DeviceSinkBuilder;
use rodio::{MixerDeviceSink, Player};

use crate::config::SoundFiles;

/// 语义音效名的前缀。
const SEMANTIC_PREFIX: char = '@';

/// 「一件事顺利办完了」。
pub const SEMANTIC_DONE: &str = "@done";

/// 「一件事搞砸了」。
pub const SEMANTIC_FAILED: &str = "@failed";

/// 「需要你来看一眼」。
pub const SEMANTIC_ATTENTION: &str = "@attention";

/// 语义名 → 各平台系统音效名（按序取首个真正存在的）。
const SEMANTICS: &[(&str, &[&str])] = &[
    (SEMANTIC_DONE, &["Glass", "complete", "bell"]),
    (SEMANTIC_FAILED, &["Basso", "dialog-error", "alert"]),
    (SEMANTIC_ATTENTION, &["Ping", "message", "bell"]),
];

/// 查找系统音效时按序尝试的扩展名。
const SOUND_EXTENSIONS: &[&str] = &["aiff", "wav", "oga", "ogg", "mp3"];

/// 音量上限兜底用的取值范围。
const VOLUME_MIN: f32 = 0.0;
const VOLUME_MAX: f32 = 1.0;

/// 规则表里 `alert.sound` 的两种写法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sound {
    /// 含 `/`：宠物包内的相对路径。
    Pack(String),
    /// 不含 `/`：裸名字。`@` 开头的是语义名，否则包内文件与系统音效都试试。
    System(String),
}

impl Sound {
    /// 按约定解析：含 `/` 就是包内路径，否则是裸名字。
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.contains('/') || trimmed.contains('\\') {
            Self::Pack(trimmed.to_string())
        } else {
            Self::System(trimmed.to_string())
        }
    }
}

/// 解析音效时需要的三处素材来源。
///
/// 打包成一个结构体而不是逐个往下传，理由和 `http::Shared` 一样：要过配置、宠物包、
/// 应用资源三样东西，位置参数一多，读的人就得回去数顺序。
#[derive(Debug, Clone, Copy, Default)]
pub struct Sources<'a> {
    /// 用户配置的三个语义音效（`config.json` 的 `notify.sound.files`）。
    pub user: Option<&'a SoundFiles>,
    /// 宠物包根目录，解析包内相对路径用。
    pub pack_root: Option<&'a Path>,
    /// 应用自带的兜底音效目录（打包后是 `resource_dir` 下的 `sounds/`）。
    pub bundled: Option<&'a Path>,
}

/// 音效素材的来路。
///
/// 单独记下来是因为「哪个文件会被播」在界面上完全看不见：用户挑了一个文件、
/// 听到声音，但实际响的可能是自带的兜底——不说清楚就会变成下次续查的谜。
/// 这几个字符串会出在 `notify/preview` 的响应里（`docs/PROTOCOL.md`），
/// 改写法就等于改协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// 用户自己在设置页挑的文件。
    User,
    /// 应用自带的兜底音效。
    Bundled,
    /// 宠物包里的文件。
    Pack,
    /// 当前平台的系统音效。
    System,
}

impl Layer {
    /// 协议里的名字。
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::User => "user",
            Layer::Bundled => "bundled",
            Layer::Pack => "pack",
            Layer::System => "system",
        }
    }
}

/// 命中的文件，以及它来自哪一层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// 磁盘上真实存在的文件。
    pub path: PathBuf,
    /// 它是哪一层给的。
    pub layer: Layer,
}

impl Hit {
    /// 给一个解析结果打上「来自哪一层」的标签。
    fn at(layer: Layer, path: Option<PathBuf>) -> Option<Self> {
        path.map(|path| Self { path, layer })
    }
}

/// 把音效解析成磁盘上真实存在的文件；找不到返回 `None`。
///
/// 找不到**不是错误**：用户从别的平台搬一个包过来、或者系统没装某个音效，
/// 都是很常见的事。调用方记一条带建议的日志然后跳过就好。
///
/// 三条路径，按写法分流：
///
/// 1. **含 `/` 的包内相对路径**——照直在宠物包里找（`"sounds/done.wav"`）。
/// 2. **`@` 开头的语义名**——依次试：用户自选的文件 → 应用自带的兜底音效 →
///    当前平台的系统音效。前两层是**文件**、随安装包走，所以两个平台上听感一致；
///    系统音效那层只做保命（开发模式没有资源目录时仍然能响）。
/// 3. **裸名字**——先当宠物包里的同名文件（`"sound": "done.wav"` 是包作者最自然的
///    写法），再当当前平台的系统音效名（`"sound": "Glass"`）。
///
/// 第 3 条的顺序不能反过来。反了之后 `"done.wav"` 会被当成一个叫 `done.wav`
/// 的系统音效，解析不到、静默失声，而它在配置里看起来完全没错。
pub fn resolve(sound: &Sound, sources: &Sources<'_>) -> Option<Hit> {
    match sound {
        Sound::Pack(relative) => Hit::at(Layer::Pack, pack_file(sources.pack_root, relative)),
        Sound::System(name) if name.starts_with(SEMANTIC_PREFIX) => {
            semantic_file(name, sources).or_else(|| Hit::at(Layer::System, system_sound(name)))
        }
        Sound::System(name) => Hit::at(Layer::Pack, pack_file(sources.pack_root, name))
            .or_else(|| Hit::at(Layer::System, system_sound(name))),
    }
}

/// 语义名的前两层：用户自选的文件，然后是应用自带的兜底。
///
/// **语义名不进宠物包**：包里那个叫 `@done` 的文件只会是误会，而包作者要指定
/// 某个文件时有路径写法（第 1 条）。这也意味着用户配置只裁决语义槽位，
/// 不会去劫持包作者点名的具体文件（见 `user_choice_does_not_hijack_an_explicit_pack_path`）。
fn semantic_file(name: &str, sources: &Sources<'_>) -> Option<Hit> {
    Hit::at(Layer::User, user_file(sources.user, name))
        .or_else(|| Hit::at(Layer::Bundled, bundled_file(sources.bundled, name)))
}

/// 用户为某个语义名挑的文件。
///
/// 没配、或者配的那个文件已经不在了，都算 `None`——**不报错**。同一份
/// `config.json` 会在两台机器上被读到，另一台机器配的路径在这台必然不存在，
/// 那时正确答案是「按没配处理」，而不是「不出声」。
fn user_file(user: Option<&SoundFiles>, name: &str) -> Option<PathBuf> {
    let chosen = match name {
        SEMANTIC_DONE => &user?.done,
        SEMANTIC_FAILED => &user?.failed,
        SEMANTIC_ATTENTION => &user?.attention,
        _ => return None,
    };
    let path = PathBuf::from(chosen.trim());
    path.is_file().then_some(path)
}

/// 应用自带的兜底音效：文件名就是语义名本身（`sounds/@done.wav`）。
///
/// 这一层是「Windows 上不再是个哑巴」的保证：它随安装包走，不依赖系统装了什么、
/// 也不依赖宠物包作者有没有带音效。
fn bundled_file(bundled: Option<&Path>, name: &str) -> Option<PathBuf> {
    let dir = bundled?;
    SOUND_EXTENSIONS
        .iter()
        .map(|ext| dir.join(format!("{name}.{ext}")))
        .find(|path| path.is_file())
}

/// 在宠物包根下找一个相对路径，要求它真实存在。
fn pack_file(pack_root: Option<&Path>, relative: &str) -> Option<PathBuf> {
    let root = pack_root?;
    let path = root.join(relative);
    path.is_file().then_some(path)
}

/// 系统音效：先按语义名展开，再逐个目录、逐个扩展名找。
fn system_sound(name: &str) -> Option<PathBuf> {
    let candidates = semantic_names(name).unwrap_or_else(|| vec![name.to_string()]);
    let dirs = system_sound_dirs();
    candidates
        .iter()
        .flat_map(|candidate| {
            dirs.iter().flat_map(move |dir| {
                SOUND_EXTENSIONS
                    .iter()
                    .map(move |ext| (dir, candidate, ext))
            })
        })
        .map(|(dir, candidate, ext)| dir.join(format!("{candidate}.{ext}")))
        .find(|path| path.is_file())
}

/// `@done` 这类语义名展开成当前平台的一组候选名；普通名字返回 `None`。
fn semantic_names(name: &str) -> Option<Vec<String>> {
    if !name.starts_with(SEMANTIC_PREFIX) {
        return None;
    }
    SEMANTICS
        .iter()
        .find(|(semantic, _)| *semantic == name)
        .map(|(_, names)| names.iter().map(|name| (*name).to_string()).collect())
}

/// 当前平台查找系统音效的目录（按序）。
#[cfg(target_os = "macos")]
fn system_sound_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/System/Library/Sounds")];
    // 用户自己塞进去的音效优先级更高：后加的排在前面。
    if let Some(home) = std::env::var_os("HOME") {
        dirs.insert(0, PathBuf::from(home).join("Library/Sounds"));
    }
    dirs.insert(0, PathBuf::from("/Library/Sounds"));
    dirs
}

/// Windows：系统音效都在 `Media` 目录里。
#[cfg(target_os = "windows")]
fn system_sound_dirs() -> Vec<PathBuf> {
    ["SystemRoot", "windir"]
        .iter()
        .filter_map(|key| std::env::var_os(key))
        .map(|root| PathBuf::from(root).join("Media"))
        .collect()
}

/// 其余平台（Linux 等）：按 freedesktop 与 GNOME 的常规位置找。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn system_sound_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/share/sounds/freedesktop/stereo"),
        PathBuf::from("/usr/share/sounds/gnome/default/alerts"),
    ]
}

/// 声音通道。
///
/// 挡在 trait 后面是为了让测试**绝不出声**：真播放器会敲音频设备，
/// 而 CI 与开发机上都不该因为跑测试而响一声。
pub trait Speaker: Send + Sync {
    /// 播放一个已经解析成绝对路径的音效。
    fn play(&self, path: &Path, volume: f32) -> Result<()>;
}

/// 什么都不做的播放器：测试用。
#[cfg(test)]
#[derive(Debug, Default)]
pub struct SilentSpeaker;

#[cfg(test)]
impl Speaker for SilentSpeaker {
    fn play(&self, _path: &Path, _volume: f32) -> Result<()> {
        Ok(())
    }
}

/// 一条播放命令。
struct Command {
    /// 已经解析好的绝对路径。
    path: PathBuf,
    /// 音量，调用方保证已夹到 `0.0..=1.0`。
    volume: f32,
}

/// rodio 播放器句柄。
///
/// 它只持有一个 `Sender`：真正的音频流在 [`run_audio_thread`] 里创建并持有。
#[derive(Debug)]
pub struct RodioSpeaker {
    /// 发往音频线程的命令通道。
    tx: Sender<Command>,
}

impl RodioSpeaker {
    /// 启动音频线程。启动本身不会失败——真正的失败（没有音频设备）
    /// 会在第一次播放时以日志形式暴露，而不是拦住 daemon 启动。
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        // 线程要命名：它会出现在崩溃报告与采样里，匿名线程查起来很痛苦。
        let spawned = thread::Builder::new()
            .name("litepet-audio".to_string())
            .spawn(move || run_audio_thread(&rx));
        if let Err(error) = spawned {
            log::warn!("音频线程启动失败，本次运行不会有声音：{error}");
        }
        Self { tx }
    }
}

impl Speaker for RodioSpeaker {
    fn play(&self, path: &Path, volume: f32) -> Result<()> {
        self.tx
            .send(Command {
                path: path.to_path_buf(),
                volume: clamp_volume(volume),
            })
            .map_err(|_| anyhow!("音频线程已退出"))
    }
}

/// 把音量夹到 `0.0..=1.0`。
///
/// 不报错而是夹住：配置文件里手写 `volume: 1.5` 是个容易犯的小错，
/// 为它把整条提醒链路憋死不值得。
///
/// NaN 单独归到 [`VOLUME_MAX`]：`f32::clamp` 对 NaN 会原样返回 NaN，
/// 而 NaN 增益在音频链路上可能变成永久静音——"以后再也不会响了"比"太响了"难查得多。
/// （正常路径拿不到 NaN：音量来自 JSON，JSON 没有 NaN。）
fn clamp_volume(volume: f32) -> f32 {
    if volume.is_nan() {
        return VOLUME_MAX;
    }
    volume.clamp(VOLUME_MIN, VOLUME_MAX)
}

/// 音频线程主循环：懒打开设备，之后每来一条命令播一次。
fn run_audio_thread(rx: &mpsc::Receiver<Command>) {
    let mut sink: Option<MixerDeviceSink> = None;
    for command in rx {
        if sink.is_none() {
            match DeviceSinkBuilder::open_default_sink() {
                Ok(opened) => sink = Some(opened),
                Err(error) => {
                    log::warn!("打开音频设备失败，本次不响：{error}");
                    continue;
                }
            }
        }
        // 上面刚保证过 `Some`；这里用 `if let` 而不是 `unwrap` 只是不想让 panic 留在音频线程上。
        if let Some(sink) = sink.as_ref() {
            if let Err(error) = play_once(sink, &command) {
                log::warn!("播放音效 {} 失败：{error:#}", command.path.display());
            }
        }
    }
}

/// 播一次并立刻脱手，不阻塞音频线程——多个音效连发时不会互相排队。
fn play_once(sink: &MixerDeviceSink, command: &Command) -> Result<()> {
    let source = rodio::Decoder::try_from(File::open(&command.path)?)?;
    // 用 `Player` 而不是 `rodio::stream::play`：后者要求音源实现 `Seek`，
    // 而 `Decoder` 不实现。`append` 只要求 `Source`，正是我们要的粒度。
    let player = Player::connect_new(sink.mixer());
    player.set_volume(command.volume);
    player.append(source);
    // 立刻脱手：音频线程要能马上接下一个音效，而不是等这一个放完。
    player.detach();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只关心宠物包那一层的用例走这个，省得每处都搭一个 [`Sources`]。
    ///
    /// 丢掉 [`Hit`] 的来路：来路由 `each_layer_is_reported_in_order` 专门钉住。
    fn resolve_in_pack(sound: &Sound, pack_root: Option<&Path>) -> Option<PathBuf> {
        resolve(
            sound,
            &Sources {
                pack_root,
                ..Sources::default()
            },
        )
        .map(|hit| hit.path)
    }

    #[test]
    fn slash_means_pack_file() {
        assert_eq!(
            Sound::parse("sounds/done.wav"),
            Sound::Pack("sounds/done.wav".to_string())
        );
        assert_eq!(
            Sound::parse("  sounds\\done.wav "),
            Sound::Pack("sounds\\done.wav".to_string())
        );
    }

    #[test]
    fn bare_name_means_system_sound() {
        assert_eq!(Sound::parse("Glass"), Sound::System("Glass".to_string()));
        assert_eq!(Sound::parse("@done"), Sound::System("@done".to_string()));
    }

    #[test]
    fn pack_sound_requires_a_root() {
        let sound = Sound::parse("sounds/done.wav");
        assert_eq!(resolve_in_pack(&sound, None), None);
        assert_eq!(resolve_in_pack(&sound, Some(Path::new("/nowhere"))), None);
    }

    #[test]
    fn pack_sound_resolves_against_root() {
        let dir = std::env::temp_dir().join("litepet-sound-resolve-test");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let file = dir.join("done.wav");
        std::fs::write(&file, b"not really audio").expect("写临时文件");

        let sound = Sound::parse("done.wav");
        assert_eq!(resolve_in_pack(&sound, Some(&dir)), Some(file));

        let missing = Sound::parse("nope.wav");
        assert_eq!(resolve_in_pack(&missing, Some(&dir)), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 裸名字 `done.wav` 是包作者最自然的写法，必须先被当成包内文件。
    ///
    /// 这条以前是反的：`done.wav` 被当成一个叫 `done.wav` 的系统音效，
    /// 于是配置看着完全正确、却一点声音也没有。这个测试就是为了钉住那个回归。
    #[test]
    fn bare_filename_prefers_the_pack_file() {
        let dir = std::env::temp_dir().join("litepet-sound-bare-name");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let file = dir.join("done.wav");
        std::fs::write(&file, b"not really audio").expect("写临时文件");

        let sound = Sound::parse("done.wav");
        assert_eq!(
            sound,
            Sound::System("done.wav".to_string()),
            "裸名字归到 System"
        );
        let hit = resolve(
            &sound,
            &Sources {
                pack_root: Some(&dir),
                ..Sources::default()
            },
        )
        .expect("应命中包内文件");
        assert_eq!(hit.layer, Layer::Pack, "包作者点名的文件应报 pack");
        assert_eq!(hit.path, file);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 包里没有就叫不响吗？不，还要回头看系统音效。
    #[cfg(target_os = "macos")]
    #[test]
    fn bare_name_falls_back_to_the_system_sound() {
        let dir = std::env::temp_dir().join("litepet-sound-fallback");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let resolved = resolve_in_pack(&Sound::System("Glass".to_string()), Some(&dir))
            .expect("应退回系统音效");
        // 不写死扩展名：用户自己的 `~/Library/Sounds` 能覆盖系统默认。
        assert_eq!(
            resolved.file_stem().and_then(|stem| stem.to_str()),
            Some("Glass"),
            "应命中名为 Glass 的系统音效：{resolved:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `@done` 是语义名，不该去包里找一个叫 `@done` 的文件。
    #[cfg(target_os = "macos")]
    #[test]
    fn semantic_name_never_looks_inside_the_pack() {
        let dir = std::env::temp_dir().join("litepet-sound-semantic");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        // 放一个同名陷阱。如果解析顺序错了，它会被命中。
        std::fs::write(dir.join(SEMANTIC_DONE), b"trap").expect("写陷阱文件");

        let resolved =
            resolve_in_pack(&Sound::parse(SEMANTIC_DONE), Some(&dir)).expect("应解析到系统音效");
        assert!(
            !resolved.starts_with(&dir),
            "语义名不该命中包里的同名陷阱：{resolved:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 一套临时素材：用户挑的、应用兜底的、宠物包里放的同名陷阱。
    ///
    /// 三处都建出来，是为了让每个用例都能断言**到底命中了哪一层**——
    /// 只断言「解析成功」是分不出层的。
    struct Fixture {
        base: PathBuf,
        user: PathBuf,
        bundled: PathBuf,
        pack: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!("litepet-sound-{tag}"));
            let user = base.join("user");
            let bundled = base.join("bundled");
            let pack = base.join("pack");
            for dir in [&user, &bundled, &pack] {
                std::fs::create_dir_all(dir).expect("建临时目录");
            }
            // 用户那份和兜底那份的名字故意不同，好分辨命中了哪层。
            std::fs::write(user.join("mine.wav"), b"user").expect("写用户音效");
            std::fs::write(bundled.join(format!("{SEMANTIC_DONE}.wav")), b"bundled")
                .expect("写兜底音效");
            Self {
                base,
                user,
                bundled,
                pack,
            }
        }

        /// 用户配置里三个语义槽位都指向自己那个文件。
        fn files(&self) -> SoundFiles {
            let mine = self.user.join("mine.wav").display().to_string();
            SoundFiles {
                done: mine.clone(),
                failed: mine.clone(),
                attention: mine,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.base).ok();
        }
    }

    /// 用户自己挑的音效优先于应用自带的兜底——这是「我想听哪个」的最终裁决。
    #[test]
    fn user_choice_wins_over_the_bundled_sound() {
        let fx = Fixture::new("user-wins");
        let user = fx.files();
        let resolved = resolve(
            &Sound::parse(SEMANTIC_DONE),
            &Sources {
                user: Some(&user),
                pack_root: Some(&fx.pack),
                bundled: Some(&fx.bundled),
            },
        );
        assert_eq!(resolved.map(|hit| hit.path), Some(fx.user.join("mine.wav")));
    }

    /// 用户挑的音效对**包作者点名的具体文件**不生效。
    ///
    /// `sounds/special.wav` 是明确指定，让一个全局配置去覆盖它只会让包失去表达能力。
    #[test]
    fn user_choice_does_not_hijack_an_explicit_pack_path() {
        let fx = Fixture::new("explicit-path");
        let user = fx.files();
        std::fs::create_dir_all(fx.pack.join("sounds")).expect("建包内 sounds/");
        std::fs::write(fx.pack.join("sounds/special.wav"), b"pack").expect("写包内音效");
        let resolved = resolve(
            &Sound::parse("sounds/special.wav"),
            &Sources {
                user: Some(&user),
                pack_root: Some(&fx.pack),
                bundled: Some(&fx.bundled),
            },
        );
        assert_eq!(
            resolved.map(|hit| hit.path),
            Some(fx.pack.join("sounds/special.wav"))
        );
    }

    /// 同一份 `config.json` 会在两台机器上被读到，另一台机器配的路径在这台必然不存在。
    /// 那时要**按没配处理**往下走，而不是「路径不对所以不出声」。
    #[test]
    fn a_missing_user_choice_falls_through_to_the_bundled_sound() {
        let fx = Fixture::new("user-missing");
        let user = SoundFiles {
            done: fx.user.join("gone.wav").display().to_string(),
            ..SoundFiles::default()
        };
        let resolved = resolve(
            &Sound::parse(SEMANTIC_DONE),
            &Sources {
                user: Some(&user),
                bundled: Some(&fx.bundled),
                ..Sources::default()
            },
        );
        assert_eq!(
            resolved.map(|hit| hit.path),
            Some(fx.bundled.join(format!("{SEMANTIC_DONE}.wav")))
        );
    }

    /// 「Windows 上不再是个哑巴」靠的就是兜底这一层：系统里没有 `Glass`、`Basso`
    /// 那种名字，兜底音效却随安装包一起走。
    ///
    /// 这个用例不依赖任何系统音效，因此在三个平台上都能跑。
    #[test]
    fn bundled_sound_keeps_all_three_semantics_alive() {
        let fx = Fixture::new("bundled-three");
        // 文件名就是语义名本身（`@failed.wav`），和仓库的 `assets/sounds` 一致。
        // 写成 `failed.wav` 就永远命不中——这正是这条用例存在的理由。
        for semantic in [SEMANTIC_FAILED, SEMANTIC_ATTENTION] {
            std::fs::write(fx.bundled.join(format!("{semantic}.wav")), b"bundled")
                .expect("写兜底音效");
        }
        // 只给兜底目录：用户没配、也没有宠物包。
        let sources = Sources {
            bundled: Some(&fx.bundled),
            ..Sources::default()
        };
        for semantic in [SEMANTIC_DONE, SEMANTIC_FAILED, SEMANTIC_ATTENTION] {
            let hit = resolve(&Sound::parse(semantic), &sources)
                .unwrap_or_else(|| panic!("{semantic} 应命中自带的兜底音效"));
            assert!(
                hit.path.starts_with(&fx.bundled),
                "{semantic} 命中了兜底目录以外的文件：{:?}",
                hit.path
            );
            assert_eq!(hit.layer, Layer::Bundled, "{semantic} 的来路应是 bundled");
        }
    }

    /// 同一层优先顺序：[`Layer`] 要如实报告「现在响的到底是谁」。
    ///
    /// 设置页靠它回答「我挑的那个生效了吗」——只听声音是分不出来的，
    /// 自带的兜底与用户挑的文件听起来一样。
    #[test]
    fn each_layer_is_reported_in_order() {
        let fx = Fixture::new("layers");
        let user = fx.files();
        let sources = Sources {
            user: Some(&user),
            pack_root: Some(&fx.pack),
            bundled: Some(&fx.bundled),
        };
        let hit = resolve(&Sound::parse(SEMANTIC_DONE), &sources).expect("用户挑的那个");
        assert_eq!(hit.layer, Layer::User);
        assert_eq!(hit.path, fx.user.join("mine.wav"));

        // 把用户槽位清掉：掉到自带兜底。
        let empty = SoundFiles::default();
        let hit = resolve(
            &Sound::parse(SEMANTIC_DONE),
            &Sources {
                user: Some(&empty),
                ..sources
            },
        )
        .expect("自带兜底");
        assert_eq!(hit.layer, Layer::Bundled);
    }

    /// 前两层都没有时掉到系统音效，并且如实报告。
    ///
    /// 只在 macOS 上有保证（Linux 得装上声音主题、Windows 的名字对不上）。
    #[cfg(target_os = "macos")]
    #[test]
    fn system_layer_is_reported() {
        let hit = resolve(
            &Sound::parse(SEMANTIC_DONE),
            &Sources {
                user: Some(&SoundFiles::default()),
                ..Sources::default()
            },
        )
        .expect("macOS 上应能解析出系统音效");
        assert_eq!(hit.layer, Layer::System);
    }

    #[test]
    fn semantic_names_expand_and_unknown_ones_do_not() {
        assert!(
            semantic_names(SEMANTIC_DONE).is_some_and(|names| names.contains(&"Glass".to_string()))
        );
        assert!(semantic_names("@nonsense").is_none());
        assert!(semantic_names("Glass").is_none(), "普通名字不做任何展开");
    }

    /// macOS 上 `Glass` 确实存在——这是「名字写法真的能work」的证据，
    /// 而不是「我们以为它能 work」。
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_has_the_semantic_sounds_we_ship() {
        for semantic in [SEMANTIC_DONE, SEMANTIC_FAILED, SEMANTIC_ATTENTION] {
            let resolved = system_sound(semantic);
            assert!(resolved.is_some(), "macOS 上应能解析出 {semantic}");
        }
    }

    #[test]
    fn silent_speaker_never_touches_audio() {
        let speaker = SilentSpeaker;
        assert!(speaker.play(Path::new("/does/not/exist.wav"), 0.5).is_ok());
        // 也用 trait 对象调一次，确认提醒层能把它当 `Box<dyn Speaker>` 存着。
        let boxed: Box<dyn Speaker> = Box::new(SilentSpeaker);
        assert!(boxed.play(Path::new("/does/not/exist.ogg"), 0.1).is_ok());
    }

    /// 音量越界是夹住而不是报错。
    ///
    /// 这里不去建 `RodioSpeaker`：那会真去打开音频设备，而测试不该动系统音频。
    #[test]
    fn volume_is_clamped_not_rejected() {
        assert_eq!(clamp_volume(5.0), VOLUME_MAX);
        assert_eq!(clamp_volume(-1.0), VOLUME_MIN);
        assert_eq!(clamp_volume(0.35), 0.35);
        assert_eq!(clamp_volume(f32::NAN), VOLUME_MAX, "NaN 不能漏出去");
    }
}
