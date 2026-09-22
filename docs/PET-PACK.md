# 宠物包契约（pet pack v1）

> 版本：v0.2 ｜ 状态：**格式层已定稿** ｜ 更新：2026-09-21 ｜ 上位文档：`SPEC.md`
>
> 本文档沿用 `SPEC.md` §9 的事实分级：**✅ 已核实** / **⚠️ 待核实** / **⏳ 待决策**。禁止把 ⏳ 和 ⚠️ 当成结论写码。
>
> **格式层的依据是 Codex 官方源码，不是第三方文档。** 取证过程与三次自我纠正见 §9。

---

## 0. 一次决策反转（必读）

原设计（`SPEC.md` §3.4）照搬 Vetta 的**硬编码动作词表**：动作 id 即文件名，加动作 = 改程序。这与"宠物形象可扩展、不写死"冲突。

随后有两轮用户决策，**方向相反**：

| 轮次 | 决策原文 | 结论 |
|---|---|---|
| 早前 | 「`~/.litepet/pets` 为默认，环境变量可替代。**原生多文件（`actions/<id>.webp`），不上 spritesheet**」 | 我们自造 manifest + 逐动作多文件 |
| **最新（生效）** | 「**不要加载 `~/.codex/pets/` 的包。我们是使用 codex 的包格式**，然后自定义交互的标准，用于 pi 和 dsh 集成」 | **采用 Codex 包格式** + 自定义交互协议 |

**两者不可调和**：Codex 的 `AnimationSpec.frames` 是 **`Vec<usize>`（精灵索引）**，指向**单张图的网格**。多文件布局在 Codex 格式里**无法表达**。

本文档按**最新决策**定稿：**采用 Codex 包格式**。由此产生的连带后果：

- ✅ 「不上 spritesheet」这条**被推翻** —— 采用 Codex 格式就等于采用图集。这不是我替你改主意，是格式本身的约束。
- ✅ 素材需要**一次性离线打包**成图集，之后零运行时成本（§5）。这是打包步骤，不是运行时转码。
- ✅ 不扫描 `~/.codex/pets/`（用户明确要求）。目录归我们（§2）。
- ✅ 我们的扩展字段放进**同一个 `pet.json`**，Codex 会忽略它们（§4.1）—— **一份清单，两个消费者**。

---

## 1. 三层扩展模型

| 层 | 含义 | v1 是否支持 |
|---|---|---|
| **L1 换美术** | 换掉整套图片素材，动作集合不变 | ✅ |
| **L2 加动作** | manifest 声明任意动作 id，不限定词表 | ✅（Codex 格式原生支持） |
| **L3 改行为** | 声明「什么事件 → 播哪个动作」，不写死在代码里 | ✅（我们的扩展，§4） |

L2 由 Codex 格式**免费获得**：`animations` 是 `HashMap<String, AnimationSpec>`，键名任意，且有 `custom:` 前缀约定（§3.6）。

L3 是唯一能把 `SPEC.md` §3.4 里"策略逻辑要手写 Rust"这条成本压下去的办法：把 Vetta `session-event-action-policy.ts`（315 行）那类逻辑变成 manifest 里的规则表，Rust 侧只做通用解释器。

### 1.1 三个反例（✅ 已核实，均为源码静态勘察）

只看 L1 的话我们和它们没区别，没必要重写。取证如下。

| 项目 | 栈 | L1 换美术 | L2 加动作名 | L3 改行为/触发 |
|---|---|---|---|---|
| codex-pet-desktop | Tauri | ✅ | ❌ 常量写死 9 行 | 🟡 有 `behavior` 数据，但只能引用已有状态名 |
| PiDeck | Electron | ✅ | ❌ 常量写死 9 行 | ❌ 无任何行为字段 |
| PetPal Desktop | Tauri 2 | ✅ | 🟡 格式允许任意名 | ❌ 运行时只播硬编码子集 |

**codex-pet-desktop**（`src/app/renderer/constants.js`）把契约写成常量，连中文标签都在代码里：

```js
export const CELL_WIDTH = 192;   export const CELL_HEIGHT = 208;
export const ATLAS_WIDTH = 1536; export const ATLAS_HEIGHT = 1872;
export const STATES = {
  idle: { row: 0, frames: 6, fps: 5 },
  "running-right": { row: 1, frames: 8, fps: 10 },
  "running-left": { row: 2, frames: 8, fps: 10 },
  waving: { row: 3, frames: 4, fps: 6, once: true },
  jumping: { row: 4, frames: 5, fps: 8, once: true },
  failed: { row: 5, frames: 8, fps: 6, once: true },
  waiting: { row: 6, frames: 6, fps: 5 },
  running: { row: 7, frames: 6, fps: 7 },
  review: { row: 8, frames: 6, fps: 6 }
};
export const STATE_LABELS = { idle: "待机", "running-right": "向右走", /* … */ };
```

它值得学的**只有** manifest 里的行为数据（但取值只能是 `STATES` 里已存在的字符串，是"在固定 9 个动作里重排参数"，不是加动作）：

```json
"behavior": {
  "clickState": "waiting", "doubleClickState": "jumping",
  "idleStates": ["idle", "waiting", "review"], "wanderDirections": [-1, 1, 0],
  "natural": {
    "nextWanderDelayMs": [5200, 11000], "idleDurationMs": [1800, 4200],
    "walkDurationMs": [2600, 5600], "edgePauseMs": [900, 2200],
    "edgePauseStates": ["waiting", "review"], "postDragState": "waiting",
    "postDragMs": 900, "clickReturnState": "idle", "doubleClickReturnState": "idle"
  }
}
```

**PiDeck** 比看起来更死（`src/renderer/src/pet/PetSpriteSheet.ts`）：

