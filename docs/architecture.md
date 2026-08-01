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
              └─ input.sock
                  └─ termway-input-broker    家中 NixOS 服务
                      └─ /dev/uinput
```

远程边界只有 SSH。`termway` 和 input broker 之间使用本机 Unix socket，不监听 TCP。

## 模块边界

```text
src/
  app/          状态机、模式切换、退出与错误恢复
  terminal/     TTY 生命周期、能力探测、输入解析
  niri/         JSON IPC、event stream、窗口选择
  capture/      grim spike 与正式 wlr-screencopy backend
  render/       half-block、Kitty Graphics、viewport
  input/        坐标映射、key translation、broker protocol
  broker/       uinput 设备生命周期和安全策略
```

模块之间传递带单位的坐标类型：`PhysicalPx`、`LogicalPx`、`TerminalCell`，避免裸 `(i32, i32)`。

## 运行模式

### Window picker

默认入口。通过 niri event stream 展示窗口标题、app id、workspace 和 output。选择后调用 niri action 聚焦窗口。

### View mode

显示目标 output 或 viewport。默认 5 FPS，只有 damage 或用户输入时才优先刷新；允许临时提高 FPS。

### Control mode

显式按键进入后才转发普通输入，明显显示控制状态。固定 escape chord 必须永远由 termway 自己处理。退出、SSH 断开或 panic 时 broker 必须释放全部按键和按钮。

## tmux 语义

- termway 是普通前台程序，不修改 tmux server；
- pane detach 后暂停输出或降至零 FPS，attach/resize 后请求关键帧；
- 监听 `SIGWINCH`，重建 viewport；
- half-block renderer 不需要 tmux passthrough；
- Kitty renderer 只有探测成功后才启用；
- 不接管 tmux prefix，控制模式的 escape chord 默认避开 `C-b`。

## 安全边界

- 不监听公网或局域网端口；
- SSH 负责认证和加密；
- broker socket 位于用户 runtime directory，权限 `0600`；
- broker 使用 `SO_PEERCRED` 验证调用者 UID；
- 不读取任何真实 input device；
- 默认只创建项目需要的有限 virtual device capabilities；
- 日志禁止记录文本输入、paste 内容和原始按键流；
- 支持 `--view-only`，完全不连接 input broker。
