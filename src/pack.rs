//! 宠物包加载：解析 `pet.json` 并校验图集网格。
//!
//! 契约见 `docs/PET-PACK.md`。读包顺序（§3.10）：
//!
//! 1. 有 `frame` → 以其为准
//! 2. 否则有 `spriteVersionNumber` → 行数取 `{1: 9, 2: 11}`，单格固定 `192×208`
//! 3. 都没有 → 按缺省 `1`（9 行）处理

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 清单文件名。
const PET_FILE: &str = "pet.json";
/// 默认单格宽（`FrameSpec` 缺省值）。
const DEFAULT_FRAME_WIDTH: u32 = 192;
/// 默认单格高。
const DEFAULT_FRAME_HEIGHT: u32 = 208;
/// 默认网格列数。
const DEFAULT_FRAME_COLUMNS: u32 = 8;
/// 默认网格行数。
const DEFAULT_FRAME_ROWS: u32 = 9;
/// V2 的网格行数。
const V2_FRAME_ROWS: u32 = 11;

/// V1 九行行名，下标即行号（取自 Codex `default_animations()`，已逐行核对）。
const V1_ROW_NAMES: [&str; 9] = [
    "idle",
    "running-right",
    "running-left",
    "waving",
    "jumping",
    "failed",
    "waiting",
    "running",
    "review",
];

/// V2 在 V1 之上追加的两行（行 9、10）。
///
/// ⚠️ 行名来自社区文档，未从源码确证，见 `docs/PET-PACK.md` §3.10。
const V2_EXTRA_ROW_NAMES: [&str; 2] = ["look-directions-a", "look-directions-b"];

/// 一行的默认播放参数：帧数、单帧时长（毫秒）、末帧时长（毫秒）。
struct RowTiming {
    /// 该行有效帧数。
    frames: u32,
    /// 常规帧时长（毫秒）。
    frame_ms: u64,
    /// 末帧时长（毫秒）。
    last_ms: u64,
}

/// V1 九行的默认播放参数。
///
/// 时长取自 Codex `default_animations()` 的 `Duration` 值；`idle` 另有非均匀时长，
/// 由 [`idle_animation`] 单独构造。
const V1_ROW_TIMINGS: [RowTiming; 9] = [
    RowTiming { frames: 6, frame_ms: 660, last_ms: 1920 },
    RowTiming { frames: 8, frame_ms: 120, last_ms: 220 },
    RowTiming { frames: 8, frame_ms: 120, last_ms: 220 },
    RowTiming { frames: 4, frame_ms: 140, last_ms: 280 },
    RowTiming { frames: 5, frame_ms: 140, last_ms: 280 },
    RowTiming { frames: 8, frame_ms: 140, last_ms: 240 },
    RowTiming { frames: 6, frame_ms: 150, last_ms: 260 },
    RowTiming { frames: 6, frame_ms: 120, last_ms: 220 },
    RowTiming { frames: 6, frame_ms: 150, last_ms: 280 },
];

/// `idle` 的非均匀帧时长（毫秒）——刻意不均等，使其看起来像「偶尔动一下」。
const IDLE_FRAME_MS: [u64; 6] = [1680, 660, 660, 840, 840, 1920];

/// 网格规格。
#[derive(Debug, Clone, Copy, Serialize)]
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

/// 渲染层需要的单帧信息。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FramePlan {
    /// 精灵索引（`row * columns + column`）。
    pub sprite_index: usize,
    /// 该帧显示时长（毫秒）。
    pub duration_ms: u64,
}

/// 单个动画的播放计划。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnimationPlan {
    /// 帧序列。
    pub frames: Vec<FramePlan>,
    /// 循环起点；`None` 表示一次性，播完交棒 `fallback`。
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

/// `pet.json` 的原始结构。
///
/// 字段名严格对应 Codex 格式；未列出的键（如我们的 `petdaemon`）被 serde 静默忽略。
#[derive(Debug, Deserialize)]
struct PetFile {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "spritesheetPath")]
    spritesheet_path: Option<String>,
    frame: Option<FrameSpec>,
    #[serde(default, rename = "spriteVersionNumber")]
    sprite_version_number: Option<u32>,
}

/// `pet.json` 的 `frame` 字段。
#[derive(Debug, Clone, Copy, Deserialize)]
struct FrameSpec {
    width: u32,
    height: u32,
    columns: u32,
    rows: u32,
}

/// 从 `pets/<id>/` 加载一个包。
///
/// `pets_root` 为宠物包根目录，用于阻挡 `spritesheetPath` 逃出包目录。
pub fn load(pets_root: &Path, id: &str) -> Result<PetInfo> {
    let dir = pets_root.join(id);
    let manifest = dir.join(PET_FILE);
    if !manifest.is_file() {
        bail!("缺失清单文件：{}", manifest.display());
    }
    let raw = fs::read_to_string(&manifest)
        .with_context(|| format!("读取清单失败：{}", manifest.display()))?;
    let file: PetFile = serde_json::from_str(&raw)
        .with_context(|| format!("解析清单失败：{}", manifest.display()))?;

    let grid = resolve_grid(file.frame, file.sprite_version_number)?;
    let sheet = resolve_spritesheet(&dir, pets_root, file.spritesheet_path.as_deref())?;
    let animations = build_animations(grid, file.sprite_version_number);

    Ok(PetInfo {
        id: file.id.unwrap_or_else(|| id.to_string()),
        display_name: file.display_name.unwrap_or_else(|| id.to_string()),
        description: file.description.unwrap_or_default(),
        spritesheet_path: sheet.to_string_lossy().into_owned(),
        frame: grid,
        animations,
    })
}