```ts
export const GRID_COLS = 8; export const GRID_ROWS = 9;
export const CELL_W = 192;  export const CELL_H = 208;
export const MODE_ROW: Record<string, number> = {
  idle: 0, running: 7, failed: 5, waiting: 6, waving: 3,
  "running-right": 1, "running-left": 2, jumping: 4, review: 8,
};
export const MODE_FRAMES: Record<string, number> = { idle: 6, /* … */ };
```

社区包契约只有 4 个字段（`src/main/pet/PetPackageManager.ts`）——**包只能换一张同规格的图**：

```ts
type PetDexManifest = { id: string; displayName?: string; description?: string; spritesheetPath: string };
```

**PetPal Desktop** 是"格式能扩展、运行时不能"，这个坑最值得记。它的格式层真的开放——`petpet.json` 的 `actions` 是任意 key → 帧号区间：

```json
"actions": { "idle": [1,4], "walk": [5,8], "sit": [9,12], "sleep": [13,16],
             "jump": [17,20], "drag": [21,24], "happy": [25,28], "alert": [29,32] }
```

WebP 布局是 `actions/<任意名>/<任意名>.webp` + `action.json`（`{action, frames, fps, loop, webp, frameDelayMs}`），演示包里真有 `actions/look_right/` 和 `actions/walk_right/`。

**但运行时只认一小撮。** 在 `src/main.js` 里按动作名数出现次数：

```
idle 25   walk 20   jump 21   tap 12   alert 12   sleep 10   drag 5   happy 3
sit 0     look 0
```

演示包声明了 `sit: [9,12]`、目录里有 `look_right` —— **永远不会被触发**。
（证据强度：中等。静态计数，未跑运行时验证；但 0 次命中已足够判定不可达。）

同一文件里的更多死 schema：`anchor` 0 次、`sounds` 0 次、`portrait` 0 次；而 `demo-petpack/sounds/meow.mp3` 是个 **0 字节**占位文件。`personality.catchphrases` 全文件仅 **2 处命中，都在合成默认值时写入**（`` catchphrases: [`${rawManifest.name} 正在看你`, …] ``），**没有任何读取点** —— 写进去，没人显示。

Rust 侧 `collect_asset_paths` 分三支校验，错误串可直接复用：`"petpet.spritesheet is required"`、`"petwebp.actions is required"`、`"petwebp action.file is required"`。

### 1.2 结论

**"声明了但没接通"比直接不支持更糟**，因为用户会以为能用。所以 §6 强制加载期校验。

---

## 2. 目录约定（✅ 已定稿）

`~/.litepet/` 是本项目的**家目录**，**配置文件和宠物包都放这里**（mirror Codex 的 `~/.codex/` 约定）：

```
~/.litepet/
├── config.json          ← daemon 配置（窗口位置/大小、置顶、当前宠物、鉴权…）
└── pets/
    └── <id>/
        ├── pet.json
        └── spritesheet.webp
```

| 项 | 值 |
|---|---|
| 家目录 | `~/.litepet/` |
| 配置文件 | `~/.litepet/config.json` |
| 宠物包根目录 | `~/.litepet/pets/<id>/` |
| 环境变量覆盖 | `LITEPET_HOME`（覆盖整个家目录，`pets/` 与 `config.json` 随之迁移） |
| 是否扫描 `~/.codex/pets/` | **否**（用户明确要求，禁止） |

**空目录是预期状态，不是风险**：本项目是全新项目，`~/.litepet/` 由 daemon 首次启动时 `mkdir -p` 创建并写入默认 `config.json`。

> `config.json` 的 `auth` 段（只有一个 `token` 字段）见 `docs/PROTOCOL.md` §1：**token 由用户自己定，daemon 不生成也不轮换；填了就要鉴权，留空就不鉴权**。

环境变量只设**一个** `LITEPET_HOME` 覆盖整个家目录，而不是用 `LITEPET_PETS_DIR` 单独覆盖宠物目录。理由：**Codex 本身就是 `CODEX_HOME` 这个形状**（`{CODEX_HOME}/pets/<id>/pet.json`），沿用同一模式降低认知成本，也避开「配置在 A、宠物在 B」的割裂。

> **与 `SPEC.md` §3.6 的偏差**：那里写的是 macOS 原生路径 `~/Library/Application Support/litepet/config.json`。现改从 `~/.litepet/`（dotdir 约定）。取舍：放弃平台惯例，换来「配置与素材同处一地、可手工编辑、与 Codex 一致」。文件格式仍是 JSON（`config.json`），不改成 Codex 的 TOML。
>
> 注：`~/.codex/pets/<id>/` 里若存在同名包，daemon **不读**。我们只是「用 Codex 的格式」，不接管 Codex 的家目录。

---

## 3. 包格式：Codex `pet.json` 权威规格（✅ 已核实）

**依据**：`openai/codex` 仓库 `codex-rs/tui/src/pets/{model.rs,catalog.rs}`，以及本机 `@openai/codex@0.153.4` 二进制交叉验证。

### 3.1 包结构

```
~/.litepet/pets/<id>/
├── pet.json          ← 清单（必需；缺失则拒绝加载）
└── spritesheet.webp  ← 透明 WebP 图集（路径由 pet.json 指定）
```

**目录名才是包的规范身份，清单里的 `id` 只用于展示。** 两者可以不同：从 codex-pets.net 之类的地方下载来的包，解出来的目录名常带后缀（`kun-signature.codex-pet/`），而清单里写的 `id` 是 `kun-signature`。

所以 `pet/list` 的每一项同时给 `dir`（目录名，切换时传这个）与 `id`（清单值，用于显示）；daemon 侧 `pack::resolve_dir` 两者都认——先按目录名找，找不到再扫一遍目录比对清单 `id`。几个包的清单 `id` 撞车时报错并列出候选目录，不猜。

