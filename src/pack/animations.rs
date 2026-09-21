//! 动画表构造：忠实复现 Codex 的 `default_animations()` / `idle_animation()` /
//! `app_state_animation()` / `load_animations()` 语义。
//!
//! 行号与时长取自 `codex-rs/tui/src/pets/model.rs` 逐行核对的结果，见
//! `docs/PET-PACK.md` §3.7。两处**刻意与 Codex 不同**的地方在函数注释里标明。

use super::manifest::AnimationSpec;
use super::{AnimationPlan, FramePlan, GridSpec};
use anyhow::{bail, Result};
use std::collections::HashMap;

/// 单包动画帧率上限（Codex `MAX_ANIMATION_FPS`）。
const MAX_ANIMATION_FPS: f64 = 60.0;
/// 未声明 `fps` 时的缺省帧率（Codex `load_animations` 中的字面量）。
const DEFAULT_FPS: f64 = 8.0;
/// 动作主序列的重复遍数（Codex `app_state_animation` 中的 3）。
const ACTION_REPEATS: usize = 3;
/// Codex 具名网格的行数；第 9 行起是社区 V2 追加的行。
const CODEX_ROWS: u32 = 9;

/// 一行动画的缺省播放参数。
struct RowAnimation {
    /// 动画名（Codex `default_animations()` 的键）。
    name: &'static str,
    /// 所在行号。
    row: u32,
    /// 该行有效帧数。
    frames: usize,
    /// 常规帧时长（毫秒）。
    frame_ms: u64,
    /// 每遍末帧时长（毫秒）。
    last_ms: u64,
}

/// Codex 的 13 个非 `idle` 动画。
///
/// 后 5 个（`move_right` / `move_left` / `wave` / `bounce` / `sad`）是前 8 个的别名，
/// 共用同一行的播放参数——这是协议 §4.4 降级映射能落到实处的依据。
const ROW_ANIMATIONS: [RowAnimation; 13] = [
    RowAnimation {
        name: "running-right",
        row: 1,
        frames: 8,
        frame_ms: 120,
        last_ms: 220,
    },
    RowAnimation {
        name: "running-left",
        row: 2,
        frames: 8,
        frame_ms: 120,
        last_ms: 220,
    },
    RowAnimation {
        name: "waving",
        row: 3,
        frames: 4,
        frame_ms: 140,
        last_ms: 280,
    },
    RowAnimation {
        name: "jumping",
        row: 4,
        frames: 5,
        frame_ms: 140,
        last_ms: 280,
    },
    RowAnimation {
        name: "failed",
        row: 5,
        frames: 8,
        frame_ms: 140,
        last_ms: 240,
    },
    RowAnimation {
        name: "waiting",
        row: 6,
        frames: 6,
        frame_ms: 150,
        last_ms: 260,
    },
    RowAnimation {
        name: "running",
        row: 7,
        frames: 6,
        frame_ms: 120,
        last_ms: 220,
    },
    RowAnimation {
        name: "review",
        row: 8,
        frames: 6,
        frame_ms: 150,
        last_ms: 280,
    },
    RowAnimation {
        name: "move_right",
        row: 1,
        frames: 8,
        frame_ms: 120,
        last_ms: 220,
    },
    RowAnimation {
        name: "move_left",
        row: 2,
        frames: 8,
        frame_ms: 120,
        last_ms: 220,
    },
    RowAnimation {
        name: "wave",
        row: 3,
        frames: 4,
        frame_ms: 140,
        last_ms: 280,
    },
    RowAnimation {
        name: "bounce",
        row: 4,
        frames: 5,
        frame_ms: 140,
        last_ms: 280,
    },
    RowAnimation {
        name: "sad",
        row: 5,
        frames: 8,
        frame_ms: 140,
        last_ms: 240,
    },
];

/// `idle` 的非均匀帧时长（毫秒）。
///
/// 刻意不均等：前 5 帧是零星小动作，末帧长按让猫「盯着你看一会儿」。
const IDLE_FRAME_MS: [u64; 6] = [1680, 660, 660, 840, 840, 1920];

