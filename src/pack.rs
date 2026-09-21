//! 宠物包加载：解析 `pet.json`、校验网格、构造动画表。
//!
//! 包格式沿用 Codex（`codex-rs/tui/src/pets/`），契约见 `docs/PET-PACK.md`。
//! 分工：
//!
//! - [`manifest`]：`pet.json` 反序列化与网格校验
//! - [`image`]：图集文件头的尺寸解析
//! - [`animations`]：动画表构造（Codex 语义的忠实复现）
//!
//! 与 Codex 的唯一实质差异是**网格推断的宽容度**，理由见 [`manifest::resolve_grid`]。

mod animations;
mod image;
mod manifest;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 清单文件名。
const PET_FILE: &str = "pet.json";

/// 网格规格（与 Codex `FrameSpec` 同名同义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GridSpec {
    /// 单格宽（像素）。
    pub width: u32,
    /// 单格高（像素）。
    pub height: u32,
    /// 网格列数。
    pub columns: u32,
    /// 网格行数。
    pub rows: u32,
}

impl GridSpec {
    /// 网格总格数。
    pub fn frame_count(self) -> usize {
        self.columns as usize * self.rows as usize
    }
}

/// 渲染层需要的单帧信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FramePlan {
    /// 精灵索引（`row * columns + column`）。
    pub sprite_index: usize,
    /// 该帧显示时长（毫秒）。
    pub duration_ms: u64,
}

/// 单个动画的播放计划。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnimationPlan {
    /// 帧序列。
    pub frames: Vec<FramePlan>,
    /// 循环起点；`null` 表示一次性，播完交棒 `fallback`。
    pub loop_start: Option<usize>,
    /// 一次性动画播完后接续的动画名。
    pub fallback: String,
}

/// 发给前端的宠物包信息。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PetInfo {
    /// 宠物包 id。
    pub id: String,
    /// 展示名。
    pub display_name: String,
    /// 描述。
    pub description: String,
    /// 图集绝对路径。
    pub spritesheet_path: String,
    /// 网格规格。
    pub frame: GridSpec,
    /// 动画名 → 播放计划。
    pub animations: HashMap<String, AnimationPlan>,
}

/// 加载结果。
#[derive(Debug, Clone)]
pub struct LoadedPet {
    /// 渲染层需要的包信息。
    pub info: PetInfo,
    /// `litepet` 扩展键的原始 JSON（`docs/PET-PACK.md` §4.2），由行为模块解析。
    pub behavior: Option<serde_json::Value>,
    /// 包自己的目录 `pets/<id>/`。
    ///
    /// 提醒层要拿它解析 `alert.sound` 里的相对路径（`docs/PET-PACK.md` §4.5）：
    /// 包里自带音效是跨平台听感一致的唯一办法。
    pub root: PathBuf,
}

/// 从 `pets/<id>/` 加载一个包。
///
/// `pets_root` 为宠物包根目录，用于阻挡 `spritesheetPath` 逃出包目录。
pub fn load(pets_root: &Path, id: &str) -> Result<LoadedPet> {
    let dir = pets_root.join(id);
    let manifest_path = dir.join(PET_FILE);
    if !manifest_path.is_file() {
        bail!("缺失清单文件：{}", manifest_path.display());
    }
    let file = manifest::PetFile::from_path(&manifest_path)?;

    let sheet = resolve_spritesheet(&dir, pets_root, file.spritesheet_path.as_deref())?;
    let atlas = image::read_size(&sheet)?;
    let frame = manifest::resolve_grid(&file, atlas)?;
    for warning in manifest::grid_warnings(&file, frame) {
        log::warn!("包 {id} 告警：{warning}");
    }
    let animations = animations::build(frame, &file.animations)?;

    Ok(LoadedPet {
        info: PetInfo {
            id: file.id.clone().unwrap_or_else(|| id.to_string()),
            display_name: file.display_name.clone().unwrap_or_else(|| id.to_string()),
            description: file.description.clone().unwrap_or_default(),
            spritesheet_path: sheet.to_string_lossy().into_owned(),
            frame,
            animations,
        },
        behavior: file.litepet.clone(),
        root: dir,
    })
}

