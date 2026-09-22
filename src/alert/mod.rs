//! 提醒层：声音、系统通知、手机推送。
//!
//! 它在渲染管线里是**旁路**，不是第二套状态机：
//!
//! ```text
//! 宿主事件 → 仲裁层（优先级/抢占/忙闲）
//!               ├─→ 渲染层：宠物动画 + 气泡       （已有）
//!               └─→ 提醒层：声音/通知/推送        （本模块）
//! ```
//!
//! 分工是死的：
//!
//! - **宠物包的规则表**决定「这个事件要不要提醒、响什么、要不要推手机」
//!   （`alert` 字段，见 `docs/PET-PACK.md` §4.5）。一份规则表同时驱动动画与提醒。
//! - **用户配置**只放全局开关、凭据与阈值（`config.json` 的 `notify` 段）。
//! - **本模块**按「人到底在不在」把两者收敛成实际动作（[`plan`]），再交给通道执行。
//!
//! 「人在不在」不由本模块判断：那是宿主的活（它们自己就是前台进程，还知道会话状态）。
//! litepet 只接受请求、发通知。各通道的真实现见 [`sound`]、[`desktop`]、[`push`]。
//! 它们都被 trait 抽象，所以测试里可以换成不碰系统的假实现。

pub mod desktop;
pub mod push;
pub mod sound;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::{bail, Result};

use crate::config::NotifyConfig;
use desktop::Notifier;
use push::Pusher;
use sound::{Layer, Sound, Speaker};

/// 规则表里 `alert` 字段的内容（`docs/PET-PACK.md` §4.5）。
///
/// 省略整个 `alert` 键 = 这组规则不提醒。给了 `alert` 之后，各字段的含义是：
///
/// ```json
/// { "alert": { "sound": "Glass", "desktop": true, "push": true } }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AlertSpec {
    /// 音效：不含 `/` 视为平台系统音效名，含 `/` 视为**包内**相对路径。
    pub sound: Option<String>,
    /// 是否弹系统通知。
    pub desktop: bool,
    /// 是否推手机。
    pub push: bool,
}

impl Default for AlertSpec {
    /// 给了 `alert` 但没写字段时的缺省：只弹系统通知，不响不推。
    fn default() -> Self {
        Self {
            sound: None,
            desktop: true,
            push: false,
        }
    }
}

impl AlertSpec {
    /// 校验取值合法性；在包加载期调用，把拼错挡在起跑线上。
    pub fn validate(&self, context: &str) -> Result<()> {
        if let Some(name) = self.sound.as_deref() {
            if name.trim().is_empty() {
                bail!("{context} 的 alert.sound 不能为空字符串");
            }
        }
        Ok(())
    }

    /// 这份规格是不是什么也不做——用来在求值结果里顺手剔掉空壳。
    pub fn is_inert(&self) -> bool {
        self.sound.is_none() && !self.desktop && !self.push
    }
}

/// 一次提醒请求：规则表已经定了规格，正文来自事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// 规则表给的规格。
    pub spec: AlertSpec,
    /// 通知标题。
    pub title: String,
    /// 通知正文。
    pub body: String,
}

impl Request {
    /// 构造一条请求。
    pub fn new(spec: AlertSpec, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            spec,
            title: title.into(),
            body: body.into(),
        }
    }
}

/// 收敛后的动作。
///
/// 没有「人在不在」这个变量：判断哪个通道该响是宿主的活。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Actions {
    /// 要播的音效标识；`None` 不响。
    pub sound: Option<String>,
    /// 是否弹系统通知。
    pub desktop: bool,
    /// 是否推手机。
    pub push: bool,
}

impl Actions {
    /// 三个通道都不做事。
    pub fn is_noop(&self) -> bool {
        self.sound.is_none() && !self.desktop && !self.push
    }
}