/// 社区 V2 图集在第 9、10 行追加的注视方向动画名。
///
/// ⚠️ 命名来自社区生态约定，**未从 Codex 源码确证**：Codex 只定义 9 行，
/// 视频站的校验报告把 11 行描述为「9 个动画 + 16 个注视方向」（2 行 × 8 列）。
const LOOK_ROW_NAMES: [&str; 2] = ["look-directions-a", "look-directions-b"];
/// 注视方向动画的有效帧数（整行为 8 列）。
const LOOK_FRAMES: usize = 8;
/// 注视方向动画的帧时长（毫秒）——源码无依据，取与 `waiting` 同量级的慢速轮换。
const LOOK_FRAME_MS: u64 = 150;

/// 构造动画表：缺省表铺底，`pet.json` 的 `animations` 逐项覆盖。
pub(super) fn build(
    grid: GridSpec,
    specs: &HashMap<String, AnimationSpec>,
) -> Result<HashMap<String, AnimationPlan>> {
    let mut animations = default_table(grid);
    for (name, spec) in specs {
        animations.insert(name.clone(), from_spec(name, spec, grid.frame_count())?);
    }
    // 覆盖表可以把 idle 删掉吗？不能——它是所有 fallback 的终点
    animations
        .entry("idle".to_string())
        .or_insert_with(|| idle(grid));
    validate(&animations, grid.frame_count())?;
    Ok(animations)
}

/// Codex 缺省表：`idle` + 13 个具名动画 + 图集实际存在的注视行。
fn default_table(grid: GridSpec) -> HashMap<String, AnimationPlan> {
    let mut map = HashMap::new();
    map.insert("idle".to_string(), idle(grid));
    for spec in &ROW_ANIMATIONS {
        if let Some(plan) = action_animation(grid, spec) {
            map.insert(spec.name.to_string(), plan);
        }
    }
    for (index, name) in LOOK_ROW_NAMES.iter().enumerate() {
        let row = CODEX_ROWS + index as u32;
        if row >= grid.rows {
            continue;
        }
        let frames = LOOK_FRAMES.min(grid.columns as usize);
        if frames == 0 {
            continue;
        }
        let plan = loop_animation(grid, row, frames, LOOK_FRAME_MS);
        map.insert((*name).to_string(), plan);
    }
    map
}

/// `idle`：非均匀时长，从第 0 帧起循环。
fn idle(grid: GridSpec) -> AnimationPlan {
    let frames = IDLE_FRAME_MS
        .iter()
        .enumerate()
        .filter_map(|(column, ms)| {
            sprite_index(grid, 0, column).map(|index| FramePlan {
                sprite_index: index,
                duration_ms: *ms,
            })
        })
        .collect::<Vec<_>>();
    AnimationPlan {
        frames,
        loop_start: Some(0),
        fallback: "idle".to_string(),
    }
}

/// 动作动画：主序列重复 3 遍，再滑入 `idle` 帧，循环点落在 `idle` 段开头。
///
/// 复现 Codex `app_state_animation()`：动作播 3 遍 → 收尾接住待机 → 在待机里循环，
/// 这样不会因为反复重播动作而显得抽风。**每一遍的末帧**都加长，不是只有最后一遍。
///
/// 返回 `None` 表示该行在网格里放不下（列数不足），此时不产出这个动画名。
fn action_animation(grid: GridSpec, spec: &RowAnimation) -> Option<AnimationPlan> {
    let columns = grid.columns as usize;
    let frames_per_round = spec.frames.min(columns);
    if frames_per_round == 0 || spec.row >= grid.rows {
        return None;
    }
    let mut frames = Vec::with_capacity(frames_per_round * ACTION_REPEATS + IDLE_FRAME_MS.len());
    for _ in 0..ACTION_REPEATS {
        for column in 0..frames_per_round {
            let is_last = column + 1 == frames_per_round;
            frames.push(FramePlan {
                sprite_index: sprite_index(grid, spec.row, column)?,
                duration_ms: if is_last { spec.last_ms } else { spec.frame_ms },
            });
        }
    }
    let loop_start = frames.len();
    let tail = idle(grid);
    frames.extend(tail.frames);
    Some(AnimationPlan {
        frames,
        loop_start: Some(loop_start),
        fallback: tail.fallback,
    })
}

