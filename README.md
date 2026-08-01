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

Spike 1 已实现单帧捕获和 truecolor half-block 输出：

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
- `q`、Esc、Ctrl-C、Ctrl-D：退出。
