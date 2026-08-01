# 技术选型

状态：已接受，用于第一轮技术验证。

## 结论

| 领域 | 选择 | 暂不选择 |
| --- | --- | --- |
| 实现语言 | Rust stable edition 2024 | C++、Go |
| 远程传输 | 现有 SSH PTY 的 stdin/stdout | 自定义 TCP、WebSocket、QUIC |
| 终端控制 | `crossterm`，必要处直接写 ANSI | 完整依赖 ratatui 的渲染模型 |
| 基础图像输出 | 24-bit ANSI + `▀` 半块字符 | ASCII 作为默认模式 |
| 增强图像输出 | Kitty Graphics direct transmission | Sixel 作为第一阶段必需能力 |
| niri 集成 | `$NIRI_SOCKET` 上的 JSON IPC | 绑定版本强耦合的 `niri-ipc` crate |
| 捕获验证 | `grim` 子进程输出 PPM/PNG | 一开始就实现全部 Wayland 协议 |
| 正式捕获 | `wayland-client` + `wayland-protocols-wlr` screencopy | PipeWire/portal 作为 niri 首选路径 |
| 图像缩放 | MVP 使用 `image`，性能验证后考虑 `fast_image_resize` | 自研 SIMD 作为早期工作 |
| 输入验证 | `ydotool`/`ydotoold` | 依赖 niri 提供不存在的点击 IPC |
| 正式输入 | 独立的最小权限 Unix-socket broker + `evdev` uinput | 让 SSH shell 直接获得整个 `input` 组权限 |
| 异步模型 | 主线程终端事件循环；捕获与 broker I/O 分离任务 | 所有 Wayland、TTY、编码工作塞进单线程 |
| 构建与开发 | Cargo + Nix flake | 仅依赖开发机全局工具链 |

## 为什么选择 Rust

这个程序同时处理不可信终端输入、Wayland buffer、像素计算、Unix socket 和 Linux input event。Rust 能减少帧缓冲区和协议解析中的内存错误，同时其 Wayland 与终端库足够成熟。项目目标又只包含一个 Linux 端二进制/服务组合，因此交叉编译到 macOS 不是必要条件。

## 为什么不使用独立 client/server 网络协议

termway 在 SSH 登录后的远端主机上运行。它从 PTY 读取按键和终端鼠标序列，将 ANSI 或图像 escape sequence 写回同一个 PTY。认证、加密、端口转发、连接保活和访问控制继续由 SSH 负责。

这直接满足“公司 Mac 不安装软件”的约束，也避免复制一套不完整的远程访问安全协议。

## 终端渲染策略

### 必须可用：truecolor half-block

字符 `▀` 的前景色表示上像素，背景色表示下像素。终端大小为 `C × R` 时，可以表达 `C × 2R` 个颜色采样点。

优点：

- 不依赖专用图像协议；
- 可穿过 SSH 和 tmux；
- macOS 常见现代终端均可使用；
- 鼠标坐标与字符网格天然对应。

缺点是分辨率有限。因此第一版必须提供局部缩放和“聚焦窗口”模式，不能只做整个高分辨率桌面的缩略图。

### 增强模式：Kitty Graphics

当前配置表明 macOS 与 NixOS 均使用 Kitty，因此第二优先级支持 Kitty Graphics 的 direct transmission。远程场景不能使用本地文件或共享内存传输；必须将压缩后的像素数据内嵌到 escape sequence 中，并探测终端响应。tmux 下需要单独验证 passthrough 和图片生命周期。

Sixel 可以以后作为第三个 renderer，不进入 MVP 的完成条件。

## niri 集成

niri 官方建议复杂程序直接连接 `$NIRI_SOCKET`。JSON IPC 有兼容性承诺，而 Rust `niri-ipc` crate 跟随 niri 自身版本，不遵循独立稳定 semver。因此采用：

- JSON event stream 维护 output、workspace、window、focus 状态；
- JSON action 聚焦目标窗口；
- serde 数据结构允许未知字段，避免新 niri 字段导致解析失败；
- `niri msg --json` 只作为诊断和 spike 工具，正式实现直接访问 Unix socket。

当前机器是 niri 26.04，并启用了 1.25 fractional scale。捕获像素、niri logical coordinates、终端 cell coordinates 三套坐标必须显式建模，不能混用。

## 屏幕捕获

第一轮用 `grim` 快速回答三个问题：SSH session 能否找到活动 Wayland display、截图延迟是多少、缩放并输出到终端后的可读性如何。

验证成立后改为 `zwlr_screencopy_manager_v1`：

- niri 已支持 wlr-screencopy v3；
- 可以按 output 或 region 捕获；
- 可利用 damage 信息减少无变化帧输出；
- 无需每帧创建子进程。

窗口捕获初期采用“聚焦窗口后捕获其所在 output + viewport/zoom”。不要假设 niri IPC 一定提供足够可靠的窗口物理像素边界；单窗口裁切需要单独验证协议与坐标信息。

## 输入路径

终端侧启用：

- raw mode；
- alternate screen；
- bracketed paste；
- SGR extended mouse mode；
- focus events（可用时）。

程序把字符、功能键、paste 和鼠标 cell 坐标转换成内部事件。Linux 侧输入注入分两步：

1. spike 使用 `ydotoold`，验证 niri 下键盘、绝对/相对鼠标和点击；
2. 正式实现使用小型 broker 创建 uinput virtual keyboard/pointer。

broker 只监听 `$XDG_RUNTIME_DIR/termway/input.sock`，校验 Unix peer credential，仅接受当前用户，限制可创建的设备能力，并提供立即断开/释放所有按键的 failsafe。主 TUI 不直接获得读取真实 `/dev/input/*` 的权限。

portal/libei 后端可以未来加入；它更符合 Wayland 安全模型，但无人值守会话中的授权弹窗和 compositor 支持必须先验证，不能作为 MVP 唯一路径。

## 初始 Rust 依赖候选

依赖在对应 spike 开始时再加入，避免过早锁定：

- `anyhow` / `thiserror`
- `clap`
- `crossterm`
- `serde` / `serde_json`
- `image`
- `wayland-client`
- `wayland-protocols-wlr`
- `evdev`
- `tokio` 或 `calloop`，在原生 screencopy spike 后二选一

不把大型视频编码器、FFmpeg、PipeWire 或 GUI toolkit 放进第一阶段依赖树。

## 已知高风险点

1. SSH 启动的进程如何稳定发现图形 session 的 `NIRI_SOCKET`、`WAYLAND_DISPLAY` 和 `XDG_RUNTIME_DIR`。
2. fractional scaling、output transform 和终端 cell 到桌面坐标的映射。
3. tmux 对 Kitty Graphics 和终端能力查询响应的转发。
4. macOS 终端按键序列无法无损表达所有 Linux keycode，特别是修饰键按下/释放状态。
5. uinput 的授权边界与 stuck key 恢复。
6. 全屏高频 ANSI 更新的带宽和 tmux CPU 消耗。
