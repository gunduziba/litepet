//! Tauri 构建脚本。

/// 把 `webview2-com-sys` 随依赖附带的 `WebView2Loader.dll` 摆到 `assets/win/`，
/// 好让 `tauri.windows.conf.json` 的 `bundle.resources` 把它捎进安装包。
///
/// 为什么非得自己带：`webview2-com-sys` 按 `target_env` 二选一 —— MSVC 链静态的
/// `WebView2LoaderStatic`，GNU（本机走的就是 GNU）链动态的 `WebView2Loader.dll`。
/// 而 Tauri 的 bundler 完全不认这个 DLL（`tauri-bundler` 源码里搜不到任何相关处理），
/// 不自己带的话，装出来的程序会直接报「缺少 WebView2Loader.dll」起不来。
///
/// 只在 Windows 上做：`tauri.windows.conf.json` 也只在 Windows 生效，macOS 包不受影响。
#[cfg(windows)]
fn stage_webview2_loader() {
    use std::path::{Path, PathBuf};
    use std::{env, fs};

    let Ok(out_dir) = env::var("OUT_DIR") else {
        return;
    };
    // `OUT_DIR` = `<target>/<profile>/build/litepet-<hash>/out`，往上是
    // litepet-<hash> → build → <profile>；取到 `build` 才能扫兄弟 crate 的产物目录。
    let Some(build_dir) = Path::new(&out_dir).ancestors().nth(2) else {
        return;
    };
    let Ok(entries) = fs::read_dir(build_dir) else {
        return;
    };

    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => return,
    };

    let source = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("webview2-com-sys-")
        })
        .map(|entry| {
            entry
                .path()
                .join("out")
                .join(arch)
                .join("WebView2Loader.dll")
        })
        .find(|candidate| candidate.is_file());
    let Some(source) = source else {
        // 依赖换了版本、产物目录还没生成时会走到这里；让打包阶段去报「资源缺失」就好。
        println!("cargo:warning=没找到 WebView2Loader.dll，安装包会缺它");
        return;
    };

    let destination = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("assets")
        .join("win")
        .join("WebView2Loader.dll");
    if let Some(parent) = destination.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // 内容一样就别重写：`tauri.windows.conf.json` 引着它，白动一次时间戳会多打一次包。
    let same = matches!(
        (fs::read(&destination), fs::read(&source)),
        (Ok(old), Ok(new)) if old == new
    );
    if !same {
        match fs::copy(&source, &destination) {
            Ok(_) => println!("已摆好 {}", destination.display()),
            Err(err) => println!("cargo:warning=摆 WebView2Loader.dll 失败：{err}"),
        }
    }
    // 依赖的产物变了、或者用户把摆好的那份删了（缺文件也算变更），都该重跑。
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed={}", destination.display());
}

fn main() {
    #[cfg(windows)]
    stage_webview2_loader();

    tauri_build::build()
}
