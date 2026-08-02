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

启动后显示 niri 当前 output；用 click-to-focus/zoom 定位目标，按需 arm 鼠标或进入 INPUT
模式，把当前终端中的键盘、鼠标和滚动事件发送给桌面。常用程序和 compositor action 由
配置式 action palette 提供入口。

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
不会产生新帧；damage 发生在当前 viewport 之外时也不会重绘。同一 viewport 下会比较
新旧终端 cell，只发送变化的连续 cell 区间；完全相同的 cell buffer 不产生图像输出。
ANSI 编码器会在连续 cell 间复用前景/背景色状态并把同时变化的颜色合成一条 SGR；同一
120×38 实际桌面样例从 baseline 的 160,521 bytes 降到 97,833 bytes（约减少 39%）。
后台 watcher 只保留最新帧，慢速渲染期间到达的旧帧会被覆盖。手动刷新使用立即完成的 capture，不会被
`copy_with_damage` 的等待语义阻塞。

viewer 默认使用 `--graphics auto`。直连 Kitty、WezTerm 或 Ghostty 时会尝试
[Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)：viewport 的传输
分辨率最高为 1920×1080，但 placement 仍按原始宽高比铺满 terminal pane。画面按 terminal
cell 对齐，拆成约 128×128 像素的稳定 PNG tile；damage 到来时使用双缓冲 image ID 完整
替换发生变化的 tile，新 tile 显示后才回收旧 tile，不使用 Kitty animation frame patch。
整帧会先归一到 terminal cell 的精确宽高比，避免各 tile 独立缩放产生接缝；每个 tile 使用
完整协议单元无空窗替换。首次显示会在 terminal 内缓存一张全屏 navigation atlas；缩放和
平移立即通过 Kitty 的 source crop 重新 placement，不重新缩放、编码或传输像素，输入停止
120ms 后再用高分辨率 tile 覆盖预览。tmux 下只写一个 Unicode placeholder 作为 pane
左上角锚点，atlas 和 tile 都使用 Kitty relative placement 挂到该锚点；zoom/pan 因而只需
更新真正支持 source crop 的普通 placement，不会重写整屏 placeholder，也不会短暂退回 1×
原图。atlas crop、旧 tile 回收和锚点变更放在同一个 DEC synchronized update 中提交。
图片位置仍随 tmux 的 pane 和 resize 正常移动。
refine 还遵守单调画质约束：fresh atlas crop 的有效像素数如果不低于带宽自适应后的 raster，
就保留 atlas，不允许 720p/540p/360p tile 反向覆盖成更模糊的画面；atlas 已因 desktop damage
过期时则优先显示较低分辨率的新内容，并在静止后刷新高质量 atlas。
modeline 会在 crop 阶段显示 `GFX:KITTY/PREVIEW`，refine 后显示实际档位（例如
`GFX:KITTY/720p`）；ANSI 则显示 `GFX:ANSI`。

1× 全景发生 damage 后，变化先走 tile diff；画面静止 2 秒会生成一次新的高质量 atlas
keyframe 并回收 tile generations。这样后续 click-to-focus 使用的是当前桌面，而不是启动时
的旧截图，同时视频/动画期间不会反复发送完整 atlas。

Kitty raster 在 PNG 前丢弃每个 sRGB channel 的最低 1 bit，单通道最大误差仅 1/255，视觉上
等同原图，同时抑制截图低位噪声。固定 1080p 桌面样例的完整 PNG 从 1,347,696 bytes 降到
1,106,338 bytes（约减少 18%），编码也从 8.8ms 降到 6.6ms；tile diff 同样受益。

tmux 下的 refined frame 以约 275ms 为单帧传输预算；默认 40 Mbit/s 时约为 1.38 MB。
如果视频或大面积动画令无损 PNG
超过预算，会按编码后的实际大小直接降到 900p/720p/540p/360p 中合适的一档，而不是先把
超大帧塞进 tmux；modeline 显示当前档位。画面静止 2 秒后逐档恢复，避免动画期间反复抖动，
同时让停止后的文字自动回到最高精度。navigation atlas 始终保留清晰的 1080p 全局预览。

