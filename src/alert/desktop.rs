//! 系统通知通道：走 `tauri-plugin-notification`。
//!
//! 用插件而不是各平台各自调 API，是因为它把 macOS 与 Windows 的通知中心、
//! 以及打包后需要的权限声明都处理好了——而打包正是我们要走的路。
//!
//! 首次发通知时系统会问一次「是否允许通知」，这是预期行为，不是错误。

use anyhow::Result;
use tauri_plugin_notification::NotificationExt;

/// 系统通知通道。
pub trait Notifier: Send + Sync {
    /// 弹一条系统通知。
    fn notify(&self, title: &str, body: &str) -> Result<()>;
}

/// 什么都不做的通知器：测试用。
#[cfg(test)]
#[derive(Debug, Default)]
pub struct SilentNotifier;

#[cfg(test)]
impl Notifier for SilentNotifier {
    fn notify(&self, _title: &str, _body: &str) -> Result<()> {
        Ok(())
    }
}

/// 用 Tauri 通知插件实现的通道。
#[derive(Clone)]
pub struct TauriNotifier {
    /// 应用句柄；Tauri 的通知 API 挂在它上面。
    app: tauri::AppHandle,
}

impl TauriNotifier {
    /// 构造。
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

impl Notifier for TauriNotifier {
    fn notify(&self, title: &str, body: &str) -> Result<()> {
        self.app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_notifier_never_touches_the_system() {
        let notifier = SilentNotifier;
        assert!(notifier.notify("标题", "正文").is_ok());
        let boxed: Box<dyn Notifier> = Box::new(SilentNotifier);
        assert!(boxed.notify("t", "b").is_ok());
    }
}
