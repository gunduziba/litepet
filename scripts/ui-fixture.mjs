// 设置页的假数据与假 RPC 分发。
//
// 两处共用：`check-ui.mjs`（DOM 替身跑一遍，查字段名与崩溃）与
// `preview-ui.mjs`（真浏览器渲染截图）。数据只有这一份，改真接口时改这里。
//
// 里面的 JSON 是从真 daemon 的 `config/get` / `pet/list` 原样抄回来的，
// 不是照着 Rust 结构体写出来的——照着写就会把字段名猜错，那正是这套检查要防的事。

/** 真实 `config/get` 返回。 */
export const CONFIG_GET = {
  config: {
    alwaysOnTop: true,
    auth: { token: 'my-own-secret-abc123' },
    notify: {
      desktop: { enabled: true },
      enabled: true,
      push: { deviceKey: '', enabled: false, endpoint: null, provider: 'bark' },
      sound: {
        enabled: true,
        volume: 0.35,
        // 三种写法各有一个：空（用包内/自带兑底）、包内相对路径、用户挑的绝对路径。
        files: {
          attention: '/Users/eee/Music/bell.mp3',
          done: '',
          failed: 'sounds/boom.wav',
        },
      },
    },
    pet: null,
    port: 4590,
    size: 220,
    x: null,
    y: null,
  },
  home: '/Users/eee/.litepet',
  justCreated: false,
  log: '/Users/eee/.litepet/logs/daemon.log',
  petsRoot: '/Users/eee/.litepet/pets',
  version: '0.1.0',
};

/** 真实 `pet/list` 返回：一个正常包（巡检喵）+ 一个坏包。 */
export const PET_LIST = {
  current: 'xunjian-miao',
  pets: [
    {
      description: '机警又耐心的黑猫巡检员，陪你定位根因、审查改动、守护每一次交付。',
      dir: 'xunjian-miao',
      displayName: '巡检喵',
      frame: { columns: 8, height: 208, rows: 11, width: 192 },
      id: 'xunjian-miao',
      problem: null,
      spritesheetPath: '/Users/eee/.litepet/pets/xunjian-miao/spritesheet.webp',
    },
    {
      description: '',
      dir: 'broken-pack',
      displayName: 'broken-pack',
      frame: null,
      id: 'broken-pack',
      problem: '图集缺 spritesheet 字段',
      spritesheetPath: null,
    },
  ],
};

/**
 * 假 RPC 分发。未知方法直接抛——页面调了没预料到的接口必须炸出来，
 * 不然以后多一个接口这里会静默返回 `undefined`，看起来一切正常。
 */
export function respond(method, params) {
  switch (method) {
    case 'config/get':
      return CONFIG_GET;
    case 'pet/list':
      return PET_LIST;
    case 'pet/select':
      return { id: params.id };
    case 'config/set':
      return { config: { ...CONFIG_GET.config, ...params }, restartRequired: [] };
    case 'notify/test':
      return { sound: '默认音效', desktop: true, push: false };
    case 'notify/preview':
      // 复刻真 daemon 的两种结果（见 src/alert/mod.rs 的 `preview`）：
      // 能落到文件上就报路径与来路，落不到就 `path: null` + 一条能照着做的建议。
      return params.sound.includes('does-not-exist')
        ? { sound: params.sound, path: null, layer: null, hint: '这个文件不存在，换个文件试试' }
        : { sound: params.sound, path: '/tmp/picked.wav', layer: 'user', hint: null };
    default:
      throw new Error(`设置页调了未预期的 RPC：${method}`);
  }
}
