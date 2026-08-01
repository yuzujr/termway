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
  kitty.rs       Kitty transport、tile、atlas、tmux placeholders
  render.rs      half-block、cell diff、raster/viewport/tile
  screencopy.rs  wlr-screencopy session 与 buffer 复用
  capture.rs     damage watcher、原生捕获与 grim fallback
  input.rs       Wayland virtual pointer/keyboard
  niri.rs        JSON IPC 与 output geometry
  config.rs      action palette 配置和进程环境
  idle.rs        ScreenSaver D-Bus inhibitor
```

捕获 viewport、terminal cell 和 niri output logical coordinates 通过显式结构传递，点击映射
先回到 capture source pixel，再映射到 output logical coordinate。

## 运行模式

### View mode

显示目标 output 或 viewport。当前版本通过持久的 wlr-screencopy Wayland 连接捕获完整
output，并跨帧复用 `wl_shm` buffer；协议不可用时回退到 grim。zoom、pan 和 pane resize
只进行本地重绘，按 `r` 手动重新捕获。原生 damage watcher 可用时，点击和键盘输入不再
额外捕获；grim 回退路径才会使用点击刷新和键盘 debounce。连续模式
使用独立 Wayland 连接执行 `copy_with_damage`，最高 5 FPS；主线程只轮询一个覆盖旧值的
latest-frame 槽，因此 SSH 输出慢时不会积压帧。手动 capture 仍走独立的立即返回路径，
不会在静止画面上等待 damage。

ANSI 重绘采用以下策略：

- 不在帧开始时清空屏幕，而是以新图直接覆盖旧图；
- 每行完成后清理右侧残留，整张图完成后再清理下方残留；
- 使用 DEC synchronized update（CSI 2026）请求终端原子提交一帧；
- 边界按键和其他无状态变化的事件不触发绘制；
- 连续 cell 复用 ANSI 颜色状态，damage 帧只发送变化 run。

Kitty 模式缓存 1080p navigation atlas，zoom/pan 只发送 source-crop placement；120ms idle
后用 128px cell-aligned tile refine。tmux 路径使用完整 Unicode placeholder 坐标、限制
输出 burst，并按配置带宽和实际 PNG 大小选择 1080p–360p；静止后逐档恢复。

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