/// 定位并校验图集路径，阻挡越出宠物目录的路径。
fn resolve_spritesheet(dir: &Path, pets_root: &Path, declared: Option<&str>) -> Result<PathBuf> {
    let rel = declared.context("pet.json 缺少 spritesheetPath")?;
    let joined = dir.join(rel);
    let sheet = joined
        .canonicalize()
        .with_context(|| format!("图集不存在：{}", joined.display()))?;
    // spritesheetPath 来自用户可写文件，视为不可信输入（docs/PET-PACK.md §7.2）
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

/// 定网格：`frame` 优先，其次由 `spriteVersionNumber` 推导，最后取缺省。
fn resolve_grid(frame: Option<FrameSpec>, version: Option<u32>) -> Result<GridSpec> {
    let grid = match frame {
        Some(f) => GridSpec {
            width: f.width,
            height: f.height,
            columns: f.columns,
            rows: f.rows,
        },
        None => {
            let rows = match version {
                None | Some(1) => DEFAULT_FRAME_ROWS,
                Some(2) => V2_FRAME_ROWS,
                Some(other) => bail!("未知的 spriteVersionNumber：{other}（仅支持 1 与 2）"),
            };
            GridSpec {
                width: DEFAULT_FRAME_WIDTH,
                height: DEFAULT_FRAME_HEIGHT,
                columns: DEFAULT_FRAME_COLUMNS,
                rows,
            }
        }
    };
    if grid.width == 0 || grid.height == 0 || grid.columns == 0 || grid.rows == 0 {
        // 错误串沿用 Codex 原文，便于对照
        bail!("pet frame dimensions and grid counts must be non-zero");
    }
    Ok(grid)
}

/// 按网格构造默认动画表（行名 → 帧序列）。
fn build_animations(grid: GridSpec, version: Option<u32>) -> HashMap<String, AnimationPlan> {
    let mut map = HashMap::new();
    let count = if version == Some(2) { V2_EXTRA_ROW_NAMES.len() } else { 0 };

    for (row, name) in V1_ROW_NAMES.iter().enumerate() {
        let plan = if row == 0 {
            idle_animation(grid)
        } else {
            action_animation(grid, row, &V1_ROW_TIMINGS[row])
        };
        map.insert((*name).to_string(), plan);
    }
    for i in 0..count {
        let row = V1_ROW_NAMES.len() + i;
        // V2 追加行的时长沿用 idle，源码未确证
        let plan = loop_animation(grid, row, &V1_ROW_TIMINGS[0]);
        map.insert(V2_EXTRA_ROW_NAMES[i].to_string(), plan);
    }
    map
}

/// 精灵索引：`row * columns + column`。
fn sprite_index(grid: GridSpec, row: usize, col: usize) -> usize {
    row * grid.columns as usize + col
}

/// `idle`：非均匀时长、从第 0 帧起循环。
fn idle_animation(grid: GridSpec) -> AnimationPlan {
    let frames = IDLE_FRAME_MS
        .iter()
        .enumerate()
        .map(|(col, ms)| FramePlan {
            sprite_index: sprite_index(grid, 0, col),
            duration_ms: *ms,
        })
        .collect();
    AnimationPlan {
        frames,
        loop_start: Some(0),
        fallback: "idle".to_string(),
    }
}

/// 纯循环动画：均匀时长，从第 0 帧起循环。
fn loop_animation(grid: GridSpec, row: usize, timing: &RowTiming) -> AnimationPlan {
    let frames = (0..timing.frames as usize)
        .map(|col| FramePlan {
            sprite_index: sprite_index(grid, row, col),
            duration_ms: timing.frame_ms,
        })
        .collect();
    AnimationPlan {
        frames,
        loop_start: Some(0),
        fallback: "idle".to_string(),
    }
}

/// 动作动画：主序列重复 3 遍后接 `idle` 帧，循环点落在 `idle` 段开头。
///
/// 复现 Codex `app_state_animation()`：动作播 3 遍 → 滑入待机 → 在待机里循环，
/// 这样收尾自然且循环时不重播动作。
fn action_animation(grid: GridSpec, row: usize, timing: &RowTiming) -> AnimationPlan {
    const REPEATS: usize = 3;
    let mut frames = Vec::new();
    for _ in 0..REPEATS {
        for col in 0..timing.frames as usize {
            let is_last = col + 1 == timing.frames as usize;
            frames.push(FramePlan {
                sprite_index: sprite_index(grid, row, col),
                duration_ms: if is_last { timing.last_ms } else { timing.frame_ms },
            });
        }
    }
    let loop_start = frames.len();
    let idle = idle_animation(grid);
    frames.extend(idle.frames);
    AnimationPlan {
        frames,
        loop_start: Some(loop_start),
        fallback: idle.fallback,
    }
}