/// 纯循环动画：均匀时长，从第 0 帧起循环。
fn loop_animation(grid: GridSpec, row: u32, frames: usize, frame_ms: u64) -> AnimationPlan {
    let plan = (0..frames)
        .filter_map(|column| {
            sprite_index(grid, row, column).map(|index| FramePlan {
                sprite_index: index,
                duration_ms: frame_ms,
            })
        })
        .collect();
    AnimationPlan {
        frames: plan,
        loop_start: Some(0),
        fallback: "idle".to_string(),
    }
}

/// 精灵索引：`row * columns + column`。
///
/// Codex 写死 `row * 8 + column`（`app_state_animation` 里的字面量 `DEFAULT_FRAME_COLUMNS`）。
/// 那只对 8 列的内置图集成立，对自定义网格会算错行；我们改用真实列数，
/// 在列数为 8 时与 Codex 完全等价。
fn sprite_index(grid: GridSpec, row: u32, column: usize) -> Option<usize> {
    if row >= grid.rows || column >= grid.columns as usize {
        return None;
    }
    Some(row as usize * grid.columns as usize + column)
}

/// 用 `pet.json` 的规格构造一个动画。
fn from_spec(name: &str, spec: &AnimationSpec, frame_count: usize) -> Result<AnimationPlan> {
    if spec.frames.is_empty() {
        bail!("animation {name} must include at least one frame");
    }
    let duration_ms = duration_ms(name, spec.fps)?;
    let mut frames = Vec::with_capacity(spec.frames.len());
    for &sprite_index in &spec.frames {
        if sprite_index >= frame_count {
            bail!(
                "animation {name} references sprite index {sprite_index}, but pet has {frame_count} frames"
            );
        }
        frames.push(FramePlan {
            sprite_index,
            duration_ms,
        });
    }
    let fallback = if spec.fallback.is_empty() {
        "idle".to_string()
    } else {
        spec.fallback.clone()
    };
    let loop_start = if spec.loop_animation.unwrap_or(true) {
        Some(0)
    } else {
        None
    };
    Ok(AnimationPlan {
        frames,
        loop_start,
        fallback,
    })
}

/// 由 `fps` 换算单帧时长（毫秒），并执行 Codex 的取值范围校验。
fn duration_ms(name: &str, fps: Option<f64>) -> Result<u64> {
    let fps = fps.unwrap_or(DEFAULT_FPS);
    if !fps.is_finite() || fps <= 0.0 || fps > MAX_ANIMATION_FPS {
        bail!(
            "animation {name} fps must be finite and between 0 and {MAX_ANIMATION_FPS}, got {fps}"
        );
    }
    // Codex 存 `Duration`；渲染层按毫秒排队，这里取整并保证不为 0
    Ok(((1000.0 / fps).round() as u64).max(1))
}