### 3.2 清单 `PetFile`（`model.rs:116-128`）

**六个字段，全部可缺省**（`id` 到 `animations` 都带 `#[serde(default)]`，`frame` 是 `Option`）。因此 `pet.json` 内容为 `{}` 在语法上合法。

```rust
#[derive(Debug, Deserialize)]
struct PetFile {
    #[serde(default)] id: Option<String>,
    #[serde(default, rename = "displayName")] display_name: Option<String>,
    #[serde(default)] description: Option<String>,
    #[serde(default, rename = "spritesheetPath")] spritesheet_path: Option<String>,
    frame: Option<FrameSpec>,
    #[serde(default)] animations: HashMap<String, AnimationSpec>,
}
```

**✅ 关键性质：没有 `deny_unknown_fields`。** 未知字段被 serde **静默忽略** —— 这是 §4 能用同一个 `pet.json` 承载我们扩展的全部依据。

字段名大小写：`displayName` / `spritesheetPath` 是 camelCase（有显式 `rename`），其余为 `id` / `description` / `frame` / `animations`。

### 3.3 网格 `FrameSpec`（`model.rs:131-155`）

```rust
#[derive(Debug, Clone, Copy, Deserialize)]
struct FrameSpec { width: u32, height: u32, columns: u32, rows: u32 }
```

四个字段**全部必填**（无 `#[serde(default)]`），但整个 `frame` 键可省略。省略时用 `Default`：

| 字段 | 默认值 | 常量 |
|---|---|---|
| `width` | `192` | `catalog::DEFAULT_FRAME_WIDTH` |
| `height` | `208` | `catalog::DEFAULT_FRAME_HEIGHT` |
| `columns` | `8` | `catalog::DEFAULT_FRAME_COLUMNS` |
| `rows` | `9` | `catalog::DEFAULT_FRAME_ROWS` |

**⚠️ 更正**：`1536×1872` 是**默认网格的乘积**，不是硬校验：

```rust
const SPRITESHEET_WIDTH: u32  = DEFAULT_FRAME_WIDTH  * DEFAULT_FRAME_COLUMNS;  // 1536
const SPRITESHEET_HEIGHT: u32 = DEFAULT_FRAME_HEIGHT * DEFAULT_FRAME_ROWS;     // 1872
```

因为它是算出来的，所以**二进制里没有这两个字面量**（我一开始因此误判为"不存在"，见 §9）。测试用例证明可以自由覆盖：

```rust
// model.rs:914 — columns 可以是 7
"frame": { "width": 192, "height": 208, "columns": 7, "rows": 9 }
// model.rs:932 — 极端值也合法
"frame": { "width": 8,   "height": 8,   "columns": 192, "rows": 234 }
```

**→ 这条对我们的价值极大**：我们可以自选单格尺寸和网格，不必迁就 192×208。

`frame_count = columns * rows`（运行时 `default_frame_count()`）。

### 3.4 动画 `AnimationSpec`（`model.rs:154-163`）

```rust
#[derive(Debug, Deserialize)]
struct AnimationSpec {
    #[serde(default)] frames: Vec<usize>,              // 显式精灵索引，不是"帧数"
    fps: Option<f64>,
    #[serde(rename = "loop")] loop_animation: Option<bool>,
    #[serde(default)] fallback: String,                // 一次性播完交给谁
}
```

**`frames` 是精灵索引数组**（`row * columns + column`），不是连续帧数。这意味着：

- 帧**不必连续**，可以任意挑、任意排序、跨行复用
- 同一格可以被多个动画引用，**图集可以共享帧，节省体积**
- 帧数可以超过 `columns`，靠多行铺开

### 3.5 运行时模型（`model.rs:33-74`）

```rust
pub struct AnimationFrame { pub sprite_index: usize, pub duration: Duration }
pub struct Animation { pub frames: Vec<AnimationFrame>, pub loop_start: Option<usize>, pub fallback: String }

pub struct Pet {
    pub id: String, pub display_name: String, pub description: String,
    pub spritesheet_path: PathBuf,
    pub frame_width: u32, pub frame_height: u32,
    pub columns: u32, pub rows: u32, pub frame_count: usize,
    pub animations: HashMap<String, Animation>,
}
```

源码文档注释（`model.rs:53-58`）明确了两条语义，**渲染层必须照做**：

> "Tracks use sprite indices into the already-decoded frame grid plus a fallback animation name for one-shot sequences. Callers should not assume an animation loops just because it has multiple frames; `loop_start == None` means the final frame eventually hands off to `fallback`."

- **`frames.len() > 1` 不等于会循环**，要看 `loop_start`
- **`loop_start == None`** → 一次性；播完最后一帧后交棒 `fallback`
- **`loop_start == Some(i)`** → 从第 `i` 帧起循环（**不是从 0**）

### 3.6 上限与约定常量

```rust
const MAX_PET_FRAMES: usize = 256;        // model.rs:29
const MAX_ANIMATION_FPS: f64 = 60.0;      // model.rs:30
pub(super) const CUSTOM_PET_PREFIX: &str = "custom:";  // model.rs:113
pub(super) const DEFAULT_PET_ID: &str = "codex";       // mod.rs:54
```

- `custom:` 前缀用于用户自定义宠物选择器（`custom_pet_selector(id)` → `format!("{CUSTOM_PET_PREFIX}{id}")`）

### 3.7 默认动画表（`default_animations()`，`model.rs:483-580`）

**14 个具名动画**。下表 `frames`/`fps` 为从源码推算的等效值（源码用 `Duration` 而非 fps 表达）：