/// 配置页宠物列表里的一项：够显示、够选择，不求能渲染。
///
/// 与 [`PetInfo`] 的区别是**不建动画表**（那只在图集与网格都合法时才有意义），
/// 只读清单与图集文件头。列表必须能容忍个别坏包：一个包有问题不该让整列都拉不出来。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PetSummary {
    /// 目录名；`load` 用它作为清单缺 `id` 时的兜底。
    pub dir: String,
    /// 清单里的 id；缺省时回落成目录名（与 `load` 的取值一致）。
    pub id: String,
    pub display_name: String,
    pub description: String,
    /// 图集绝对路径。配置页的首帧预览要用它。
    pub spritesheet_path: Option<String>,
    /// 网格规格。预览要靠它算出「第一格」该裁哪块区域。
    pub frame: Option<GridSpec>,
    /// 这个包坏在哪；`None` 表示清单、图集、网格都读得通。
    ///
    /// 坏包仍然列出来：用户明明装了它，凭空消失只会让人怀疑自己装错了地方。
    pub problem: Option<String>,
}

/// 列出 `pets_root` 下所有可选的宠物包。
///
/// 按目录名排序：`read_dir` 的顺序各文件系统不同，配置页每次刷新看到的顺序
/// 都变一遭会很难用。
pub fn list(pets_root: &Path) -> Vec<PetSummary> {
    let Ok(entries) = fs::read_dir(pets_root) else {
        // 家目录还没建起来时不是错误，只是「一个包都没有」。
        return Vec::new();
    };
    let mut found: Vec<PetSummary> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            summarize(&entry.path(), pets_root, &name)
        })
        .collect();
    found.sort_by(|a, b| a.dir.cmp(&b.dir));
    found
}

/// 把一个目录总结成列表项；没有 `pet.json` 的目录返回 `None`。
///
/// 「没有清单」与「清单是坏的」必须分开：前者多半不是宠物包（`pets/` 下混着
/// 说明目录很常见），静默跳过；后者是用户以为装好了的东西，要列出来并告诉他坏在哪。
fn summarize(dir: &Path, pets_root: &Path, name: &str) -> Option<PetSummary> {
    let manifest_path = dir.join(PET_FILE);
    if !manifest_path.is_file() {
        return None;
    }
    let mut summary = PetSummary {
        dir: name.to_string(),
        id: name.to_string(),
        display_name: name.to_string(),
        description: String::new(),
        spritesheet_path: None,
        frame: None,
        problem: None,
    };
    let file = match manifest::PetFile::from_path(&manifest_path) {
        Ok(file) => file,
        Err(err) => {
            summary.problem = Some(format!("{err:#}"));
            return Some(summary);
        }
    };
    summary.id = file.id.clone().unwrap_or_else(|| name.to_string());
    summary.display_name = file
        .display_name
        .clone()
        .unwrap_or_else(|| name.to_string());
    summary.description = file.description.clone().unwrap_or_default();

    // 图集只读文件头：配置页要的是「能不能预览」，不是「能不能渲染」。
    let resolved = resolve_spritesheet(dir, pets_root, file.spritesheet_path.as_deref())
        .and_then(|sheet| image::read_size(&sheet).map(|atlas| (sheet, atlas)))
        .and_then(|(sheet, atlas)| manifest::resolve_grid(&file, atlas).map(|grid| (sheet, grid)));
    match resolved {
        Ok((sheet, grid)) => {
            summary.spritesheet_path = Some(sheet.to_string_lossy().into_owned());
            summary.frame = Some(grid);
        }
        Err(err) => summary.problem = Some(format!("{err:#}")),
    }
    Some(summary)
}

