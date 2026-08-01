# ADR-0001：采用 SSH-native 架构

- 状态：接受
- 日期：2026-08-01

## 背景

使用者从受管控的 macOS 电脑通过 SSH 和 tmux 访问家里的 NixOS/niri。macOS 不能安装远程桌面客户端，更不能授予读取输入设备等高权限。

现有 Waytermirror 采用单独 client/server 和裸 TCP streams。其 client 直接使用 libinput 获取本地键盘鼠标，因此不能等同于在远端 tmux pane 中读取 PTY 输入。

## 决策

termway 本体运行在远端 NixOS 主机。它只使用当前 PTY 的 stdin/stdout 与用户交互，不定义跨网络应用协议，也不需要 macOS 二进制。

需要特权的输入注入隔离到远端本机的最小 broker；画面捕获和 niri 状态均来自远端 graphical session。

## 后果

正面：

- macOS 零安装；
- 直接兼容 SSH 的认证、加密和审计；
- 能自然运行于 tmux；
- 没有额外开放端口。

代价：

- 受终端协议表达能力限制；
- 无法获得原生 client 那样完整的按键按下/释放信息；
- Kitty/Sixel 等增强能力依赖终端与 tmux；
- 音频和高帧率视频不属于适合的目标。