| 动画名 | 行 | 帧数 | 单帧 ms | 末帧 ms |
|---|---|---|---|---|
| `idle` | — | 6（显式索引 `0..5`） | 1680 / 660 / 660 / 840 / 840 | 1920 |
| `running-right` | 1 | 8 | 120 | 220 |
| `running-left` | 2 | 8 | 120 | 220 |
| `waving` | 3 | 4 | 140 | 280 |
| `jumping` | 4 | 5 | 140 | 280 |
| `failed` | 5 | 8 | 140 | 240 |
| `waiting` | 6 | 6 | 150 | 260 |
| `running` | 7 | 6 | 120 | 220 |
| `review` | 8 | 6 | 150 | 280 |
| `move_right` | 1 | ↳ 同 `running-right` | | |
| `move_left` | 2 | ↳ 同 `running-left` | | |
| `wave` | 3 | ↳ 同 `waving` | | |
| `bounce` | 4 | ↳ 同 `jumping` | | |
| `sad` | 5 | ↳ 同 `failed` | | |

后 5 个是**别名**（`move_right`/`move_left`/`wave`/`bounce`/`sad` 分别指向与 `running-right`/`running-left`/`waving`/`jumping`/`failed` 相同的行）。我们的状态机应优先用**语义直白的那一组**（`bounce`/`sad`/`wave` 比 `jumping`/`failed`/`waving` 更贴合 agent 状态）。

> ⚠️ 第三方文档把 `failed` 写成不存在、把 `review` 也判为不存在 —— **两者都真实存在**，是它的行名表错了（见 §9）。

#### 3.7.1 `idle` 的精妙之处（值得原样抄）

```rust
fn idle_animation() -> Animation {
    Animation {
        frames: [(0,1680),(1,660),(2,660),(3,840),(4,840),(5,1920)] /* … */ ,
        loop_start: Some(0),          // 从 0 起循环
        fallback: "idle".to_string(),
    }
}
```

`idle` 的手写时长**刻意不均等**：两帧 660ms 的快速小动作夹在 1680ms/1920ms 的长停顿之间 → 看起来像"偶尔动一下"，不是机械循环。**照抄。**

#### 3.7.2 非 idle 动画的结构（`app_state_animation()`，`model.rs:600-628`）

```rust
let primary_frame_count = primary_frames.len() * 3;
let frames = primary_frames.iter()
    .chain(primary_frames.iter())
    .chain(primary_frames.iter())      // 主序列重复 3 次
    .cloned()
    .chain(idle_animation().frames)    // 再接 idle 的 6 帧
    .collect();
Animation { frames, loop_start: Some(primary_frame_count), fallback: "idle".to_string() }
```

**语义**：动作播 **3 遍** → **滑入待机**（复用 idle 的帧，不是硬切）→ 循环点落在**待机段的开头**，之后在待机里循环。

好处：不需要额外状态迁移，动作自然收尾，且循环时不会突兀地重播动作。**这是我们渲染层要复现的行为。**

注：`sprite_index = row_index * DEFAULT_FRAME_COLUMNS + column_index` —— 索引按**默认 8 列**算，与自定义 `columns` 无关。自定义网格的包必须**显式写全 `frames`**。

### 3.8 加载与解析顺序（`model.rs:81-240`）

选择器解析优先级：

1. **`path_like(value)`** → 当路径处理：是目录则用之，否则取其父目录；`canonicalize()` 归一化；清单取 `pet.json`，缺失则 `avatar.json`，都没有则报 `missing pet.json or avatar.json in {dir}`；`id` 回退为目录名
2. **`custom:<id>`** → 加载自定义宠物
3. **内置目录 id** → `catalog::BUILTIN_PETS` 里查（**8 个**：`codex` / `dewey` / `fireball` / `rocky` / `seedy` / `stacky` / `bsod` / `null-signal`）
4. 否则当自定义 id 处理

自定义 id 的查找路径（`load_custom_pet`）：

```
{CODEX_HOME}/pets/<id>/pet.json      ← 先查
{CODEX_HOME}/avatars/<id>/avatar.json ← 回退（legacy）
→ 都无：bail!("unknown pet {value}")
```

**`avatar.json` 是 legacy 别名** —— 我们的实现可选择不支持，但**必须能识别**，否则遇到老包会报错而不是忽略。

内置宠物的 `id` / `displayName` / `description` **不来自 manifest**，来自 `BUILTIN_PETS` 常量（`catalog.rs`），图集文件名形如 `codex-spritesheet-v4.webp`。

### 3.9 缓存指纹（`frame_cache_key()`，`model.rs:99-110`）

```rust
let bytes = fs::read(&self.spritesheet_path)?;
let digest = Sha256::digest(&bytes);
Ok(format!("sha256-{digest:x}-{}x{}-{}x{}", self.frame_width, self.frame_height, self.columns, self.rows))
```

**图集内容哈希 + 网格参数**。比 PiDeck 的 `mtimeMs:size` 强，且天然没有"只换图不改 manifest 就不失效"的坑。**我们照抄。**

### 3.10 真实社区包实测（✅ 地真值）

**下载自 codex-pets.net 的实际包 `xunjian-miao`（巡检喵），已装入 `~/.litepet/pets/xunjian-miao/`。**

`pet.json` 全文（**263 字节**）：

```json
{
  "id": "xunjian-miao",
  "displayName": "巡检喵",
  "description": "机警又耐心的黑猫巡检员，陪你定位根因、审查改动、守护每一次交付。",
  "spritesheetPath": "spritesheet.webp",
  "spriteVersionNumber": 2,
  "kind": "animal"
}
```

**关键：没有 `frame`，没有 `animations`。** 整个包只有图 + 一个版本号。