Kitty 协议输出使用非阻塞 PTY 和完整协议事务队列。一张 atlas 或一个 128×128 tile 的所有
chunk 都作为不可拆分上传事务，tile placement 也包含在同一事务中，满足协议“不在 `m=1`
上传中插入其他图形命令”的要求；这也避免 resize 丢弃 atlas 的后续 chunk。控制
最多等待当前 tile（最坏的无损 RGB 数据经 base64 后约 66 KiB），不会等待整帧。上一帧仍在
发送时不会继续堆积 damage 帧，只记录一次最新画面重绘请求。modeline、echo 和输入状态输出
优先于后续图像事务，退出时最多补完当前协议事务，随后
丢弃尚未发送的图像，避免低速 tmux/SSH 链路阻塞控制。tmux 会主动吸收 pane PTY 输出、无法
把真实 client 背压传回程序，因此 termway 在 tmux 下默认把图像输出平滑限制在 40 Mbit/s、
只允许 16 KiB burst；控制输出不受此限制，避免它排在 tmux 已缓存的数 MB 图像之后。可按
实测 SSH 路径调整，例如 50 Mbit/s 链路保守使用：

```console
termway view --graphics kitty --tmux-bandwidth-mbps 40
```

更快的中转可以提高该值，画质预算会随之同步增加；直连 Kitty 不使用这个限速参数。

可以显式选择或排错：

```console
termway view --graphics kitty   # 要求 Kitty Graphics 可用，否则报错
termway view --graphics ansi    # 强制使用 truecolor half-block + cell diff
```

tmux 至少需要：

```tmux
set -g allow-passthrough on
```

tmux 图形路径目前要求实际 client 为 Kitty 0.31+（relative placement 在 0.31 加入）；其他
client 在 `auto` 模式下安全回退到 ANSI，避免声称支持基础 Kitty Graphics、实际却无法实现
无像素重传的 source-crop navigation。

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
- `MOUSE:ON` 时鼠标左键/右键：向远端桌面发送对应点击；
- 鼠标滚轮或触控板上下滚动：垂直平移 viewport；
- 触控板左右滚动：水平平移 viewport（取决于终端是否发送水平滚动事件）；
- `s`（需要 `--control`）：切换滚动控制 viewport 或远端桌面；
- `a`（需要 `--control`）：切换是否阻止远端桌面进入 idle；
- `x`：打开配置驱动的 action palette；
- `q`、Esc、Ctrl-C、Ctrl-D：退出。

鼠标事件使用终端的 SGR mouse protocol。tmux 会把事件转换成 pane 内坐标，因此 pane
在 window 中的位置不需要额外补偿；图像右侧/下方留白和状态栏中的点击会被忽略。

viewer 有两个互斥交互模式，鼠标控制则是独立的安全开关：

| mode line | 键盘 | 左键 | 右键 |
| --- | --- | --- | --- |
| `NAV \| READ-ONLY` | termway 导航命令 | 渐进聚焦并放大 | 无操作 |
| `NAV \| MOUSE:OFF` | termway 导航命令 | 渐进聚焦并放大 | 无操作 |
| `INPUT \| MOUSE:OFF` | 发送给远端窗口 | 渐进聚焦并放大 | 无操作 |
| `NAV/INPUT \| MOUSE:ON` | 取决于 NAV/INPUT | 远端左键 | 远端右键 |

`--control` 只授予远端输入能力，并不是一种运行模式：

```console
nix develop --command cargo run --release -- view --control
```

启动时为 `[NAV | MOUSE:OFF | SCROLL:VIEW]`。在 NAV 中按 `i` 切换 `MOUSE:OFF/ON`；只有
`MOUSE:ON` 状态下的左键或右键按下才会通过 niri 提供的 wlr virtual pointer 协议发送，
点击后会自动刷新一帧。该路径使用 Wayland 绝对坐标，不依赖 `ydotool`、`/dev/uinput`
或鼠标加速度。当前输入映射仅支持 niri 的 `Normal` output transform。

