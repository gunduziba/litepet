//! pet-daemon 入口。
//!
//! 架构与约束见 `SPEC.md`，宠物包契约见 `docs/PET-PACK.md`，通信协议见 `docs/PROTOCOL.md`。

mod config;
mod pack;

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Mutex;
use tauri::Manager;

/// 已加载的宠物包，供渲染层与行为模块读取。
struct PetState(Mutex<Option<pack::LoadedPet>>);

/// 渲染层启动时拉取宠物包信息。
#[tauri::command]
fn pack_info(state: tauri::State<'_, PetState>) -> std::result::Result<pack::PetInfo, String> {
    let guard = state.0.lock().map_err(|err| err.to_string())?;
    guard
        .as_ref()
        .map(|loaded| loaded.info.clone())
        .ok_or_else(|| "宠物包尚未加载，请检查 ~/.litepet/pets".to_string())
}

/// 渲染层就绪握手：前端加载完宠物包后调用，便于确认宠物确实已上屏。
#[tauri::command]
fn renderer_ready(animations: usize) {
    println!("pet-daemon: 渲染层就绪，可用动画 {animations} 个");
}

/// 选中要加载的包 id：优先配置指定，否则取 `pets/` 下第一个可用包。
fn pick_pack(root: &Path, preferred: Option<&str>) -> Option<String> {
    if let Some(id) = preferred {
        if root.join(id).join("pet.json").is_file() {
            return Some(id.to_string());
        }
        eprintln!("pet-daemon: 配置的宠物包不可用，回退到自动挑选：{id}");
    }
    let mut ids: Vec<String> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().join("pet.json").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    ids.sort();
    ids.into_iter().next()
}

/// 读取配置并加载宠物包。
fn try_init_pet() -> Result<pack::LoadedPet> {
    let (cfg, created) = config::load_or_init()?;
    if created {
        println!(
            "pet-daemon: 已初始化家目录 {}",
            config::home_dir()?.display()
        );
    }
    let root = config::pets_dir()?;
    let id = pick_pack(&root, cfg.pet.as_deref())
        .with_context(|| format!("{} 下没有可用宠物包（需含 pet.json）", root.display()))?;
    let loaded = pack::load(&root, &id)?;
    println!(
        "pet-daemon: 已加载宠物包 {}（{}），{} 个动画，网格 {}x{}x{}x{}",
        loaded.info.id,
        loaded.info.display_name,
        loaded.info.animations.len(),
        loaded.info.frame.width,
        loaded.info.frame.height,
        loaded.info.frame.columns,
        loaded.info.frame.rows,
    );
    match loaded.behavior.as_ref().and_then(|b| b.get("behavior")) {
        Some(_) => println!("pet-daemon: 已读取 petdaemon.behavior 扩展配置"),
        // 纯 Codex 包没有这个键，此时全走 §4.4 降级映射
        None => println!("pet-daemon: 无 petdaemon.behavior 扩展，使用 Codex 降级映射"),
    }
    Ok(loaded)
}

/// 加载失败只告警，不阻塞窗口启动。
fn init_pet() -> PetState {
    match try_init_pet() {
        Ok(info) => PetState(Mutex::new(Some(info))),
        Err(err) => {
            eprintln!("pet-daemon: 宠物包加载失败：{err:#}");
            PetState(Mutex::new(None))
        }
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            app.manage(init_pet());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![pack_info, renderer_ready])
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}