图集实测（`webpinfo` + PIL 双重确认）：

```
Width: 1536   Height: 2288   Alpha: 1   Animation: 0   Format: Lossless (VP8L)
is_animated: False   n_frames: 1   alpha 极值: (0, 255)   ≈ 1.62 MB
```

= **8 列 × 11 行**，单格 `192×208`，**静态单帧**、带真透明。站点侧 `validationReport` 与之一致（`atlasSize 1536x2288` / `cellSize 192x208` / `statesDetected 11`）。

> **静态单帧这一点很重要**：图集不需要运行时解码动画，`background-position` 切格就够。这直接消掉了当初担心的“spritesheet 要 JS 逐帧绘制”的成本。

#### 结论：`spriteVersionNumber` 决定行数

站点渲染器源码：`ef = { 1: 9, 2: 11 }`（版本号 → 行数）。

| 版本 | 行数 | 图集高 | 额外行 |
|---|---|---|---|
| 1 | 9 | `208×9 = 1872` | — |
| 2 | 11 | `208×11 = 2288` | 9–10 |

**这改变了实现策略**：`frame` 和 `animations` 在真实包里**都不存在**。读包顺序必须是：

1. 有 `frame` → 以其为准（Codex 允许自定义网格，见 §3.3）
2. 否则有 `spriteVersionNumber` → 行数取 `{1:9, 2:11}`，单格固定 `192×208`
3. 都没有 → 按缺省 `1`（9 行）处理

> V1 九行行名（取 `codex-rs/tui/src/pets/model.rs` 的 `default_animations()`，已逐行核对）：
>
> `idle` / `running-right` / `running-left` / `waving` / `jumping` / `failed` / `waiting` / `running` / `review`
>
> **⚠️ V2 的第 9–10 行**：社区文档称为注视方向（`look-directions-a/b`），**未从源码确证**，实现时以实际图集目视为准。
>
> 【我们的 `litepet.behavior.groups` 引用这套行名；纯 Codex 包走 §4.4 降级映射。】

> **素材许可提醒**：codex-pets.net 是社区上传站，包内**无 licence 字段**。`xunjian-miao` 仅作**开发用素材**，不随仓库发行；发行用内置包需换成明确许可（CC0）的素材。

---

## 4. 我们的扩展：`litepet` 命名空间

### 4.1 为什么能与 Codex 共存

Codex 的 `PetFile` **没有 `deny_unknown_fields`**（§3.2）→ 同一份 `pet.json` 里多出的键，Codex 静默忽略。因此：

```jsonc
{
  // ↓↓↓ Codex 读的（严格遵循 §3）
  "id": "demo-pet",
  "displayName": "示例宠",
  "description": "一只在屏幕角陪你写代码的小宠物",
  "spritesheetPath": "spritesheet.webp",
  "frame": { "width": 256, "height": 256, "columns": 8, "rows": 8 },
  "animations": {
    "idle":    { "frames": [0,1,2,3,4,5], "fps": 4 },
    "working": { "frames": [8,9,10,11,12], "fps": 8, "fallback": "idle" }
  },

  // ↓↓↓ 只有我们读的（Codex 忽略）
  "litepet": {
    "schemaVersion": 1,
    "license": "Apache-2.0",
    "behavior": {
      "idleTimeoutMs": 90000,
      "groups": {
        "idle":    ["idle"],
        "working": ["working"],
        "resting": ["rest_tea", "rest_sleep"],
        "feedback":["celebrate", "sad"]
      },
      "rules": [
        { "on": "agent.start", "play": "group:working",
          "bubble": { "kind": "status", "text": "开始工作" } },
        { "on": "agent.end", "when": { "success": true },
          "play": "celebrate", "bubble": { "kind": "success", "text": "任务完成" } },
        { "on": "agent.end", "when": { "success": false },
          "play": "sad", "bubble": { "kind": "info", "text": "任务失败" } },
        { "on": "tool.start", "bubble": { "kind": "tool", "text": "{toolName}" } }
      ]
    }
  }
}
```

**收益**：
- 单向依赖解除 → **同一份包，Codex 能读，我们也能读**（L1/L2 双向互操作）
- 我们的行为规则（L3）不污染 Codex 的字段空间
- 纯 Codex 包（没有 `litepet` 键）我们**照样能加载**，用 §3.7 的默认动画表跑

### 4.2 `litepet` 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `schemaVersion` | number | 我们这层扩展的版本，当前 `1` |
| `license` / `author` / `version` / `minAppVersion` | string | 元数据，设置页展示用（Codex 格式里没有这些） |
| `displaySize` | number | 显示边长（px），不填则按单格尺寸 |
| `behavior.idleTimeoutMs` | number | 无事件多久进 `resting`（默认 90000） |
| `behavior.groups` | `Record<string, string[]>` | 组名 → 动画 id 列表，组内轮换 |
| `behavior.rules` | `Rule[]` | §4.3 |

### 4.3 规则表求值

- `on` = 协议事件 `type`（清单见 `docs/PROTOCOL.md` §4）
- `when` = 对事件字段的等值/存在性匹配；缺省 = 总是匹配
- **首个匹配的规则生效**，顺序敏感
- 与 `docs/PROTOCOL.md` §6 仲裁**无关**：仲裁决定"哪个宿主说话"，规则表决定"播什么"
- `play` 取值：动画 id（必须在 `animations` 里存在）或 `group:<组名>`
- 省略 `play` → 不改当前动画，仅出气泡（`tool.start` 的常态）

