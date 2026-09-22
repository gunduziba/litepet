//! 宠物窗口该不该露脸。
//!
//! 宠物是常驻的：不能没人连还杵在桌面上，也不能有人连了还躲着。
//! 判断只依赖「宿主数怎么变的」和「用户有没有手动藏过」，跟窗口、托盘都无关，
//! 所以单独放一个模块——托盘是 GUI，脚本点不到，这里的逻辑得能自己测。
//!
//! 一条硬规则：**自动逻辑只在事件上动作，不看「现在是不是 0 个」**。
//! 空转每拍都会报一次宿主数，要是按状态持续动作，用户刚从托盘点出来的宠物
//! 会被下一拍立刻再藏回去。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// 宠物窗口显隐的记忆。
#[derive(Default)]
pub struct Presence {
    /// 上一次看到的宿主数，用来认「刚断开」和「刚接上」这两个事件。
    last_hosts: AtomicUsize,
    /// 用户在托盘上手动藏过；藏过之后自动逻辑不再碰窗口。
    manually_hidden: AtomicBool,
}

impl Presence {
    /// 报一次当前宿主数，返回该把窗口切成什么样；`None` 表示别动它。
    pub fn report(&self, hosts: usize) -> Option<bool> {
        let previous = self.last_hosts.swap(hosts, Ordering::Relaxed);
        let hidden = self.manually_hidden.load(Ordering::Relaxed);
        decide(previous, hosts, hidden)
    }

    /// 托盘「隐藏宠物」：这是「我说了算」，自动逻辑从此不再碰窗口。
    pub fn hide_manually(&self) {
        self.manually_hidden.store(true, Ordering::Relaxed);
    }

    /// 托盘「显示宠物」：清掉手动状态，控制权交回自动。
    pub fn show_manually(&self) {
        self.manually_hidden.store(false, Ordering::Relaxed);
    }
}

/// 显隐决策本身：`Some(visible)` 就该切，`None` 表示什么都不做。
fn decide(previous: usize, hosts: usize, manually_hidden: bool) -> Option<bool> {
    // 手动藏过就是「我说了算」，后面两个事件都不该把它顶出来。
    if manually_hidden {
        return None;
    }
    // 只在「刚才还有人，现在没人了」这一下藏。
    if hosts == 0 && previous > 0 {
        return Some(false);
    }
    // 宿主连回来就露脸。
    if hosts > 0 && previous == 0 {
        return Some(true);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{decide, Presence};

    /// 最后一个宿主走了就藏。
    #[test]
    fn last_host_leaving_hides_the_pet() {
        assert_eq!(decide(1, 0, false), Some(false));
        assert_eq!(decide(2, 0, false), Some(false));
    }

    /// 有宿主连进来就露脸。
    #[test]
    fn a_host_arriving_shows_the_pet_again() {
        assert_eq!(decide(0, 1, false), Some(true));
        assert_eq!(decide(0, 3, false), Some(true));
    }

    /// 空转每拍都报同一个数，不能每拍都去动窗口。
    #[test]
    fn unchanged_host_count_does_nothing() {
        assert_eq!(decide(0, 0, false), None);
        assert_eq!(decide(1, 1, false), None);
    }

    /// 手动藏过之后，宿主来去都不该把宠物顶出来——用户是拿托盘点掉的。
    #[test]
    fn manual_hide_wins_over_automatic_logic() {
        assert_eq!(decide(0, 1, true), None);
        assert_eq!(decide(1, 0, true), None);
    }

    /// 按顺序报一遍，状态机自己认得事件，不靠调用方替它挑时机。
    #[test]
    fn presence_tracks_events_across_reports() {
        let presence = Presence::default();
        assert_eq!(presence.report(0), None, "启动时空转，别动");
        assert_eq!(presence.report(1), Some(true), "宿主接入");
        assert_eq!(presence.report(1), None, "还在，别动");
        assert_eq!(presence.report(0), Some(false), "走空了，藏");
        assert_eq!(presence.report(0), None, "一直空着，别动");
        assert_eq!(presence.report(2), Some(true), "又来人了，露脸");
    }

    /// 手动藏之后自动逻辑闭嘴；手动显示之后又交回自动。
    #[test]
    fn manual_override_can_be_handed_back() {
        let presence = Presence::default();
        presence.report(1);
        presence.hide_manually();
        assert_eq!(presence.report(0), None);
        assert_eq!(presence.report(1), None);
        presence.show_manually();
        assert_eq!(presence.report(0), Some(false), "交回自动后照常动作");
    }
}
