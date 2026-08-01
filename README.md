# termway

在 SSH 终端中查看和操作远端 Wayland 桌面，首先支持 NixOS + niri。

termway 的约束和一般远程桌面不同：

- macOS 端只需要现有的 SSH 客户端和终端模拟器；
- 不在 macOS 上安装原生 client、驱动或高权限组件；
- 不开放额外网络端口，SSH 的 PTY 是唯一的远程传输层；
- 可以作为普通全屏终端程序运行在 tmux window/pane 中；
- 所有屏幕捕获和输入注入均发生在远端 Linux 主机。

项目目前处于技术验证阶段。技术选型和架构见：

- [技术选型](docs/technical-selection.md)
- [架构设计](docs/architecture.md)
- [验证计划](docs/spikes.md)
- [ADR-0001：SSH-native 架构](docs/adr/0001-ssh-native.md)

## 目标体验

```console
$ ssh home
$ tmux new-window -n gui termway
```

启动后先显示 niri 窗口列表；选择窗口后显示远端画面，并把当前终端中的键盘和鼠标事件发送给该窗口。

## 非目标

第一阶段不实现音频、麦克风、文件传输、独立网络协议、macOS 原生客户端、完整视频播放或通用桌面环境支持。

## 当前状态

Spike 0 已实现，可以直接检查 niri session：

```console
nix develop --command cargo run -- doctor
nix develop --command cargo run -- events --count 10
```

单帧捕获和 truecolor half-block 输出已经可用：

```console
# 自动使用当前终端尺寸和 niri 的 focused output
nix develop --command cargo run --release -- capture

# 固定输出尺寸，便于测试或重定向
nix develop --command cargo run --release -- capture --cols 120 --rows 36

# 放大屏幕中央；移动中心点以查看其他区域
nix develop --command cargo run --release -- capture --zoom 3
nix develop --command cargo run --release -- capture --zoom 3 --center-x 0.25 --center-y 0.4
```

图像 escape sequence 写入 stdout，捕获耗时、渲染耗时和字节数写入 stderr。图像路径请使用 release build；debug build 的缩放性能不具有代表性。

termway 优先通过持久 Wayland 连接直接使用 `wlr-screencopy`，并在连续捕获之间复用
`wl_shm` buffer，不再为每次刷新启动截图进程。compositor 不支持该协议或原生捕获失败时，
会自动回退到 `grim`；`capture` 命令的 stderr 会显示实际 backend。

viewer 在原生 backend 上额外运行最高 5 FPS 的 damage watcher。桌面静止时 compositor
不会产生新帧；damage 发生在当前 viewport 之外，或可见像素实际没有变化时，也不会向
SSH 终端发送重复 ANSI 帧。手动刷新、点击后刷新仍使用立即完成的 capture，不会被
`copy_with_damage` 的等待语义阻塞。

交互查看器默认从 1× 全景打开：

```console
nix develop --command cargo run --release -- view
```

键位：

- 方向键或 `hjkl`：平移 viewport；
- `+`/`-`：逐级缩放；
- `1`–`9`：直接切换倍率；
- `0`：返回 1× 全景并居中；
- `c`：保持倍率并回到中心；
- `r`：重新捕获当前画面；
- 鼠标左键：向点击位置移动 viewport，并放大一级；
- 鼠标滚轮或触控板上下滚动：垂直平移 viewport；
- 触控板左右滚动：水平平移 viewport（取决于终端是否发送水平滚动事件）；
- `q`、Esc、Ctrl-C、Ctrl-D：退出。

鼠标事件使用终端的 SGR mouse protocol。tmux 会把事件转换成 pane 内坐标，因此 pane
在 window 中的位置不需要额外补偿；图像右侧/下方留白和状态栏中的点击会被忽略。

viewer 有两个互斥交互模式，鼠标控制则是独立的安全开关：

| mode line | 键盘 | 左键 |
| --- | --- | --- |
| `NAV \| READ-ONLY` | termway 导航命令 | 渐进聚焦并放大 |
| `NAV \| MOUSE:OFF` | termway 导航命令 | 渐进聚焦并放大 |
| `INPUT \| MOUSE:OFF` | 发送给远端窗口 | 渐进聚焦并放大 |
| `NAV/INPUT \| MOUSE:ON` | 取决于 NAV/INPUT | 点击远端桌面 |

`--control` 只授予远端输入能力，并不是一种运行模式：

```console
nix develop --command cargo run --release -- view --control
```

启动时为 `[NAV | MOUSE:OFF]`。在 NAV 中按 `i` 切换 `MOUSE:OFF/ON`；只有
`MOUSE:ON` 状态下的左键按下才会通过 niri 提供的 wlr virtual pointer 协议发送，
点击后会自动刷新一帧。该路径使用 Wayland 绝对坐标，不依赖 `ydotool`、`/dev/uinput`
或鼠标加速度。当前输入映射仅支持 niri 的 `Normal` output transform。

在 `--control` 模式下按 `t` 进入 `[INPUT]`，键盘事件会通过 Wayland virtual
keyboard 发送给当前聚焦的远端窗口。输入模式使用 `Ctrl-\` 作为 termway 前缀：

- `Ctrl-\ t`：返回 command mode；
- `Ctrl-\ r`：刷新画面；
- `Ctrl-\ q`：退出 termway；
- `Ctrl-\ i`：切换 `MOUSE:OFF/ON`；
- 连按两次 `Ctrl-\`：向远端发送一次 `Ctrl-\`。

在 legacy terminal keyboard protocol 中，`Ctrl-\` 与 `Ctrl-4` 都编码为 `0x1c`，
termway 会将这个字节统一解释为前缀；支持增强键盘协议的终端则没有这项歧义。

ASCII、方向/navigation、F1–F12 以及 Shift、Control、Alt、Super 使用 US evdev keymap。
非 ASCII 字符使用独立的动态 XKB keymap，将终端收到的 Unicode code point 直接发送给
远端应用，不依赖远端输入法。
成功发送键盘输入后会启动 250ms debounce；期间有新按键就重新计时，输入停止后自动
捕获并重绘一帧。显式 `Ctrl-\ r` 会取消尚未触发的自动刷新，避免重复捕获。

底部采用 Emacs 式两层信息区：mode line 持续显示当前模式、输出、倍率和 viewport；
echo area 显示刷新结果、点击坐标和错误。普通消息 2 秒后自动清空，错误保留 5 秒，
不会覆盖 mode line 中的控制状态。

高频滚动采用短窗口事件合并，只为一批输入重绘一帧；一次连续手势会锁定最初的
主导轴，过滤触控板的交叉轴噪声。持续的交叉轴输入可以突破锁定，短暂停顿也会自动
解锁，因此可以直接改变滑动方向。每帧位移有限幅，避免快速滑动造成事件排队或
viewport 突然越过过大范围。