一条规则的完整字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `on` | string | 协议事件名（`agent.start` / `agent.end` / `agent.settled` / `tool.start` / `tool.end` / `bubble`） |
| `when` | object | 字段等值匹配；缺省 = 总是匹配 |
| `play` | string | 动画 id 或 `group:<组名>`；缺省 = 不动画面 |
| `bubble` | `{kind, text}` | 气泡；`text` 支持 `{字段名}` 插值 |
| `alert` | `{sound, desktop, push}` | 提醒（见 §4.5）；缺省 = 不提醒 |

**可见性**：`play` / `bubble` 只作用于屏幕上的宠物；`alert` 是**另一个通道**——它的意义是在人**没在看屏幕**的时候起作用。两者可以同一条规则里同时给，也可以只给一个。

**⚠️ 与 Codex 的语义差异**：我们的 `working` / `resting` 等是**agent 状态**，Codex 的 `running-right` / `jumping` 是**角色动作**。二者不是 1:1（Codex 的走路/跳跃在我们的场景里没有对应）。所以**我们的包不复用 Codex 的行语义，只用它的格式**；遇到纯 Codex 包时按下面的映射降级。

### 4.4 纯 Codex 包的降级映射（✅ 已实现并实测）

| 我们的状态 | 取 Codex 包里的 | 理由 |
|---|---|---|
| `idle` | `idle` | 直接对应 |
| `working` | `running` | 唯一"持续进行"语义 |
| `resting` | `waiting` | 语义最接近 |
| `feedback.success` | `bounce`（缺则 `jumping`） | 正面 |
| `feedback.failure` | `sad`（缺则 `failed`） | 负面 |
| `feedback.celebrate` | `wave`（缺则 `waving`） | 庆祝 |
| — | `running-left` / `running-right` / `review` / `move_*` | **不映射**（桌面贴边场景无走路需求） |

取候选动画时**用清单里 `animations` 实际声明的键**，不是图集里的行名：Codex 的别名表（§3.7）已经把 `wave` / `bounce` / `sad` 指到同一行，所以两种写法都能命中。候选全缺时退到渲染层的兜底动画。

映射表**写在 Rust 常量里**（`src/behavior.rs` 的 `CODEX_FALLBACK`；因为 Codex 包里没有规则数据，只能由我们兜底），但**仅用于降级路径**；有 `litepet.behavior` 的包一律走规则表。

**重要差异**：带规则表的包里，`success` / `failure` / `celebrate` 三态共用 `feedback` 组并按 §4.2 的规则**组内轮换**；而降级路径是**逐状态**取动画（各查各的键）。所以同一个包里同时放正负两种反馈时，想让「失败」永远显示 `sad`，应当在规则里对 `agent.end` 显式写 `play`，而不是依赖 `feedback` 组。

### 4.5 提醒（`alert`）

```jsonc
{ "on": "agent.settled",
  "alert": { "sound": "@done", "desktop": true, "push": true } }
```

| 字段 | 类型 | 缺省 | 说明 |
|---|---|---|---|
| `sound` | string | `null` | 音效，见下面的解析顺序 |
| `desktop` | bool | `true` | 发系统通知 |
| `push` | bool | `true` | 推手机（是否真推还要看总开关，见 §4.6） |

**三个通道各自独立降级**：关掉声音不会连带关掉通知与推送；某一项失败（音效文件缺、系统没装、Bark 密钥写错）只让那一项失效，绝不拖累其他项，也绝不拖累宠物本身。三项都关（或 `alert` 整个省略）则这条规则不提醒。

`sound` 的解析顺序（按写法决定，不做猜测）：

| 写法 | 解释 |
|---|---|
| 含 `/`（或 `\\`） | 包内相对路径，相对包目录（如 `sounds/done.wav`） |
| `@语义名` | 语义音效名：`@done`（办妥了）、`@failed`（砸了）、`@attention`（要你看一眼）。按序试 **用户自选 → 应用自带兜底 → 系统音效** |
| 裸名字 | 先到包目录找同名文件，找不到再当系统音效名 |

后两种的区别不是排版问题：**裸名字的这两步顺序不能反过来**。反了之后 `"done.wav"` 会被当成一个叫 `done.wav` 的系统音效，解析不到、静默失声，而它在配置里看起来完全没错（`src/alert/sound.rs` 的 `resolve`）。

**找不到音效不是错误**：用户把包从别的平台搬过来、或系统没装某个音效，都是常见情况。记一条带建议的日志然后跳过声音通道。

**为什么用语义名，而不是系统音效名**：Windows 与 Linux 根本没有 macOS 那套音效，靠名字跨平台必然时灵时不灵（Windows 的 `Media\Windows Notify.wav` 对不上我们写的候选名）。语义名这条链的**前两层是文件、随安装包走**，两个平台上听感一致；系统音效那层只做保命。

#### 4.5.1 纯 Codex 包的默认提醒

规则表本就是可选的，但有两个语义是**跨包通用**的；少了它们，提醒链路对绝大多数现成包就是死的（用户会以为功能坏了，而不是以为「这个包没声」）：

| 事件 | 音效 | 提醒内容 |
|---|---|---|
| `agent.settled` | `@done` | 整轮结束且不会自动继续——最值得打断主人的一件事 |
| `agent.end`（**仅失败**） | `@failed` | 一轮以失败告终 |

这里必须写**语义名**，不能写 `Glass` / `Basso`：解析链里「应用自带兜底」那层是按语义名存的（`assets/sounds/@done.wav`），写成具体名字就绕过了它，于是 Windows 上整轮结束会完全静音——**用户报的「没声音」正是这个**。`src/behavior.rs` 的 `fallback_alert_uses_semantic_names` 单测锁住这条。

`agent.end` **成功不提醒**：屏幕上本来就在动，再响一次会很快变成噪声。有规则表的包写了自己的 `alert` 时，以规则表为准（`src/behavior.rs` 的 `fallback_alert` 只在没有任何规则命中该事件时兜底）。

