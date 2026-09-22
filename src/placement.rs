//! 窗口摆位的坐标换算与可见性判断。
//!
//! 位置一律按**逻辑坐标**存取：`Moved` 给的是物理像素，除以窗口当时所在那块屏的
//! 缩放率之后才落盘，恢复时直接当逻辑坐标用。
//!
//! 这套约定必须两边对称。存物理值就等于把「将来用哪个缩放率还原」这个问题留给
//! 下次启动——猜错时窗口会落到两块屏之间的空处：内置屏 1470×956（缩放 2.0）与
//! 外接 1920×1080（缩放 1.0）并排时，外接屏上记下的 `2440, -288` 被再除一次 2
//! 得到 `1220, -144`，两块屏谁都不覆盖它。表现是「宠物还在显示，但哪儿都看不见」。
//!
//! 换算与兜底都放在这里，好在单测里把那次事故的坐标钉住。

/// 一块显示器在逻辑坐标系里的矩形（左上起、右下止，右边界与下边界为开区间）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorBox {
    /// 左边界（逻辑坐标）。
    pub left: f64,
    /// 上边界（逻辑坐标）。
    pub top: f64,
    /// 右边界（逻辑坐标），不含。
    pub right: f64,
    /// 下边界（逻辑坐标），不含。
    pub bottom: f64,
}

impl MonitorBox {
    /// 由显示器自报的物理几何造出来：位置与尺寸各自按**它自己的**缩放率折成逻辑坐标。
    ///
    /// 缩放率不是正数时按 1.0 处理——除零会算出 inf，比摆错地方更难查。
    pub fn from_physical(origin: (i32, i32), size: (u32, u32), scale: f64) -> Self {
        let scale = positive(scale);
        let left = f64::from(origin.0) / scale;
        let top = f64::from(origin.1) / scale;
        Self {
            left,
            top,
            right: left + f64::from(size.0) / scale,
            bottom: top + f64::from(size.1) / scale,
        }
    }

    /// 逻辑坐标点是否落在这块屏里。
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// 物理像素换成逻辑坐标。`scale` 要用**窗口当时所在那块屏**的缩放率。
pub fn to_logical(position: (i32, i32), scale: f64) -> (i32, i32) {
    let scale = positive(scale);
    (
        (f64::from(position.0) / scale).round() as i32,
        (f64::from(position.1) / scale).round() as i32,
    )
}

/// 这个逻辑坐标点落在任何一块屏上吗。一块都不落，说明它在屏与屏之间的空处。
pub fn lands_on_any(monitors: &[MonitorBox], x: f64, y: f64) -> bool {
    monitors.iter().any(|monitor| monitor.contains(x, y))
}

/// 兜底落点：主屏右下角往里收 `margin`；屏比宠物还小时贴住左上角。
///
/// 收边留白是为了整只宠物都在画面里——半只挂在屏幕外的宠物比看不见更难解释。
pub fn fallback_in(primary: &MonitorBox, pet_size: f64, margin: f64) -> (f64, f64) {
    let x = (primary.right - pet_size - margin).max(primary.left);
    let y = (primary.bottom - pet_size - margin).max(primary.top);
    (x, y)
}

/// 缩放率的兜底：非正数、NaN 一律当 1.0。
fn positive(scale: f64) -> f64 {
    if scale > 0.0 {
        scale
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 出那次事故的机器：内置屏 1470×956（缩放 2.0），外接 1920×1080（缩放 1.0）
    /// 摆在主屏右上——外接屏的物理原点因此是 `1470, -557`（y 向下，屏幕最上沿为负）。
    fn user_screens() -> Vec<MonitorBox> {
        vec![
            MonitorBox::from_physical((0, 0), (2940, 1912), 2.0),
            MonitorBox::from_physical((1470, -557), (1920, 1080), 1.0),
        ]
    }

    #[test]
    fn physical_is_divided_by_the_scale_of_the_screen_the_window_is_on() {
        // 内置屏上拖动：报上来的是逻辑坐标的两倍。
        assert_eq!(to_logical((800, 800), 2.0), (400, 400));
        // 外接屏缩放 1.0：报什么就是什么。
        assert_eq!(to_logical((2440, -288), 1.0), (2440, -288));
    }

    #[test]
    fn a_useless_scale_does_not_produce_infinities() {
        assert_eq!(to_logical((100, 200), 0.0), (100, 200));
        assert_eq!(to_logical((100, 200), f64::NAN), (100, 200));
        let boxed = MonitorBox::from_physical((0, 0), (100, 100), 0.0);
        assert_eq!((boxed.right, boxed.bottom), (100.0, 100.0));
    }

    #[test]
    fn the_position_recorded_in_the_accident_is_off_every_screen() {
        let screens = user_screens();
        // 事故坐标：配置里存着物理 "2440, -288"，被再除一次 2 得到 "1220, -144"。
        assert!(
            !lands_on_any(&screens, 1220.0, -144.0),
            "落在两块屏之间的空处"
        );
        // 同一个物理值按外接屏的缩放率还原，才是它本来的位置。
        assert!(lands_on_any(&screens, 2440.0, -288.0), "外接屏上");
        assert!(lands_on_any(&screens, 400.0, 400.0), "内置屏上");
        // 右边界与下边界是开区间：贴着屏幕右下外沿的点不算在屏内。
        assert!(!lands_on_any(&screens, 1470.0, 956.0));
        assert!(lands_on_any(&screens, 1469.0, 955.0));
    }

    #[test]
    fn a_position_survives_the_round_trip_on_either_screen() {
        let screens = user_screens();
        for (physical, scale) in [((800, 800), 2.0), ((2440, -288), 1.0)] {
            let (x, y) = to_logical(physical, scale);
            assert!(
                lands_on_any(&screens, f64::from(x), f64::from(y)),
                "{physical:?} 除以其屏幕的缩放率后应当还在屏上"
            );
        }
    }

    #[test]
    fn the_fallback_lands_inside_the_primary_screen() {
        let screens = user_screens();
        let (x, y) = fallback_in(&screens[0], 132.0, 24.0);
        assert_eq!((x, y), (1314.0, 800.0));
        assert!(lands_on_any(&screens, x, y));
        // 宠物比屏幕还大时不许算出负坐标，贴住左上角。
        let tiny = MonitorBox::from_physical((0, 0), (100, 100), 1.0);
        assert_eq!(fallback_in(&tiny, 400.0, 24.0), (0.0, 0.0));
    }
}