/// 定位并校验图集路径。
///
/// `spritesheetPath` 来自用户可写文件，视为不可信输入（`docs/PET-PACK.md` §7.2）：
/// 先规范化再检查是否落在宠物根目录内，符号链接也一并挡住。
fn resolve_spritesheet(dir: &Path, pets_root: &Path, declared: Option<&str>) -> Result<PathBuf> {
    let relative = declared.context("pet.json 缺少 spritesheetPath")?;
    if relative.trim().is_empty() {
        bail!("pet.json 的 spritesheetPath 为空");
    }
    let joined = dir.join(relative);
    let sheet = joined
        .canonicalize()
        .with_context(|| format!("图集不存在：{}", joined.display()))?;
    let root = pets_root
        .canonicalize()
        .with_context(|| format!("宠物根目录不存在：{}", pets_root.display()))?;
    if !sheet.starts_with(&root) {
        bail!("spritesheetPath 越出宠物目录：{}", sheet.display());
    }
    if !sheet.is_file() {
        bail!("图集不是文件：{}", sheet.display());
    }
    Ok(sheet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 真实社区包的图集尺寸（8 列 × 11 行）。
    const V2_ATLAS: (u32, u32) = (1536, 2288);
    /// 测试用临时根目录名。
    const TEST_ROOT: &str = "litepet-test-pack";

    /// 造一个能通过尺寸解析的 PNG 图集文件。
    fn write_atlas(path: &Path, width: u32, height: u32) {
        let mut raw = Vec::new();
        raw.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        raw.extend_from_slice(&13u32.to_be_bytes());
        raw.extend_from_slice(b"IHDR");
        raw.extend_from_slice(&width.to_be_bytes());
        raw.extend_from_slice(&height.to_be_bytes());
        raw.extend_from_slice(&[8, 6, 0, 0, 0]);
        fs::write(path, raw).expect("写图集失败");
    }

    /// 在临时目录里搭一个宠物包，返回 `(pets_root, id)`。
    fn make_pack(dir_name: &str, manifest: &str) -> (PathBuf, String) {
        let root = std::env::temp_dir().join(format!("{TEST_ROOT}-{dir_name}"));
        let _ = fs::remove_dir_all(&root);
        let pet = root.join("pets").join(dir_name);
        fs::create_dir_all(&pet).expect("建目录失败");
        fs::write(pet.join(PET_FILE), manifest).expect("写清单失败");
        write_atlas(&pet.join("spritesheet.png"), V2_ATLAS.0, V2_ATLAS.1);
        (root.join("pets"), dir_name.to_string())
    }

    /// 只声明 `spriteVersionNumber` 的真实社区包形态应能完整加载。
    #[test]
    fn real_world_manifest_loads() {
        let manifest = r#"{
            "id": "xunjian-miao",
            "displayName": "巡检喵",
            "description": "黑猫巡检员",
            "spritesheetPath": "spritesheet.png",
            "spriteVersionNumber": 2,
            "kind": "animal"
        }"#;
        let (root, id) = make_pack("xunjian-miao", manifest);
        let loaded = load(&root, &id).expect("应加载成功");
        assert_eq!(loaded.info.id, "xunjian-miao");
        assert_eq!(loaded.info.display_name, "巡检喵");
        assert_eq!((loaded.info.frame.columns, loaded.info.frame.rows), (8, 11));
        assert_eq!(loaded.info.animations.len(), 16);
        assert!(loaded.behavior.is_none(), "纯 Codex 包没有 litepet 键");
        let _ = fs::remove_dir_all(root.parent().expect("有父目录"));
    }

    /// `litepet` 键必须原样透出，且不影响 Codex 字段的解析。
    #[test]
    fn litepet_extension_is_passed_through() {
        let manifest = r#"{
            "id": "extended",
            "spritesheetPath": "spritesheet.png",
            "spriteVersionNumber": 2,
            "animations": { "cheer": { "frames": [8, 9, 10], "fps": 12, "loop": false } },
            "litepet": { "schemaVersion": 1, "behavior": { "idleTimeoutMs": 90000 } }
        }"#;
        let (root, id) = make_pack("extended", manifest);
        let loaded = load(&root, &id).expect("应加载成功");
        let behavior = loaded.behavior.expect("应透出 litepet");
        assert_eq!(behavior["schemaVersion"], 1);
        // 覆盖表生效，且缺省动画仍在
        let cheer = &loaded.info.animations["cheer"];
        assert_eq!(cheer.frames.len(), 3);
        assert_eq!(cheer.loop_start, None, "loop: false 应为一次性");
        assert_eq!(loaded.info.animations.len(), 17);
        let _ = fs::remove_dir_all(root.parent().expect("有父目录"));
    }

    /// 未知顶层键（如 Codex 的 `kind`）必须被静默忽略，而不是报错。
    #[test]
    fn unknown_keys_are_ignored() {
        let manifest = r#"{
            "spritesheetPath": "spritesheet.png",
            "spriteVersionNumber": 2,
            "kind": "animal",
            "uploadedAt": "2026-09-20T15:50:18.589Z",
            "someFutureKey": [1, 2, 3]
        }"#;
        let (root, id) = make_pack("unknown-keys", manifest);
        // 缺 id / displayName 时回退成目录名
        let loaded = load(&root, &id).expect("应加载成功");
        assert_eq!(loaded.info.id, "unknown-keys");
        assert_eq!(loaded.info.display_name, "unknown-keys");
        let _ = fs::remove_dir_all(root.parent().expect("有父目录"));
    }

    /// `spritesheetPath` 逃出宠物目录时必须被拒（不可信输入）。
    #[test]
    fn spritesheet_escaping_pack_dir_is_rejected() {
        let root = std::env::temp_dir().join(format!("{TEST_ROOT}-escape"));
        let _ = fs::remove_dir_all(&root);
        let pet = root.join("pets").join("evil");
        fs::create_dir_all(&pet).expect("建目录失败");
        fs::write(root.join("outside.png"), b"x").expect("写外部文件失败");
        fs::write(
            pet.join(PET_FILE),
            r#"{"spritesheetPath": "../../outside.png"}"#,
        )
        .expect("写清单失败");

        let err = load(&root.join("pets"), "evil").expect_err("越界路径应被拒");
        assert!(
            format!("{err}").contains("越出宠物目录"),
            "错误信息应指出越界，实际为 {err}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 缺少 `spritesheetPath` 时必须报错，不能默默加载。
    #[test]
    fn missing_spritesheet_path_is_rejected() {
        let (root, id) = make_pack("no-path", r#"{"id":"no-path"}"#);
        let err = load(&root, &id).expect_err("应被拒");
        assert!(format!("{err}").contains("spritesheetPath"), "实际为 {err}");
        let _ = fs::remove_dir_all(root.parent().expect("有父目录"));
    }

    /// 真实本机 `~/.litepet/pets` 下的每个包都应能加载。
    ///
    /// 这是唯一会碰到真实 WebP 头的用例（`xunjian-miao` 是无损 `VP8L`）。
    /// 目录不存在时跳过，保证在干净环境下也能跑。
    #[test]
    fn local_real_packs_load_if_present() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let root = PathBuf::from(home).join(".litepet").join("pets");
        let Ok(entries) = fs::read_dir(&root) else {
            return;
        };
        let mut checked = 0;
        for entry in entries.flatten() {
            let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !entry.path().join(PET_FILE).is_file() {
                continue;
            }
            let loaded = load(&root, &id)
                .unwrap_or_else(|err| panic!("真实包 {id} 应能加载，实际报错：{err:#}"));
            assert_eq!(
                loaded.info.frame.frame_count(),
                loaded.info.frame.columns as usize * loaded.info.frame.rows as usize
            );
            checked += 1;
        }
        println!("已校验 {checked} 个真实本机宠物包");
    }

    /// 造一个能装多个包的临时 `pets/` 根目录。
    ///
    /// `make_pack` 一次只造一个包，而列表接口要的恰好是「一个根目录下好几个包」。
    fn make_pets_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("{TEST_ROOT}-list-{tag}"));
        let _ = fs::remove_dir_all(&root);
        let pets = root.join("pets");
        fs::create_dir_all(&pets).expect("建目录失败");
        pets
    }

    /// 往根目录下写一个能完整加载的包。
    fn write_ok_pack(pets: &Path, dir_name: &str) {
        let pet = pets.join(dir_name);
        fs::create_dir_all(&pet).expect("建目录失败");
        let manifest = format!(
            r#"{{"id":"{dir_name}","displayName":"名称-{dir_name}","description":"描述","spritesheetPath":"spritesheet.png"}}"#
        );
        fs::write(pet.join(PET_FILE), manifest).expect("写清单失败");
        write_atlas(&pet.join("spritesheet.png"), V2_ATLAS.0, V2_ATLAS.1);
    }

    /// 列表要按目录名排序，跳过不是宠物包的目录，并容忍根目录不存在。
    #[test]
    fn list_sorts_and_skips_non_packs() {
        let pets = make_pets_root("sort");
        write_ok_pack(&pets, "charlie");
        write_ok_pack(&pets, "alpha");
        write_ok_pack(&pets, "bravo");
        // 没有 pet.json 的目录与顶层散文件都不是宠物包
        fs::create_dir_all(pets.join("readme-dir")).expect("建目录失败");
        fs::write(pets.join("loose.txt"), b"x").expect("写文件失败");

        let found = list(&pets);
        let dirs: Vec<&str> = found.iter().map(|pet| pet.dir.as_str()).collect();
        assert_eq!(dirs, ["alpha", "bravo", "charlie"], "要按目录名排序");
        assert!(found.iter().all(|pet| pet.problem.is_none()));

        // 家目录还不存在时是空列表，不是 panic
        assert!(list(&pets.join("不存在的目录")).is_empty());
        let _ = fs::remove_dir_all(pets.parent().expect("有父目录"));
    }

    /// 列表项要带上图集与网格：配置页靠它们裁出第一格做预览。
    #[test]
    fn list_fills_path_and_frame_for_preview() {
        let pets = make_pets_root("preview");
        write_ok_pack(&pets, "miao");

        let found = list(&pets);
        assert_eq!(found.len(), 1);
        let pet = &found[0];
        assert_eq!(pet.id, "miao");
        assert_eq!(pet.display_name, "名称-miao");
        assert_eq!(pet.description, "描述");
        let sheet = pet.spritesheet_path.as_deref().expect("应有图集路径");
        assert!(sheet.ends_with("spritesheet.png"), "实际为 {sheet}");
        let frame = pet.frame.expect("应有网格");
        assert_eq!((frame.columns, frame.rows), (8, 11));
        let _ = fs::remove_dir_all(pets.parent().expect("有父目录"));
    }

    /// 坏包也要列出来（带原因）：用户明明装了它，凭空消失更难查。
    #[test]
    fn list_reports_broken_pack_with_reason() {
        let pets = make_pets_root("broken");
        // 清单是坏 JSON
        let bad = pets.join("bad-json");
        fs::create_dir_all(&bad).expect("建目录失败");
        fs::write(bad.join(PET_FILE), b"{not json").expect("写清单失败");
        // 清单能读，但图集缺失
        let missing = pets.join("missing-sheet");
        fs::create_dir_all(&missing).expect("建目录失败");
        fs::write(missing.join(PET_FILE), r#"{"spritesheetPath":"nope.png"}"#).expect("写清单失败");

        let found = list(&pets);
        assert_eq!(found.len(), 2, "两个坏包都要在列表里");
        for pet in &found {
            let problem = pet.problem.as_deref().expect("应给出坏在哪");
            assert!(!problem.is_empty(), "坏因不能是空字符串");
            assert_eq!(pet.id, pet.dir, "清单读不通时 id 回落成目录名");
            assert!(pet.frame.is_none());
        }
        let bad_json = found
            .iter()
            .find(|pet| pet.dir == "bad-json")
            .expect("应有 bad-json");
        assert!(bad_json.spritesheet_path.is_none());
        let _ = fs::remove_dir_all(pets.parent().expect("有父目录"));
    }
}
