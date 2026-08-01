# 技术验证计划

每个 spike 必须能独立失败；失败时记录结论并重新选型，不把未知风险带进主体实现。

## Spike 0：环境探测（已实现）

目标：从普通 SSH login 和已有 tmux session 中发现活动 niri session。

验收：

- 找到并连接 `$NIRI_SOCKET`；
- 获取 outputs、windows、focused window 和 event stream；
- 找到正确 Wayland socket；
- 在多个 SSH/tmux attach 场景中不给环境变量写死具体 socket 名。

实现会依次检查命令行覆盖、当前进程环境、systemd user environment 和 runtime directory。扫描得到多个活动 niri session 时拒绝猜测，要求显式传入 `--niri-socket`。

## Spike 1：只看画面（已完成）

目标：`grim` 截图经缩放后，以 truecolor half-block 输出到当前 PTY。

验收：

- macOS 端不安装 termway client；
- Terminal/Kitty → SSH → tmux 链路显示正确；
- 正确处理 1.25 scale 和终端 resize；
- CC Switch 的 profile 名在局部 zoom 下可读；
- 记录单帧耗时、字节数和 CPU。

当前实现：

- 自动从 niri focused output 选择目标，也支持 `--output` 覆盖；
- 为 `grim` 补齐 SSH session 中缺失的 `WAYLAND_DISPLAY`；
- 直接解析 stdout 中的 P6 PPM，不创建持久截图文件；
- 使用 triangle filter 等比缩放；
- 每个 `▀` 字符以前景色承载上像素、背景色承载下像素；
- 自动读取终端尺寸，也支持 `--cols`/`--rows`；
- 支持 `--zoom` 与归一化的 `--center-x`/`--center-y` viewport；
- 图像与 metrics 分别写入 stdout/stderr。
- 提供 alternate-screen 交互 viewer，支持即时 zoom、pan、resize 和手动刷新；
- 所有正常错误与退出路径均恢复 raw mode、光标和 line wrap。

2026-08-01 在当前 eDP-1（2560×1600、scale 1.25）上的 release 实测：

- grim 捕获约 30–32 ms；
- 115×36 cells 渲染约 9 ms；
- 单帧 ANSI 约 170 KB；
- 80×24 tmux pane 中实际图像宽 73 cells，没有横向换行。

单独捕获 focused window 的路径也做了验证。niri 的 IPC window ID 与 foreign-toplevel identifier 可以对应，但当前 niri 26.04 没有实现 grim `-T` 所需的 `ext-image-copy-capture` 窗口捕获协议。因此当前版本使用 output viewport zoom，不依赖不稳定的窗口绝对坐标。未来 compositor 支持该协议时可以重新启用单窗口捕获。

macOS SSH 实测确认：half-block 在 5× 以上配合合适 viewport 可以读清 CC Switch 文字。因此该 renderer 定位为不支持 Kitty Graphics 时的兼容回退：1× 用于全景定位，5×–9× 用于阅读和操作。连续刷新和 damage tracking 属于 Spike 4。

## Spike 2：终端输入

目标：只解析并可视化终端事件，不注入桌面。

验收：

- 字符、方向键、组合键、bracketed paste；
- SGR mouse 的移动、按下、释放和滚轮；
- tmux 内外行为一致；
- SSH 中断后恢复终端模式和光标。

进展：viewer 已覆盖普通字符、方向键、组合退出键和 resize，并通过真实 tmux send-keys 测试。bracketed paste、SGR mouse 和完整输入可视化仍待实现。

## Spike 3：家中侧输入注入

目标：使用 ydotool 验证 niri 下的键盘、点击与坐标映射。

验收：

- 可以点击 CC Switch 中的目标 profile；
- 可以输入 ASCII 与中文 paste；
- 终端 cell 到 1.25-scale output 的映射正确；
- 任意退出路径不会遗留按下状态。

## Spike 4：原生连续捕获（进行中）

目标：用 wlr-screencopy 替换 `grim`。

已完成的基础路径：

- 直接连接 SSH session 发现的 Wayland socket；
- 选择 niri 的目标 `wl_output`；
- 通过 `zwlr_screencopy_manager_v1` 捕获完整 output；
- 使用 memfd-backed `wl_shm` buffer，并在尺寸和格式不变时跨帧复用；
- 处理 stride、XRGB/ARGB/XBGR/ABGR 和 Y-invert；
- 原生 backend 不可用或运行中失败时自动回退到 `grim`。

当前 niri 26.04 实际暴露 wlr-screencopy v3，但没有暴露
ext-image-copy-capture。termway 首版 client 绑定兼容的 screencopy v1，以获得有保证的
`wl_shm` 格式协商；后续升级到 v3 的 `copy_with_damage`。

2026-08-01 同一 eDP-1 上的 release 单帧命令实测为 17.7–18.7 ms（包含新建 Wayland
连接），相比 warm grim 的约 30–32 ms 已有下降。viewer 会继续复用连接和 buffer。

验收：

- 复用 buffers；（已完成）
- 利用 damage 或等价机制避免无意义刷新；
- 默认 5 FPS 下交互可用；
- SSH 延迟和带宽受限时自动降帧，不堆积旧帧。

## Spike 5：Kitty Graphics

目标：在支持时自动提供高分辨率模式。

验收：

- 直接数据传输，不引用远端文件路径；
- 原生 SSH 与 tmux 两种链路均完成能力探测；
- 不支持或响应超时时无缝降级至 half-block；
- resize、切 pane、detach/attach 后不残留图片。

## MVP 完成定义

在 macOS 的现有终端中 SSH 到家里 NixOS，进入 tmux 后运行 termway，可以：

1. 从 niri 窗口列表中选择 CC Switch；
2. 看清 profile 列表；
3. 使用键盘或终端鼠标切换 profile；
4. 安全退出并回到 shell；
5. macOS 端除 SSH/终端外不安装任何组件。