/// 逐项校验：帧序列非空、索引在范围内、`fallback` 必须存在。
fn validate(animations: &HashMap<String, AnimationPlan>, frame_count: usize) -> Result<()> {
    // 键排序，保证报错稳定可复现
    let mut names: Vec<&String> = animations.keys().collect();
    names.sort();
    for name in names {
        let plan = &animations[name];
        if plan.frames.is_empty() {
            bail!("animation {name} must include at least one frame");
        }
        for frame in &plan.frames {
            if frame.sprite_index >= frame_count {
                bail!(
                    "animation {name} references sprite index {}, but pet has {frame_count} frames",
                    frame.sprite_index
                );
            }
        }
        if !animations.contains_key(&plan.fallback) {
            bail!("animation {name} fallback {} does not exist", plan.fallback);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// V1 网格（8 列 × 9 行）。
    const V1: GridSpec = GridSpec {
        width: 192,
        height: 208,
        columns: 8,
        rows: 9,
    };
    /// V2 网格（8 列 × 11 行）。
    const V2: GridSpec = GridSpec {
        width: 192,
        height: 208,
        columns: 8,
        rows: 11,
    };
    /// 造一个规格。
    fn spec(frames: Vec<usize>) -> AnimationSpec {
        AnimationSpec {
            frames,
            fps: None,
            loop_animation: None,
            fallback: String::new(),
        }
    }

    /// V1 缺省表应有 14 个动画：idle + 13 个具名（含 5 个别名）。
    #[test]
    fn v1_has_fourteen_animations() {
        let animations = build(V1, &HashMap::new()).expect("应构造成功");
        assert_eq!(animations.len(), 14, "实际：{:?}", {
            let mut names: Vec<&str> = animations.keys().map(String::as_str).collect();
            names.sort();
            names
        });
        for required in [
            "idle",
            "running-right",
            "running-left",
            "waving",
            "jumping",
            "failed",
            "waiting",
            "running",
            "review",
            "move_right",
            "move_left",
            "wave",
            "bounce",
            "sad",
        ] {
            assert!(animations.contains_key(required), "缺少动画 {required}");
        }
    }

    /// V2 在 V1 之上多出两行注视方向。
    #[test]
    fn v2_has_sixteen_animations() {
        let animations = build(V2, &HashMap::new()).expect("应构造成功");
        assert_eq!(animations.len(), 16);
        for name in LOOK_ROW_NAMES {
            assert!(animations.contains_key(name), "缺少动画 {name}");
        }
    }

    /// `idle` 帧序与时长必须与 Codex 逐帧一致。
    #[test]
    fn idle_matches_codex_frames_and_timings() {
        let animations = build(V1, &HashMap::new()).expect("应构造成功");
        let plan = &animations["idle"];
        let pairs: Vec<(usize, u64)> = plan
            .frames
            .iter()
            .map(|f| (f.sprite_index, f.duration_ms))
            .collect();
        assert_eq!(
            pairs,
            vec![(0, 1680), (1, 660), (2, 660), (3, 840), (4, 840), (5, 1920)]
        );
        assert_eq!(plan.loop_start, Some(0));
        assert_eq!(plan.fallback, "idle");
    }

    /// 动作动画：3 遍主序列 + idle 尾，循环点落在 idle 段开头；每遍末帧加长。
    #[test]
    fn action_animation_matches_codex_structure() {
        let animations = build(V1, &HashMap::new()).expect("应构造成功");
        let plan = &animations["running-right"];
        let expected_round = 8;
        assert_eq!(
            plan.frames.len(),
            expected_round * ACTION_REPEATS + IDLE_FRAME_MS.len()
        );
        assert_eq!(plan.loop_start, Some(expected_round * ACTION_REPEATS));
        // 第 1 行第 0 帧的索引是 8
        assert_eq!(plan.frames[0].sprite_index, 8);
        // 每遍的末帧用 last_ms，其余用 frame_ms
        for round in 0..ACTION_REPEATS {
            for column in 0..expected_round {
                let frame = &plan.frames[round * expected_round + column];
                let expected = if column + 1 == expected_round {
                    220
                } else {
                    120
                };
                assert_eq!(frame.duration_ms, expected, "第 {round} 遍第 {column} 帧");
            }
        }
    }

    /// 别名与主名必须共用同一行，这样降级映射才站得住。
    #[test]
    fn aliases_share_the_same_row() {
        let animations = build(V1, &HashMap::new()).expect("应构造成功");
        for (alias, primary) in [("wave", "waving"), ("bounce", "jumping"), ("sad", "failed")] {
            assert_eq!(
                animations[alias].frames, animations[primary].frames,
                "{alias} 应与 {primary} 同帧序"
            );
        }
        // move_right/move_left 是 running-right/running-left 的别名
        assert_eq!(
            animations["move_right"].frames,
            animations["running-right"].frames
        );
        assert_eq!(
            animations["move_left"].frames,
            animations["running-left"].frames
        );
    }

    /// 自定义网格：索引按真实列数算，不学 Codex 写死 8。
    #[test]
    fn custom_columns_use_real_stride() {
        let grid = GridSpec {
            width: 192,
            height: 208,
            columns: 4,
            rows: 9,
        };
        let animations = build(grid, &HashMap::new()).expect("应构造成功");
        // idle 只有 4 帧可用，索引必须落在第 0 行内
        for frame in &animations["idle"].frames {
            assert!(frame.sprite_index < 4, "越出第 0 行：{frame:?}");
        }
        // running-right 的第 1 行起点是 4，而不是 8
        assert_eq!(animations["running-right"].frames[0].sprite_index, 4);
    }

    /// 网格只有 1 行时，行动画整体缺席而不是报错。
    #[test]
    fn single_row_grid_still_loads() {
        let grid = GridSpec {
            width: 192,
            height: 208,
            columns: 8,
            rows: 1,
        };
        let animations = build(grid, &HashMap::new()).expect("应仍能加载");
        assert_eq!(animations.len(), 1);
        assert!(animations.contains_key("idle"));
    }

    /// 缺省 `fps` = 8 → 125ms；`loop` 缺省为 true。
    #[test]
    fn spec_defaults_match_codex() {
        let mut specs = HashMap::new();
        specs.insert("custom".to_string(), spec(vec![0, 1, 5]));
        let animations = build(V1, &specs).expect("应构造成功");
        let plan = &animations["custom"];
        assert!(plan.frames.iter().all(|f| f.duration_ms == 125));
        assert_eq!(plan.loop_start, Some(0));
        assert_eq!(plan.fallback, "idle");
        // 覆盖表是「逐项覆盖」，其余缺省动画仍在
        assert_eq!(animations.len(), 15);
    }

    /// `loop: false` → 一次性动画，播完交棒 `fallback`。
    #[test]
    fn loop_false_makes_one_shot() {
        let mut specs = HashMap::new();
        let mut one_shot = spec(vec![32, 33]);
        one_shot.loop_animation = Some(false);
        one_shot.fallback = "waiting".to_string();
        specs.insert("flash".to_string(), one_shot);
        let animations = build(V1, &specs).expect("应构造成功");
        assert_eq!(animations["flash"].loop_start, None);
        assert_eq!(animations["flash"].fallback, "waiting");
    }

    /// `fps` 越界必须被拒。
    #[test]
    fn out_of_range_fps_is_rejected() {
        for fps in [0.0, -1.0, 61.0, f64::NAN] {
            let mut specs = HashMap::new();
            let mut bad = spec(vec![0]);
            bad.fps = Some(fps);
            specs.insert("bad".to_string(), bad);
            let err = build(V1, &specs).expect_err("应被拒");
            assert!(
                format!("{err}").contains("fps must be finite"),
                "实际为 {err}"
            );
        }
    }

    /// 空帧序、越界索引、不存在的 fallback 都必须被拒。
    #[test]
    fn invalid_specs_are_rejected() {
        let cases: Vec<(&str, AnimationSpec)> =
            vec![("空帧序", spec(vec![])), ("索引越界", spec(vec![72]))];
        for (label, bad) in cases {
            let mut specs = HashMap::new();
            specs.insert("bad".to_string(), bad);
            assert!(build(V1, &specs).is_err(), "{label} 应被拒");
        }

        let mut specs = HashMap::new();
        let mut dangling = spec(vec![0]);
        dangling.fallback = "not-a-real-animation".to_string();
        specs.insert("bad".to_string(), dangling);
        let err = build(V1, &specs).expect_err("悬空 fallback 应被拒");
        assert!(format!("{err}").contains("does not exist"), "实际为 {err}");
    }

    /// 覆盖表不能把 `idle` 弄丢。
    #[test]
    fn idle_cannot_be_removed() {
        let mut specs = HashMap::new();
        let mut custom_idle = spec(vec![0, 1]);
        custom_idle.loop_animation = Some(false);
        specs.insert("idle".to_string(), custom_idle);
        let animations = build(V1, &specs).expect("应构造成功");
        assert_eq!(animations["idle"].frames.len(), 2);
        // idle 被改成一次性后，fallback 仍是 idle（自指），循环仍能自洽
        assert_eq!(animations["idle"].fallback, "idle");
    }
}