/// 决策：把「规则表的规格」与「用户配置」收敛成实际动作。
///
/// 这是整个提醒层唯一有判断的地方，**纯函数**，所以可以直接单测。
///
/// 规则：
///
/// - 总开关关掉 → 什么都不做；
/// - 声音：规格要响 **且** 声音通道开着；
/// - 系统通知：规格要点 **且** 该通道开着；
/// - 手机推送：规格要推 **且** 该通道开着。
///
/// 三个通道彼此独立——关掉一个不该连带影响另两个。
pub fn plan(cfg: &NotifyConfig, spec: &AlertSpec) -> Actions {
    if !cfg.enabled {
        return Actions::default();
    }
    let sound = if cfg.sound.enabled {
        spec.sound.clone()
    } else {
        None
    };
    let desktop = spec.desktop && cfg.desktop.enabled;
    let push = spec.push && cfg.push.enabled;
    Actions {
        sound,
        desktop,
        push,
    }
}

/// 三个通道的集合。
///
/// 用 `Arc` 而不是 `Box`：提醒要扔到后台线程去发，而会话线程不能等它。
#[derive(Clone)]
pub struct Channels {
    /// 声音。
    pub speaker: Arc<dyn Speaker>,
    /// 系统通知。
    pub notifier: Arc<dyn Notifier>,
    /// 手机推送。
    pub pusher: Arc<dyn Pusher>,
}

/// 提醒层的执行端。
///
/// 它把「配置」「素材来源」「三个通道」卷在一起，对外只暴露两个动作：
/// [`Alerter::fire`]（异步，给会话线程用）与 [`Alerter::dispatch`]（同步，给测试用）。
#[derive(Clone)]
pub struct Alerter {
    /// 用户配置里的 `notify` 段。
    config: NotifyConfig,
    /// 宠物包根目录，用于解析 `alert.sound` 里的相对路径。
    pack_root: Option<PathBuf>,
    /// 应用自带的兜底音效目录。
    bundled_sounds: Option<PathBuf>,
    /// 三个通道。
    channels: Channels,
}

/// 构造 [`Alerter`] 需要的素材来源。
///
/// 位置参数到第三个就该收手：配置、宠物包、应用资源三样东西一路往下传，
/// 读的人得回去数顺序。
pub struct AlertSources {
    /// 用户配置里的 `notify` 段。
    pub config: NotifyConfig,
    /// 宠物包根目录，用于解析 `alert.sound` 里的相对路径。
    pub pack_root: Option<PathBuf>,
    /// 应用自带的兜底音效目录；开发模式下拿不到资源目录时是 `None`。
    pub bundled_sounds: Option<PathBuf>,
}

impl Alerter {
    /// 构造。
    pub fn new(sources: AlertSources, channels: Channels) -> Self {
        Self {
            config: sources.config,
            pack_root: sources.pack_root,
            bundled_sounds: sources.bundled_sounds,
            channels,
        }
    }

    /// 发一条提醒。**立刻返回**，不阻塞调用方。
    ///
    /// 播声音要等解码器起来，推送要出网，两者都可能慢上几拍。这些一律丢给后台线程：
    /// 会话线程的任务是处理下一个协议事件，不是等声音放完。
    pub fn fire(&self, request: Request) {
        if request.spec.is_inert() {
            return;
        }
        let worker = self.clone();
        // 线程要命名：它会出现在采样与崩溃报告里，而提醒恰好是最容易出问题的一环。
        let spawned = thread::Builder::new()
            .name("litepet-alert".to_string())
            .spawn(move || {
                worker.dispatch(&request);
            });
        if let Err(error) = spawned {
            log::warn!("提醒线程启动失败，本次不提醒：{error}");
        }
    }

    /// 按既定决策执行应做的动作，返回实际做了什么。
    pub fn dispatch(&self, request: &Request) -> Actions {
        let actions = plan(&self.config, &request.spec);
        if actions.is_noop() {
            return actions;
        }
        if let Some(raw) = actions.sound.as_deref() {
            self.play_sound(raw);
        }
        if actions.desktop {
            if let Err(error) = self.channels.notifier.notify(&request.title, &request.body) {
                log::warn!("发系统通知失败：{error:#}");
            }
        }
        if actions.push {
            if let Err(error) = self.channels.pusher.push(&request.title, &request.body) {
                log::warn!("推手机失败：{error:#}");
            }
        }
        actions
    }

