# 测试方法

termway 的图形路径不能只靠检查 escape sequence，也不能把所有肉眼验收留给使用者。当前分三层验证：

1. `cargo test` 检查 viewport/坐标映射、tile diff、输出队列、Kitty image 生命周期，以及
   placement、delete、同步更新的协议顺序。
2. `scripts/visual-regression.sh` 在真实 Kitty 和真实 Kitty+tmux 中启动确定性四色 fixture，
   用 `grim` 连续截图并由 ImageMagick 取样。缩放前必须是洋红色 refined tile，缩放后必须是
   红色 atlas crop；过渡中的每一张截图只能属于这两种完整状态。四色原图、背景、上下分裂或
   条带都会直接令测试失败。
3. 发布前运行 release build、Clippy、Nix flake check，再用真实 `termway view` 做输入延迟和
   compositor 集成 smoke test。视觉测试产物保存在 `target/visual-regression/`，可以直接检查
   `direct-montage.png` 与 `tmux-montage.png`。

在 niri 图形会话内运行：

```console
nix develop -c scripts/visual-regression.sh
```

该测试会短暂打开两个全屏 Kitty 窗口，完成后自动关闭并恢复原先聚焦的窗口。fixture 的协议
分段被故意延迟 20ms，因此非原子的实现会被稳定放大并捕获，不依赖截图恰好撞上短暂坏帧。
