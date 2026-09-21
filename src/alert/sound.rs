//! 音效通道：用 rodio 出声，并且绝不阻塞会话线程。
//!
//! 两个现实约束决定了这里的形状：
//!
//! 1. **cpal 的输出流不是 `Send`，而且一 drop 就断音。** 所以它不能躺在
//!    某个随时可能被移动或丢弃的句柄里，必须待在一个专用线程上。顺带的好处是
//!    「打开音频设备」被推到了第一次真要出声的时刻，而不是每次启动都敲一次 CoreAudio。
//! 2. **系统音效的名字不跨平台。** macOS 是 `Glass`，Windows 是 `Windows Notify`，
//!    Linux 是 `bell`。所以除了「原样写名字」之外，还认 `@done` 这类语义名，
//!    由本模块映射到当前平台。想要听感完全一致，就把音效文件放进包里用相对路径引用。
//! 3. **裸名字先当包内文件。** `"sound": "done.wav"` 是包作者最自然的写法，
//!    所以不含 `/` 的名字先到宠物包里找，找不到再当系统音效名——顺序见 [`resolve`]。

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::{anyhow, Result};
use rodio::stream::DeviceSinkBuilder;
use rodio::{MixerDeviceSink, Player};

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

/// 把音效解析成磁盘上真实存在的文件；找不到返回 `None`。
///
/// 找不到**不是错误**：用户从别的平台搬一个包过来、或者系统没装某个音效，
/// 都是很常见的事。调用方记一条带建议的日志然后跳过就好。
///
/// 裸名字有两种合法解释，按**包内优先**的顺序试：
///
/// 1. 宠物包里的同名文件——`"sound": "done.wav"` 是包作者最自然的写法；
/// 2. 当前平台的系统音效——`"sound": "Glass"`。
///
/// 顺序不能反过来。反了之后 `"done.wav"` 会被当成一个叫 `done.wav` 的系统音效，
/// 解析不到、静默失声，而它在配置里看起来完全没错。
/// `@` 开头的语义名只走系统音效，不去包里撞运气。
pub fn resolve(sound: &Sound, pack_root: Option<&Path>) -> Option<PathBuf> {
    match sound {
        Sound::Pack(relative) => pack_file(pack_root, relative),
        Sound::System(name) if name.starts_with(SEMANTIC_PREFIX) => system_sound(name),
        Sound::System(name) => pack_file(pack_root, name).or_else(|| system_sound(name)),
    }
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
        assert_eq!(resolve(&sound, None), None);
        assert_eq!(resolve(&sound, Some(Path::new("/nowhere"))), None);
    }

    #[test]
    fn pack_sound_resolves_against_root() {
        let dir = std::env::temp_dir().join("litepet-sound-resolve-test");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let file = dir.join("done.wav");
        std::fs::write(&file, b"not really audio").expect("写临时文件");

        let sound = Sound::parse("done.wav");
        assert_eq!(resolve(&sound, Some(&dir)), Some(file));

        let missing = Sound::parse("nope.wav");
        assert_eq!(resolve(&missing, Some(&dir)), None);
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
        assert_eq!(resolve(&sound, Some(&dir)), Some(file));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 包里没有就叫不响吗？不，还要回头看系统音效。
    #[cfg(target_os = "macos")]
    #[test]
    fn bare_name_falls_back_to_the_system_sound() {
        let dir = std::env::temp_dir().join("litepet-sound-fallback");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let resolved =
            resolve(&Sound::System("Glass".to_string()), Some(&dir)).expect("应退回系统音效");
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

        let resolved = resolve(&Sound::parse(SEMANTIC_DONE), Some(&dir)).expect("应解析到系统音效");
        assert!(
            !resolved.starts_with(&dir),
            "语义名不该命中包里的同名陷阱：{resolved:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
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
