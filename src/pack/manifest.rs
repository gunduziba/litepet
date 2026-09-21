//! `pet.json` 反序列化与网格解析。
//!
//! 字段名严格对齐 Codex `codex-rs/tui/src/pets/model.rs`；`PetFile` 不设
//! `deny_unknown_fields`，这正是我们能在同一个 `pet.json` 里塞 `litepet` 键的原因
//! （`docs/PET-PACK.md` §4.1）——Codex 与我们都读它，各取所需、互不报错。

use super::image::AtlasSize;
use super::GridSpec;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// 缺省单格宽（Codex `catalog::DEFAULT_FRAME_WIDTH`）。
const DEFAULT_FRAME_WIDTH: u32 = 192;
/// 缺省单格高（Codex `catalog::DEFAULT_FRAME_HEIGHT`）。
const DEFAULT_FRAME_HEIGHT: u32 = 208;
/// 缺省网格列数（Codex `catalog::DEFAULT_FRAME_COLUMNS`）。
const DEFAULT_FRAME_COLUMNS: u32 = 8;
/// Codex 内置网格的行数（`spriteVersionNumber` 缺省/1）。
const DEFAULT_FRAME_ROWS: u32 = 9;
/// Codex 单包最多帧数（`model.rs` `MAX_PET_FRAMES`）。
const MAX_PET_FRAMES: usize = 256;

/// `pet.json` 的原始结构。
#[derive(Debug, Deserialize)]
pub(super) struct PetFile {
    /// 包 id；缺省时用目录名。
    #[serde(default)]
    pub(super) id: Option<String>,
    /// 展示名；缺省时用目录名。
    #[serde(default, rename = "displayName")]
    pub(super) display_name: Option<String>,
    /// 描述。
    #[serde(default)]
    pub(super) description: Option<String>,
    /// 图集相对路径。
    #[serde(default, rename = "spritesheetPath")]
    pub(super) spritesheet_path: Option<String>,
    /// 显式网格；给了就以此为准。
    #[serde(default)]
    pub(super) frame: Option<FrameSpec>,
    /// 社区生态的图集版本号：1 → 9 行，2 → 11 行。Codex 不读该字段。
    #[serde(default, rename = "spriteVersionNumber")]
    pub(super) sprite_version_number: Option<u32>,
    /// 动画覆盖表：键为动画名。Codex 语义是「覆盖缺省表」，不是替换。
    #[serde(default)]
    pub(super) animations: HashMap<String, AnimationSpec>,
    /// 我们的扩展键，由行为模块解析（`docs/PET-PACK.md` §4.2）。
    #[serde(default)]
    pub(super) litepet: Option<serde_json::Value>,
}

/// `pet.json` 的 `frame` 字段；四个字段都必填。
#[derive(Debug, Clone, Copy, Deserialize)]
pub(super) struct FrameSpec {
    width: u32,
    height: u32,
    columns: u32,
    rows: u32,
}

/// `pet.json` 的单个动画规格。
#[derive(Debug, Deserialize)]
pub(super) struct AnimationSpec {
    /// 精灵索引序列，索引语义为 `row * columns + column`。
    #[serde(default)]
    pub(super) frames: Vec<usize>,
    /// 播放帧率；缺省 8。
    #[serde(default)]
    pub(super) fps: Option<f64>,
    /// 是否循环；缺省 true。
    #[serde(default, rename = "loop")]
    pub(super) loop_animation: Option<bool>,
    /// 一次性动画播完后的接续动画；空串视作 `idle`。
    #[serde(default)]
    pub(super) fallback: String,
}