#### 4.5.2 用户侧开关（`config.json` 的 `notify` 段）

包决定「哪些事件要提醒、响什么」，用户只决定「哪些通道允许响」以及「语义名的声音换成什么文件」。两者相与：包要响且用户开着，才真响。

```jsonc
// ~/.litepet/config.json
"notify": {
  "enabled": true,                              // 总开关，关掉则整层静默
  "sound": {
    "enabled": true,
    "volume": 0.35,
    "files": {                                  // 自选音效，留空则走自带兜底
      "done": "",                               // 整轮结束
      "failed": "",                             // 一轮失败
      "attention": ""                           // 要你看一眼
    }
  },
  "desktop": { "enabled": true },               // 系统通知
  "push": {                                    // 手机推送（Bark）
    "enabled": false,                          // 默认关：要用户自己填密钥
    "provider": "bark",
    "deviceKey": "",                           // 明文存在这里，所以文件 0600
    "endpoint": null                            // 自建服务时才填
  }
}
```

`files` 的三个槽位就是上面 `@done` / `@failed` / `@attention` 解析链的第一层，填**绝对路径**（或在相对路径下能找到的文件）。两层意思要分清：

- **自选只裁决语义槽位，不会劫持包作者点名的文件**。包作者写 `"sound": "sounds/boom.wav"` 是明确表态，用户配置不该覆盖它（`src/alert/sound.rs` 的 `user_choice_does_not_hijack_an_explicit_pack_path`）。
- **配的那个文件不在了，按「没配」处理，不报错也不静默**。同一份 `config.json` 会在两台机器上被读到，另一台配的路径在这台必然不存在——那时正确答案是往下层走。

整个 `notify` 段可缺省，缺省时逐字段补齐而不是整段报错；旧版配置里没有这个段、或 `files` 里少一个键，都能正常读（`src/config.rs` 的 `notify_section_defaults_when_absent` 单测锁定）。

> 这里没有「人在不在」「前台是不是终端」这类字段：那个判断不属于 litepet，见 §4.6。

### 4.6 为什么 litepet 不判断「人在不在」

一个真实需求：同一件事需要不同强度的提醒。人在终端里时宠物就在眼前，手机再震一下就是噪声；人不知道去哪了时，不推手机就等于没提醒。

这个判断**不在 litepet 里做**，早就该由宿主（pi / dsh）做。原因是它本来就属于宿主：

- 宿主自己就是那个前台运行的进程，比任何外部探测都清楚自己是不是在前台；
- 宿主知道自己的会话状态（正在跑、等用户输入、还是空闲）；
- 反过来，litepet 要拿到这些事实只能去起子进程问系统（实际实现过：macOS 的 `lsappinfo` 取前台 App、`ioreg -c IOHIDSystem` 取键鼠空闲 `HIDIdleTime`），还得维护一份必然漏的终端 App 白名单，而且从 Linux/Windows 上根本拿不到——为一个安静待在角落的宠物引平台探测与权限风险，不划算。

**所以 litepet 是哑的**：收到请求，按包规则表与全局开关发通知。它不问「人到底在不在」。

结果是 `plan()` 里只剩三个平级判断：规格要响 **且** 通道开着。三个通道彼此独立，没有任何一条依赖另一个的结果。

因此 `docs/PROTOCOL.md` 的事件里没有任何「在场」字段，`config.json` 的 `notify` 段里也没有对应开关：**这些都是宿主自己的事**。宿主判断出“现在不该出声”时，它不发那个事件、或发一个不同的事件即可。

> 提醒仍然是**异步**的：播声音要等解码器起来，推送要出网，所以一律丢后台线程，绝不让宿主的请求等着。

---

## 5. 素材生产：从 webp 到图集

Codex 格式要求**单张图集**。手上的素材如果是逐动作的独立动图（每个动作一个 animated WebP 之类），先按下面这套步骤拼成一张。

**打包步骤（一次性、离线、可复现）**：

1. 用 `ffprobe`/`webpinfo` 读出每个动作的帧数与尺寸
2. 选单格尺寸与列数（`frame` 可由我们自选，见 §3.3）
3. 用 `ffmpeg` 或 `webpmux` 把各动作的帧拼进一张 `spritesheet.webp`（保持 alpha）
4. 按实际铺放位置**显式写出每个动画的 `frames` 索引**（不要依赖 `row*8+col` 的默认推导）

**⚠️ 图集面积需要实测确认**：

| 单格 | 槽位数 | 图集面积 | 备注 |
|---|---|---|---|
| `440×440` | 64 | 12.4 MP | 原始尺寸，偏重 |
| `256×256` | 64 | 4.2 MP | 建议；显示 220px 时 1× 够用，2× 略欠 |
| `220×220` | 64 | 3.1 MP | 1× 正好 |

⏳ 需在目标机型上实测 WebP 解码耗时与显存占用后定稿。

**这是打包步骤，不是运行时转码** —— 产物入库，用户侧零成本。

---

## 6. 加载期校验（硬性）

**任何声明了但不可达的内容都必须被拒绝加载并报错**（§1.2 的教训）。

| 校验项 | 失败处理 |
|---|---|
| `pet.json` 缺失／JSON 解析失败 | 拒绝加载该包 |
| `frame` 为 `0`（任一维度） | 拒绝加载：`pet frame dimensions and grid counts must be non-zero`（Codex 原文） |
| 网格越界 | `pet frame grid width overflow` / `pet frame grid height overflow` / `pet frame count overflow`（Codex 原文，可复用） |
| 动画引用的 `sprite_index >= columns*rows` | 拒绝加载，报出动画名与索引 |
| `frames` 为空且无 `fallback` | 拒绝加载 |
| 动画名不在 `animations` 中却被 `behavior` 引用 | **拒绝加载**，错误串写明是哪个 id |
| `spritesheetPath` 缺失／文件不存在／解不出 alpha | 拒绝加载该包，不影响其他包 |
| `spritesheetPath` 越出包目录 | 拒绝加载（见 §7.2） |