在 `--control` 模式下按 `t` 进入 `[INPUT]`，键盘事件会通过 Wayland virtual
keyboard 发送给当前聚焦的远端窗口。输入模式使用 `Ctrl-\` 作为 termway 前缀：

- `Ctrl-\ t`：返回 command mode；
- `Ctrl-\ r`：刷新画面；
- `Ctrl-\ q`：退出 termway；
- `Ctrl-\ i`：切换 `MOUSE:OFF/ON`；
- `Ctrl-\ s`：切换 `SCROLL:VIEW/DESKTOP`；
- `Ctrl-\ a`：切换远端 idle inhibit；
- `Ctrl-\ x`：打开 action palette；
- `Ctrl-\` 后接 `+`、`-`、`0`–`9`、`c`、方向键或 `hjkl`：在 INPUT
  模式中缩放、复位或平移 viewport；
- 连按两次 `Ctrl-\`：向远端发送一次 `Ctrl-\`。

在 legacy terminal keyboard protocol 中，`Ctrl-\` 与 `Ctrl-4` 都编码为 `0x1c`，
termway 会将这个字节统一解释为前缀；支持增强键盘协议的终端则没有这项歧义。

ASCII、方向/navigation、F1–F12 以及 Shift、Control、Alt、Super 使用 US evdev keymap。
非 ASCII 字符使用独立的动态 XKB keymap，将终端收到的 Unicode code point 直接发送给
远端应用，不依赖远端输入法。

Mac 的 Option 可以在终端配置允许时作为 Alt 发送；Command/Super 通常由本地终端处理，
无法可靠穿过 SSH。Wayland virtual keyboard 可以把组合键发给聚焦应用，但不能触发
niri 的 compositor-global bindings；这类入口适合放进 action palette。

`view --control` 默认通过 `org.freedesktop.ScreenSaver` 阻止远端进入 idle，modeline
显示 `IDLE:BLOCK`。退出 viewer 会自动解除；如果要把 viewer 留在 detach 的 tmux pane
中并允许正常锁屏，请先按 `a`（INPUT 中按 `Ctrl-\ a`）切到 `IDLE:NORMAL`。只读 viewer
不会改变 idle 行为。

## Action palette

termway 不内建任何 niri action。palette 完全由配置文件驱动，每个条目都是一个普通
argv command，因此也可以用于其他 compositor、脚本或任意程序。默认读取：

```text
~/.config/termway/config.toml
```

也可以显式指定：

```console
termway --config /path/to/config.toml view --control
```

完整示例见 [`examples/config.toml`](examples/config.toml)，可这样安装：

```console
mkdir -p ~/.config/termway
cp examples/config.toml ~/.config/termway/config.toml
```

示例根据当前 niri 配置提供 Kitty、Noctalia launcher/clipboard、Dolphin、Chrome，以及
overview、窗口和工作区操作。niri 项也只是普通命令，例如：

```toml
[[actions]]
name = "overview"
description = "Toggle the niri overview"
command = ["niri", "msg", "action", "toggle-overview"]
```

在 NAV 中按 `x`，或在 INPUT 中按 `Ctrl-\ x` 打开。输入字符过滤 name 和
description；Tab/Down 与 BackTab/Up 切换候选，Enter 执行，Esc 或 Ctrl-G 取消。
termway 会为子进程补齐已发现的 `XDG_RUNTIME_DIR`、`WAYLAND_DISPLAY`、
`NIRI_SOCKET`，并从本地图形会话补齐 `DISPLAY`、session D-Bus 等桌面环境变量；因此
从 SSH 启动 Chrome 这类应用时不会错误继承 SSH 会话。配置缺失时 viewer 仍可正常使用，
只会在打开 palette 时给出提示。

滚动目标独立于 NAV/INPUT 和点击安全开关。默认 `SCROLL:VIEW` 保留触控板导航体验；
切到 `SCROLL:DESKTOP` 后，termway 会将滚动位置映射到远端 output，再通过 wlr virtual
pointer 发送水平或垂直 wheel axis。滚动仍使用事件合并、手势轴锁和每帧限幅。
原生 damage watcher 可用时，键盘输入和点击产生的画面变化由 compositor 自动报告，
不会额外发起前台 capture。回退到 grim 或 watcher 运行失败时，键盘输入会启用 250ms
debounce，点击后也会主动捕获，维持相同的基本交互能力。显式 `Ctrl-\ r` 始终强制立即
捕获当前画面，作为手动刷新和恢复手段。

底部采用 Emacs 式两层信息区：mode line 持续显示当前模式、输出、倍率和 viewport；
echo area 显示刷新结果、点击坐标和错误。普通消息 2 秒后自动清空，错误保留 5 秒，
不会覆盖 mode line 中的控制状态。

高频滚动采用短窗口事件合并，只为一批输入重绘一帧；一次连续手势会锁定最初的
主导轴，过滤触控板的交叉轴噪声。持续的交叉轴输入可以突破锁定，短暂停顿也会自动
解锁，因此可以直接改变滑动方向。每帧位移有限幅，避免快速滑动造成事件排队或
viewport 突然越过过大范围。