impl PetFile {
    /// 从磁盘读并解析清单。
    pub(super) fn from_path(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("读取清单失败：{}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("解析清单失败：{}", path.display()))
    }
}

/// 定网格：`frame` 优先；缺省时按缺省单格尺寸从真实图集反推。
///
/// 反推是相对 Codex 的**唯一实质放宽**：Codex 对无 `frame` 的包直接用 192×208×8×9，
/// 再要求「网格精确覆盖图集」，于是 11 行的社区图集（1536×2288）一律加载失败。
pub(super) fn resolve_grid(file: &PetFile, atlas: AtlasSize) -> Result<GridSpec> {
    let grid = match file.frame {
        Some(spec) => explicit_grid(spec, atlas)?,
        None => inferred_grid(atlas)?,
    };
    ensure_frame_budget(grid)?;
    Ok(grid)
}

/// 显式网格：四个维度非零、无溢出，且必须精确覆盖图集（Codex 原文校验）。
fn explicit_grid(spec: FrameSpec, atlas: AtlasSize) -> Result<GridSpec> {
    if spec.width == 0 || spec.height == 0 || spec.columns == 0 || spec.rows == 0 {
        // 错误串沿用 Codex 原文，便于对照排查
        bail!("pet frame dimensions and grid counts must be non-zero");
    }
    let total_width = spec
        .width
        .checked_mul(spec.columns)
        .context("pet frame grid width overflow")?;
    let total_height = spec
        .height
        .checked_mul(spec.rows)
        .context("pet frame grid height overflow")?;
    if total_width != atlas.width || total_height != atlas.height {
        bail!(
            "pet frame grid must cover spritesheet exactly: expected {}x{}, got {total_width}x{total_height}",
            atlas.width,
            atlas.height
        );
    }
    Ok(GridSpec {
        width: spec.width,
        height: spec.height,
        columns: spec.columns,
        rows: spec.rows,
    })
}

/// 隐式网格：按缺省单格尺寸整除真实图集。
fn inferred_grid(atlas: AtlasSize) -> Result<GridSpec> {
    if !atlas.is_valid() {
        bail!("图集尺寸无效：{}x{}", atlas.width, atlas.height);
    }
    if atlas.width % DEFAULT_FRAME_WIDTH != 0 || atlas.height % DEFAULT_FRAME_HEIGHT != 0 {
        bail!(
            "图集 {}x{} 无法按缺省单格 {}x{} 整除，且 pet.json 未声明 frame",
            atlas.width,
            atlas.height,
            DEFAULT_FRAME_WIDTH,
            DEFAULT_FRAME_HEIGHT
        );
    }
    Ok(GridSpec {
        width: DEFAULT_FRAME_WIDTH,
        height: DEFAULT_FRAME_HEIGHT,
        columns: atlas.width / DEFAULT_FRAME_WIDTH,
        rows: atlas.height / DEFAULT_FRAME_HEIGHT,
    })
}

/// 总帧数上限（Codex `MAX_PET_FRAMES`）。
fn ensure_frame_budget(grid: GridSpec) -> Result<()> {
    let frame_count = grid.frame_count();
    if frame_count > MAX_PET_FRAMES {
        bail!("pet frame count {frame_count} exceeds maximum {MAX_PET_FRAMES}");
    }
    Ok(())
}

/// 网格与包声明的差异告警（不硬失败）。
///
/// 社区版本号与行名表都是外部约定，图集像素才是事实依据，所以一律只告警：
/// 声明未知版本、行数与图集不符、列数非标准的 8 列（行名表的帧数是按 8 列写的）。
pub(super) fn grid_warnings(file: &PetFile, grid: GridSpec) -> Vec<String> {
    let mut warnings = Vec::new();
    let expected_rows = match file.sprite_version_number {
        None | Some(1) => Some(DEFAULT_FRAME_ROWS),
        Some(2) => Some(DEFAULT_FRAME_ROWS + 2),
        Some(other) => {
            warnings.push(format!(
                "pet.json 声明了未知的 spriteVersionNumber：{other}，已按图集像素 {}x{} 反推网格",
                grid.columns, grid.rows
            ));
            None
        }
    };
    if let Some(expected) = expected_rows {
        if grid.rows != expected {
            warnings.push(format!(
                "pet.json 声明 spriteVersionNumber={:?}（应为 {} 行），但图集反推为 {} 行；以图集为准",
                file.sprite_version_number, expected, grid.rows
            ));
        }
    }
    if grid.columns != DEFAULT_FRAME_COLUMNS {
        warnings.push(format!(
            "图集为 {} 列（Codex 行名表按 {} 列定义），行号为推理得来的动画已按实际列数收敛；此包建议在 pet.json 里显式声明 animations",
            grid.columns, DEFAULT_FRAME_COLUMNS
        ));
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实社区包图集：8 列 × 11 行。
    const V2_ATLAS: AtlasSize = AtlasSize {
        width: 1536,
        height: 2288,
    };
    /// Codex 内置包图集：8 列 × 9 行。
    const V1_ATLAS: AtlasSize = AtlasSize {
        width: 1536,
        height: 1872,
    };

    /// 解析一个只有 `spriteVersionNumber` 的清单。
    fn file_with_version(version: Option<u32>) -> PetFile {
        PetFile {
            id: None,
            display_name: None,
            description: None,
            spritesheet_path: None,
            frame: None,
            sprite_version_number: version,
            animations: HashMap::new(),
            litepet: None,
        }
    }

    /// 无 `frame` 时，11 行图集应反推为 8×11，而不是 Codex 那样直接报错。
    #[test]
    fn v2_atlas_is_inferred_from_pixels() {
        let grid = resolve_grid(&file_with_version(Some(2)), V2_ATLAS).expect("应反推成功");
        assert_eq!((grid.columns, grid.rows), (8, 11));
        assert_eq!((grid.width, grid.height), (192, 208));
        assert_eq!(grid.frame_count(), 88);
    }

    /// 无 `frame` 时，9 行图集应反推为 8×9。
    #[test]
    fn v1_atlas_is_inferred_from_pixels() {
        for version in [None, Some(1)] {
            let grid = resolve_grid(&file_with_version(version), V1_ATLAS).expect("应反推成功");
            assert_eq!((grid.columns, grid.rows), (8, 9));
            assert_eq!(grid.frame_count(), 72);
        }
    }

    /// 尺寸不能被缺省单格整除、又没有 `frame` 时，必须报错而不是猜。
    #[test]
    fn indivisible_atlas_without_frame_is_rejected() {
        let atlas = AtlasSize {
            width: 1000,
            height: 1000,
        };
        let err = resolve_grid(&file_with_version(None), atlas).expect_err("应被拒");
        assert!(format!("{err}").contains("整除"), "实际为 {err}");
    }

    /// 显式 `frame` 必须精确覆盖图集（Codex 原文校验）。
    #[test]
    fn explicit_frame_must_cover_atlas_exactly() {
        let mut file = file_with_version(Some(2));
        file.frame = Some(FrameSpec {
            width: 192,
            height: 208,
            columns: 8,
            rows: 9,
        });
        let err = resolve_grid(&file, V2_ATLAS).expect_err("9 行网格盖不住 11 行图集");
        assert!(
            format!("{err}").contains("must cover spritesheet exactly"),
            "实际为 {err}"
        );

        file.frame = Some(FrameSpec {
            width: 192,
            height: 208,
            columns: 8,
            rows: 11,
        });
        assert!(resolve_grid(&file, V2_ATLAS).is_ok(), "精确覆盖应通过");
    }

    /// 网格维度为 0 必须被拒。
    #[test]
    fn zero_dimension_is_rejected() {
        let mut file = file_with_version(None);
        file.frame = Some(FrameSpec {
            width: 192,
            height: 0,
            columns: 8,
            rows: 9,
        });
        let err = resolve_grid(&file, V1_ATLAS).expect_err("应被拒");
        assert!(
            format!("{err}").contains("must be non-zero"),
            "实际为 {err}"
        );
    }

    /// 单格过大导致总帧数超过 256 时必须被拒。
    #[test]
    fn frame_budget_is_enforced() {
        let atlas = AtlasSize {
            width: 192 * 20,
            height: 208 * 20,
        };
        let err = resolve_grid(&file_with_version(None), atlas).expect_err("400 帧应超限");
        assert!(format!("{err}").contains("exceeds maximum"), "实际为 {err}");
    }

    /// 未知 `spriteVersionNumber` 不硬失败，只给告警。
    #[test]
    fn unknown_version_only_warns() {
        let file = file_with_version(Some(9));
        let grid = resolve_grid(&file, V2_ATLAS).expect("应仍能加载");
        let warnings = grid_warnings(&file, grid);
        assert!(
            warnings.iter().any(|w| w.contains("未知")),
            "实际为 {warnings:?}"
        );
    }

    /// 标准的 8 列包不应产生列数告警。
    #[test]
    fn standard_pack_has_no_column_warning() {
        let file = file_with_version(Some(2));
        let grid = resolve_grid(&file, V2_ATLAS).expect("应反推成功");
        assert!(grid_warnings(&file, grid).is_empty());
    }

    /// 非 8 列图集应提示显式声明动画。
    #[test]
    fn non_standard_columns_warn() {
        let mut file = file_with_version(None);
        file.frame = Some(FrameSpec {
            width: 384,
            height: 208,
            columns: 4,
            rows: 11,
        });
        let grid = resolve_grid(&file, V2_ATLAS).expect("应解析成功");
        let warnings = grid_warnings(&file, grid);
        assert!(
            warnings.iter().any(|w| w.contains("4 列")),
            "实际为 {warnings:?}"
        );
    }
}
