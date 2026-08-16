# 架构设计

## 进程边界

```text
macOS terminal
  └─ ssh
      └─ tmux pane
          └─ termway                         普通用户进程
              ├─ stdin: key/paste/SGR mouse
              ├─ stdout: ANSI/Kitty Graphics
              ├─ $NIRI_SOCKET: state/action
              ├─ Wayland socket: screencopy
              ├─ Wayland virtual pointer/keyboard
              └─ session D-Bus: idle inhibit
```

远程边界只有 SSH。termway 不监听 TCP，也不需要额外 service 或特权进程。

## 模块边界

```text
src/
  viewer.rs      状态机、TTY 生命周期、输出调度、坐标映射
  kitty.rs       Kitty transport、tile、atlas、tmux relative placements
  render.rs      half-block、cell diff、raster/viewport/tile
  screencopy.rs  wlr-screencopy session 与 buffer 复用
  capture.rs     damage watcher、原生捕获与 grim fallback
  input.rs       Wayland virtual pointer/keyboard
  niri.rs        JSON IPC 与 output geometry
  config.rs      用户画质配置、action palette 和进程环境
  idle.rs        ScreenSaver D-Bus inhibitor
```

捕获 viewport、terminal cell 和 niri output logical coordinates 通过显式结构传递，点击映射
先回到 capture source pixel，再映射到 output logical coordinate。

## 交互设计约束

termway 是一个短时进入、随用随走的工具，界面采用渐进披露，不要求用户记住完整状态机：

- 无配置即可启动；默认选择 focused output、1080p 和 `Auto` 画质；
- 常驻状态栏只回答“我在哪块屏、当前能做什么、画质如何”，不显示内部坐标和网格尺寸；
- `?` 是所有控制的统一发现入口，`g` 是画质与分辨率的唯一入口；设置行同时显示值、含义和
  当前可用方向键，避免把 renderer 参数变成快捷键；
- 配置文件复用界面中的 `quality` / `resolution` 词汇；damage 阈值、传输预算和恢复时间属于
  高级调优，默认示例不启用；
- 程序内调整只影响当前会话，避免一次试调意外修改持久配置；安全相关状态（远端鼠标、
  Keyboard 模式、idle inhibit）始终可见。

## 运行模式

### View mode

显示目标 output 或 viewport。当前版本通过持久的 wlr-screencopy Wayland 连接捕获完整
output，并跨帧复用 `wl_shm` buffer；协议不可用时回退到 grim。zoom、pan 和 pane resize
只进行本地重绘，按 `r` 手动重新捕获。原生 damage watcher 可用时，点击和键盘输入不再
额外捕获；grim 回退路径才会使用点击刷新和键盘 debounce。连续模式
使用独立 Wayland 连接执行 `copy_with_damage`，最高 5 FPS；主线程只轮询一个覆盖旧值的
latest-frame 槽，因此 SSH 输出慢时不会积压帧。手动 capture 仍走独立的立即返回路径，
不会在静止画面上等待 damage。

viewer 在启动时解析一个目标 output：命令行优先于配置文件，二者都未指定时使用 niri
focused output。screencopy、damage watcher、几何映射和 virtual pointer 都绑定同一个
`wl_output`，因此普通方向的不同 scale/负逻辑坐标互不混用。当前生命周期内不会切换 output，
也不处理 output 热插拔或非 `Normal` transform；全桌面拼接不在现有模型中。

ANSI 重绘采用以下策略：

- 不在帧开始时清空屏幕，而是以新图直接覆盖旧图；
- 每行完成后清理右侧残留，整张图完成后再清理下方残留；
- 使用 DEC synchronized update（CSI 2026）请求终端原子提交一帧；
- 边界按键和其他无状态变化的事件不触发绘制；
- 连续 cell 复用 ANSI 颜色状态，damage 帧只发送变化 run。

Kitty 模式缓存可配置分辨率的 navigation atlas；内容仍新鲜时，zoom/pan 只发送 source-crop
placement，默认 120ms idle 后用 128px cell-aligned tile refine。desktop damage 会令 atlas
失效，此时导航跳过旧 crop 和 120ms 延迟，直接从最新捕获帧生成 tile；画面静止 2 秒后在
任意 viewport 重建 atlas。tmux 路径用单个 Unicode placeholder 建立 pane 锚点，
atlas/tile 通过 relative placement 定位；crop 切换以 synchronized update 原子提交。输出
仍限制 burst；一次性 navigation refine 默认保持最高配置分辨率，只有在配置时间窗内达到
连续 damage 帧阈值后才按配置带宽和实际 PNG 大小降档。静止恢复 redraw 会强制最高档，避免
刚恢复就在同一帧被再次降档。fresh atlas 上只允许分辨率更高的 refine 覆盖。
atlas 的 4 KiB APC chunk 可独立调度普通终端控制；图形队列
未排空时保留旧 chunk，再追加 replacement，避免留下未完成的 `m=1` 上传。

### Control mode

显式按键进入后才转发普通输入，明显显示控制状态。固定 escape chord 永远由 termway 自己
处理。每次 Wayland 按键/按钮操作在一个调用中完成 press/release，断开不会遗留按下状态。

## tmux 语义

- termway 是普通前台程序，不修改 tmux server；
- resize 后重建 atlas 和 viewport；
- half-block renderer 不需要 tmux passthrough；
- Kitty renderer 只有探测成功后才启用；
- 不接管 tmux prefix，控制模式的 escape chord 默认避开 `C-b`。

## 安全边界

- 不监听公网或局域网端口；
- SSH 负责认证和加密；
- 不读取任何真实 input device；
- 日志禁止记录文本输入、paste 内容和原始按键流；
- 默认只读；只有显式 `--control` 才连接 virtual input protocols，点击仍需在 TUI 内按 `i`
  二次 arm。