    /// 解析并播放一个音效。
    ///
    /// 声音是三个通道里最不重要的一环：找不到文件只记一条日志，
    /// 绝不因此把系统通知和推送一起吞掉。
    fn play_sound(&self, raw: &str) {
        let sound = Sound::parse(raw);
        match self.resolve_path(&sound) {
            Some(hit) => self.play_file(&hit.path),
            None => log::warn!(
                "找不到音效「{raw}」：{hint}",
                hint = missing_sound_hint(&sound)
            ),
        }
    }

    /// 把一条音效写法解析成磁盘上真实的文件；解析规则见 [`sound::resolve`]。
    fn resolve_path(&self, sound: &Sound) -> Option<sound::Hit> {
        sound::resolve(
            sound,
            &sound::Sources {
                // 用户自选的那一层只裁决语义槽位（由 `sound::resolve` 决定）。
                user: Some(&self.config.sound.files),
                pack_root: self.pack_root.as_deref(),
                bundled: self.bundled_sounds.as_deref(),
            },
        )
    }

    /// 播一个已经解析好的文件；失败只记日志。
    fn play_file(&self, path: &Path) {
        if let Err(error) = self.channels.speaker.play(path, self.config.sound.volume) {
            log::warn!("播放音效 {} 失败：{error:#}", path.display());
        }
    }
}

/// 一次试听的结果。
///
/// 与 [`Alerter::dispatch`] 分开放在两个 `impl Alerter` 块里：试听是**设置页专用**
/// 的一条旁路，不是第四个提醒通道，分开写免得读的人把它当通道数。
///
/// 不带 JSON：本模块不认识 HTTP 那层的形状，拼响应是 `main` 的活。
pub struct Preview {
    /// 实际解析到的文件；`None` 表示这条写法在当前配置与平台上没有可播的文件。
    pub path: Option<PathBuf>,
    /// 命中文件来自哪一层；没有命中就是 `None`。
    pub layer: Option<Layer>,
    /// 没命中时给用户的一句建议。
    pub hint: Option<&'static str>,
}

impl Alerter {
    /// 试听一个音效：**只出声**，不发系统通知、也不推送。
    ///
    /// 与正式提醒共用同一个解析器（[`sound::resolve`]），但**不看开关**：
    /// `notify.enabled` 与 `notify.sound.enabled` 都关着也照放。理由是试听的语义是
    /// 「这个文件能不能响」，而不是「现在会不会响」；把开关叠上去，用户点了没声音
    /// 就分不出是文件不对还是开关不对。
    ///
    /// 返回值里带回命中路径与来路：设置页需要能说出「现在响的是你挑的，还是自带的兜底」。
    pub fn preview(&self, raw: &str) -> Preview {
        let sound = Sound::parse(raw);
        match self.resolve_path(&sound) {
            Some(hit) => {
                self.play_file(&hit.path);
                Preview {
                    path: Some(hit.path),
                    layer: Some(hit.layer),
                    hint: None,
                }
            }
            None => {
                let hint = missing_sound_hint(&sound);
                log::warn!("试听「{raw}」找不到文件：{hint}");
                Preview {
                    path: None,
                    layer: None,
                    hint: Some(hint),
                }
            }
        }
    }
}