`litepet.groups` 里引用的动画 id 同样必须存在，否则拒绝加载。

---

## 7. 工程约束（✅ 已核实，抄自三家）

### 7.1 素材读取用自定义协议，不要 base64

PiDeck 早期把整图 base64 成 data URL 经 IPC 传给渲染层，现已改为自定义协议 `pideck-pet://`（CSP 允许 `img-src pideck-pet:`），源码注释明写"不再经 IPC 搬运 base64 大字符串"。

Tauri 侧对应方案：`register_asynchronous_uri_scheme_protocol` 注册自定义 scheme，CSP 放行。⚠️ 具体 API 名待核实。

### 7.2 路径安全

`spritesheetPath` 来自用户可写的 manifest，视为**不可信输入**。PiDeck 的做法（`petPackageScanner.ts`）：`resolve(dir, json.spritesheetPath)` 之后强制校验结果仍在 `petsRoot` 内。我们照做。

### 7.3 内置素材不进包体

PiDeck 用 Electron `extraResources` 分发内置图到 `process.resourcesPath/pets`，注释理由是"避免将 6.2MB 的 webp 精灵图打包进 app.asar"。Tauri 侧对应 `bundle.resources`。

### 7.4 逐包 try/catch

用户目录里出现坏包是必然而非意外。PiDeck 是 `catch { /* 单个包失败不影响整体 */ }`，PetPal 有独立错误路径 `"failed to read petpack zip"` / `"failed to find asset"`。**一个包坏了，其余照常加载。**

### 7.5 缓存指纹

见 §3.9：`sha256-{图集字节哈希}-{w}x{h}-{cols}x{rows}`。Codex 的做法优于 PiDeck 的 `mtimeMs:size`，**无已知边界问题**。

---

## 8. 待办

- [x] ✅ 家目录与配置路径定稿：`~/.litepet/`（`config.json` + `pets/`）
- [x] ✅ 环境变量定稿：`LITEPET_HOME`（覆盖整个家目录）
- [ ] ⏳ 图集单格尺寸定稿（§5，需在目标机型上实测解码耗时与显存占用）
- [x] ✅ 纯 Codex 降级映射表定稿（§4.4，已在 `src/behavior.rs` 实现并有单测锁定）
- [x] ✅ 提醒契约定稿（§4.5 规则字段与音效解析、§4.6 职责边界，已在 `src/alert/` 实现）
- [ ] 定稿后回填 `SPEC.md` §3.4 / §3.8，并把原动作词表标注为"内置包示例"
- [ ] 改 `docs/PROTOCOL.md` §9：动作 id 表述改为"动画名"，并说明图集模型
- [ ] 补 `scripts/` 下的图集打包脚本（见 `SPEC.md` §7，当前仓库缺失，不可复现）
- [ ] ⚠️ 在目标机型上复测 WKWebView 的 animated WebP 透明（见 `SPEC.md` §9）

---

## 9. 取证过程与自我纠正（教训记录）

**错误做法**：用 `grep -oE "\b(1536|1872)\b"` 在 `strings` 输出里搜索。字符串是**拼接存储**的（如 `...displayName1536...`），`\b` 词边界因此不匹配 → **误判为"1536 不存在"**。

**三次纠正**：

| # | 我先前说 | 实际 | 依据 |
|---|---|---|---|
| 1 | 「`1536×1872` 二进制里不存在，是第三方编的」 | ❌ 错。**确实存在**（14 次 / 6 次），是默认网格的乘积 | `catalog.rs:10-15` |
| 2 | 「`failed` / `review` 是第三方编造的」 | ❌ 错。**两者都真实存在**。被二进制里一段连续 blob 骗了（只覆盖行 1–8） | `model.rs:483-580` |
| 3 | 「动画是固定行号的 8×9 网格」 | ❌ 错。是**具名 map + 显式精灵索引**，网格可自定义，帧可任意挑 | `model.rs:116-163` |

**保留的正确判断**：

| 说法 | 实际 | 依据 |
|---|---|---|
| `spriteVersionNumber` 字段 | ✅ **真实存在**（社区包的必需字段） | `~/.litepet/pets/xunjian-miao/pet.json` |
| `2288`（V2 图集高） | ✅ **真实存在**（`208×11`） | 同上，`webpinfo` 实测 |
| `look-directions-a/b` | ⚠️ 未确证；社区文档如此命名 | — |
| 「图集必须精确 1536×1872」是校验规则 | ❌ 是**默认值**，可覆盖 | `model.rs:137-155`、测试 `model.rs:914/932` |

**教训一**：不要用 `grep -oE "\b(1536|1872)\b"` 搜拼接字符串 —— `\b` 词边界会失效（如 `...displayName1536...`），导致误判“不存在”。

**教训二（更严重）**：`command -v codex` / `readlink -f` 拿到的是 **8790 字节的 JS 启动壳**（`@openai/codex/bin/codex.js`），不是 Rust 二进制。**拿它 grep 得到的所有“0 次命中”全是无效证据。** 真二进制在：

```
~/.nvm/versions/node/v24.18.0/lib/node_modules/@openai/codex/node_modules/
  @openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex   # 220 MB
```

**教训三**：**格式类结论以官方源码为准；真实包是最终地真值。** 源码说“网格可自定义”是真的，但真实包根本不用 `frame` —— 只靠源码会设计出没人用的读包逻辑。
