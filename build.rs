//! Tauri 构建脚本。

/// 把 `webview2-com-sys` 随依赖附带的 `WebView2Loader.dll` 摆到 `assets/win/`，
/// 好让 `tauri.windows.conf.json` 的 `bundle.resources` 把它捎进安装包。
///
/// 为什么非得自己带：`webview2-com-sys` 按 `target_env` 二选一 —— MSVC 链静态的
/// `WebView2LoaderStatic`，GNU（本机走的就是 GNU）链动态的 `WebView2Loader.dll`。
/// 而 Tauri 的 bundler 完全不认这个 DLL（`tauri-bundler` 源码里搜不到任何相关处理），
/// 不自己带的话，装出来的程序会直接报「缺少 WebView2Loader.dll」起不来 ——
/// `objdump -p` 能看到它是 exe 的**硬导入**，不是可选优化。
///
/// 为什么 `assets/win/WebView2Loader.dll` **也入库**：Cargo 里本包的 build script
/// 只依赖 build-dependencies（`tauri-build`），而 `webview2-com-sys` 挂在 `tauri`
/// 的**运行时**依赖下，两者之间没有先后边，**可以并行跑**。于是冷 target 第一次
/// 构建时这里可能抢跑，读不到 `out/<arch>/` 里的产物。入库一份正好补上这个空档；
/// 下面这段拷贝也就退化成「依赖升级时自动更新」，不再是唯一来源。
///
/// 只在 Windows 上做：`tauri.windows.conf.json` 也只在 Windows 生效，macOS 包不受影响。
#[cfg(windows)]
fn stage_webview2_loader() {
    use std::path::{Path, PathBuf};
    use std::{env, fs};

    let destination = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("assets")
        .join("win")
        .join("WebView2Loader.dll");

    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        Ok(other) => {
            println!("cargo:warning=WebView2Loader.dll 没有 {other} 架构的预置文件，跳过");
            return;
        }
        Err(_) => return,
    };

    // 扫 `<target>/<profile>/build/` 下兄弟 crate 的产物目录，找 webview2-com-sys 摆好的那份。
    let source = env::var("OUT_DIR")
        // `OUT_DIR` = `<target>/<profile>/build/litepet-<hash>/out`，往上是
        // litepet-<hash> → build → <profile>；取到 `build` 才能看到兄弟 crate。
        .ok()
        .and_then(|out_dir| Path::new(&out_dir).ancestors().nth(2).map(Path::to_path_buf))
        .and_then(|build_dir| fs::read_dir(build_dir).ok())
        .and_then(|entries| {
            entries
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
                .find(|candidate| candidate.is_file())
        });

    let Some(source) = source else {
        // 见文件头：冷构建时这里可能抢在 `webview2-com-sys` 前面跑。那就用入库那份兜底；
        // 两份都没有必须当场报错 —— 打条 warning 放过去，会产出一个「装完起不来」的包，
        // 那种故障要等用户装上去才暴露，比在这里失败难查得多。
        assert!(
            destination.is_file(),
            "既拿不到 WebView2Loader.dll 的构建产物，仓库里也没有 assets/win/WebView2Loader.dll。\
             先跑一次 `cargo build` 让 webview2-com-sys 产出，或从版本库恢复该文件。"
        );
        println!("cargo:rerun-if-changed={}", destination.display());
        return;
    };

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