/// 音效找不到时给出一条能照着做的提示。
///
/// 「静默失声」是这套东西里最难查的故障：不把建议写清楚，
/// 用户只会觉得「声音功能坏了」。
fn missing_sound_hint(sound: &Sound) -> &'static str {
    match sound {
        Sound::Pack(_) => "宠物包里没有这个文件，请检查 pet.json 里的相对路径",
        Sound::System(_) => "当前平台上没有这个系统音效；建议把音效文件放进宠物包，用相对路径引用",
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;

    use super::*;
    use sound::SEMANTIC_DONE;

    /// 造一份全开的配置，按需关掉某项。
    fn cfg() -> NotifyConfig {
        let mut cfg = NotifyConfig::default();
        cfg.push.enabled = true;
        cfg.push.device_key = "test-key".to_string();
        cfg
    }

    fn spec(sound: Option<&str>, desktop: bool, push: bool) -> AlertSpec {
        AlertSpec {
            sound: sound.map(str::to_string),
            desktop,
            push,
        }
    }

    /// 规格里要什么就发什么：三个通道各干各的。
    #[test]
    fn all_requested_channels_fire() {
        let actions = plan(&cfg(), &spec(Some("Glass"), true, true));
        assert_eq!(actions.sound.as_deref(), Some("Glass"));
        assert!(actions.desktop);
        assert!(actions.push);
    }

    /// 规格没要的通道不发，不管配置开得多全。
    #[test]
    fn spec_without_sound_stays_silent() {
        let actions = plan(&cfg(), &spec(None, true, true));
        assert!(actions.sound.is_none(), "规格没给音效就不响");
        assert!(actions.desktop);
        assert!(actions.push);
    }

    /// 三个开关分别能独立掐掉对应通道。
    #[test]
    fn global_switches_are_respected() {
        let mut off = cfg();
        off.enabled = false;
        assert!(plan(&off, &spec(Some("Glass"), true, true)).is_noop());

        let mut no_sound = cfg();
        no_sound.sound.enabled = false;
        let actions = plan(&no_sound, &spec(Some("Glass"), true, true));
        assert!(actions.sound.is_none());
        assert!(actions.desktop, "关声音不该连带关掉通知");

        let mut no_desktop = cfg();
        no_desktop.desktop.enabled = false;
        let actions = plan(&no_desktop, &spec(Some("Glass"), true, true));
        assert!(!actions.desktop);
        assert!(actions.push, "关系统通知不该连带关掉推送");

        let mut no_push = cfg();
        no_push.push.enabled = false;
        let actions = plan(&no_push, &spec(Some("Glass"), true, true));
        assert!(!actions.push);
        assert!(actions.desktop);
    }

    /// 规格里没要的通道，配置开着也不该做。
    #[test]
    fn spec_can_suppress_channels() {
        let actions = plan(&cfg(), &spec(None, false, false));
        assert!(actions.is_noop(), "规格什么都没要：{actions:?}");
    }

    #[test]
    fn empty_sound_name_is_rejected() {
        let bad = spec(Some("   "), false, false);
        assert!(bad.validate("规则 agent.settled").is_err());
        assert!(spec(Some("Glass"), false, false).validate("规则").is_ok());
        assert!(spec(None, false, false).validate("规则").is_ok());
    }

    /// 省略 `alert` 键与给一个空 `alert` 是两回事：后者仍会弹通知。
    #[test]
    fn bare_alert_key_still_notifies() {
        let bare: AlertSpec = serde_json::from_str("{}").expect("应能解析空对象");
        assert_eq!(bare, AlertSpec::default());
        assert!(bare.desktop);
        assert!(!bare.push);
        assert!(!bare.is_inert());
    }

    #[test]
    fn alert_spec_parses_camel_case() {
        let spec: AlertSpec =
            serde_json::from_str(r#"{"sound":"Hero","desktop":false,"push":true}"#)
                .expect("应能解析");
        assert_eq!(spec.sound.as_deref(), Some("Hero"));
        assert!(!spec.desktop);
        assert!(spec.push);
    }

    /// 一张「什么被调用了」的记账表。
    ///
    /// 整套测试都靠它：它既是声音、又是通知、又是推送，但什么都不做，
    /// 所以跑测试既不会响一声，也不会弹窗，更不会出网。
    #[derive(Default)]
    struct Recorder {
        sounds: Mutex<Vec<(PathBuf, f32)>>,
        desktop: Mutex<Vec<(String, String)>>,
        push: Mutex<Vec<(String, String)>>,
    }

    impl Speaker for Recorder {
        fn play(&self, path: &Path, volume: f32) -> Result<()> {
            self.sounds
                .lock()
                .expect("记账锁")
                .push((path.to_path_buf(), volume));
            Ok(())
        }
    }

    impl Notifier for Recorder {
        fn notify(&self, title: &str, body: &str) -> Result<()> {
            self.desktop
                .lock()
                .expect("记账锁")
                .push((title.to_string(), body.to_string()));
            Ok(())
        }
    }

    impl Pusher for Recorder {
        fn push(&self, title: &str, body: &str) -> Result<()> {
            self.push
                .lock()
                .expect("记账锁")
                .push((title.to_string(), body.to_string()));
            Ok(())
        }
    }

    /// 建一套记账版通道。
    fn alerter(cfg: NotifyConfig, pack_root: Option<PathBuf>) -> (Arc<Recorder>, Alerter) {
        let recorder = Arc::new(Recorder::default());
        // 必须分三行写：`Arc::clone` 自己不做 unsize 转换，
        // `Arc::clone(&recorder)` 会要求 `&Arc<dyn Speaker>` 而拒绝 `&Arc<Recorder>`。
        let speaker: Arc<dyn Speaker> = recorder.clone();
        let notifier: Arc<dyn Notifier> = recorder.clone();
        let pusher: Arc<dyn Pusher> = recorder.clone();
        let channels = Channels {
            speaker,
            notifier,
            pusher,
        };
        let sources = AlertSources {
            config: cfg,
            pack_root,
            bundled_sounds: None,
        };
        (recorder, Alerter::new(sources, channels))
    }

    /// 建一个临时目录，里面放一个真文件当音效。
    fn temp_sound_pack(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("litepet-alert-{name}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let file = dir.join("done.wav");
        std::fs::write(&file, b"not really audio").expect("写临时文件");
        (dir, file)
    }

    fn request(sound: Option<&str>, desktop: bool, push: bool) -> Request {
        Request::new(spec(sound, desktop, push), "LitePet", "pi 干完了")
    }

    /// 配置里挑的音效要真的被播出来。
    ///
    /// 这是「配置 → 播放路径」的接缝：`sound.rs` 的单测盖不到它，
    /// 而它恰好是“配了却不出声”这类 bug 的所在地。
    #[test]
    fn configured_sound_reaches_the_speaker() {
        let (pack_root, _) = temp_sound_pack("configured");
        let mine = pack_root.join("mine.wav");
        std::fs::write(&mine, b"not really audio").expect("写临时文件");
        let mut config = cfg();
        config.sound.files.done = mine.display().to_string();

        let (recorder, alerter) = alerter(config, Some(pack_root.clone()));
        alerter.dispatch(&request(Some("@done"), false, false));
        let played = recorder.sounds.lock().expect("锁").clone();
        // 记账的是（路径, 音量）对，这里只关心路径。
        assert_eq!(played.len(), 1, "应该只响一声：{played:?}");
        assert_eq!(played[0].0, mine);
        std::fs::remove_dir_all(&pack_root).ok();
    }

    /// 试听只出声：不发系统通知、也不推送，并且如实报出命中的来路。
    #[test]
    fn preview_only_plays_the_sound() {
        let (recorder, alerter) = alerter(cfg(), None);
        let preview = alerter.preview(SEMANTIC_DONE);
        assert!(
            recorder.desktop.lock().expect("锁").is_empty(),
            "试听不该发系统通知"
        );
        assert!(
            recorder.push.lock().expect("锁").is_empty(),
            "试听不该推手机"
        );
        match preview.path {
            Some(path) => {
                let played = recorder.sounds.lock().expect("锁").clone();
                assert_eq!(played.len(), 1, "命中了就该响一声");
                assert_eq!(played[0].0, path);
                assert!(preview.layer.is_some(), "命中了就必须报来路");
                assert!(preview.hint.is_none(), "命中了就不该给建议");
            }
            None => {
                // 平台上根本没有对应音效也算正常，但那就什么都别放。
                assert!(preview.layer.is_none());
                assert!(preview.hint.is_some(), "没命中必须给一条能照着做的建议");
                assert!(recorder.sounds.lock().expect("锁").is_empty());
            }
        }
    }

    /// 试听不看开关：总开关与音效开关都关着，试听也要出声。
    ///
    /// 这是刻意的：试听回答的是「这个文件能不能响」。要是也看开关，用户点了没声音
    /// 就分不出是文件不对、还是开关不对。（正式提醒仍然会老老实实看开关。）
    #[test]
    fn preview_ignores_the_switches() {
        let (pack_root, file) = temp_sound_pack("preview-switches");
        let mut config = cfg();
        config.enabled = false;
        config.sound.enabled = false;
        config.sound.files.done = file.display().to_string();

        let (recorder, alerter) = alerter(config, Some(pack_root.clone()));
        let preview = alerter.preview(SEMANTIC_DONE);
        assert_eq!(preview.path.as_deref(), Some(file.as_path()));
        assert_eq!(preview.layer, Some(Layer::User));
        assert_eq!(
            recorder.sounds.lock().expect("锁").len(),
            1,
            "开关关着也要出声：试听不看开关"
        );

        // 同一份配置走正式提醒就不出声了——上面那条断言才有意义。
        let actions = alerter.dispatch(&request(Some("@done"), false, false));
        assert!(actions.sound.is_none(), "正式提醒应被总开关拦下");
        assert_eq!(recorder.sounds.lock().expect("锁").len(), 1);
        std::fs::remove_dir_all(&pack_root).ok();
    }

    /// 只给声音的规格不该顺手把通知与推送也发出去。
    #[test]
    fn sound_only_spec_rings_alone() {
        let (recorder, alerter) = alerter(cfg(), None);
        let actions = alerter.dispatch(&request(Some("@done"), false, false));
        assert!(actions.sound.is_some());
        assert_eq!(recorder.sounds.lock().expect("锁").len(), 1);
        assert!(recorder.desktop.lock().expect("锁").is_empty());
        assert!(recorder.push.lock().expect("锁").is_empty());
    }

    #[test]
    fn all_three_channels_fire_when_requested() {
        let (recorder, alerter) = alerter(cfg(), None);
        // `@done` 在 macOS 上能解析到系统音效；其他平台上找不到就只是不响，
        // 但通知与推送照旧——这正是「声音最不重要」这条约定的作用。
        alerter.dispatch(&request(Some("@done"), true, true));
        assert_eq!(recorder.desktop.lock().expect("锁").len(), 1);
        assert_eq!(recorder.push.lock().expect("锁").len(), 1);
    }

    #[test]
    fn global_switch_off_touches_nothing() {
        let mut off = cfg();
        off.enabled = false;
        let (recorder, alerter) = alerter(off, None);
        let actions = alerter.dispatch(&request(Some("@done"), true, true));
        assert!(actions.is_noop());
        assert!(recorder.sounds.lock().expect("锁").is_empty());
        assert!(recorder.desktop.lock().expect("锁").is_empty());
        assert!(recorder.push.lock().expect("锁").is_empty());
    }

    #[test]
    fn pack_sound_is_resolved_and_played_at_configured_volume() {
        let (pack_root, expected) = temp_sound_pack("resolve");
        let (recorder, alerter) = alerter(cfg(), Some(pack_root.clone()));
        alerter.dispatch(&request(Some("done.wav"), false, false));

        let played = recorder.sounds.lock().expect("锁").clone();
        assert_eq!(played.len(), 1);
        assert_eq!(played[0].0, expected, "应该拿到包内那个真文件");
        assert_eq!(played[0].1, cfg().sound.volume, "音量应该来自配置");
        std::fs::remove_dir_all(&pack_root).ok();
    }

    /// 音效找不到不能连带把通知和推送吞掉。
    ///
    /// 这是提醒层最容易写错的地方，而它的症状（"没通知"）看起来像另一回事。
    #[test]
    fn missing_sound_does_not_block_the_other_channels() {
        let (recorder, alerter) = alerter(cfg(), None);
        alerter.dispatch(&request(Some("sounds/nonexistent.wav"), true, true));
        assert!(recorder.sounds.lock().expect("锁").is_empty());
        assert_eq!(recorder.desktop.lock().expect("锁").len(), 1);
        assert_eq!(recorder.push.lock().expect("锁").len(), 1);
    }

    #[test]
    fn inert_requests_are_dropped_before_any_thread_starts() {
        let (recorder, alerter) = alerter(cfg(), None);
        alerter.fire(request(None, false, false));
        assert!(recorder.sounds.lock().expect("锁").is_empty());
        assert!(recorder.desktop.lock().expect("锁").is_empty());
        assert!(recorder.push.lock().expect("锁").is_empty());
    }

    #[test]
    fn missing_sound_hints_point_at_the_real_fix() {
        assert!(missing_sound_hint(&Sound::Pack("a.wav".into())).contains("宠物包"));
        assert!(missing_sound_hint(&Sound::System("Glass".into())).contains("系统音效"));
    }
}
