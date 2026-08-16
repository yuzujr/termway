# 测试方法

termway 的图形路径不能只靠检查 escape sequence，也不能把所有肉眼验收留给使用者。当前分三层验证：

1. `cargo test` 检查 viewport/坐标映射、tile diff、输出队列、Kitty image 生命周期，以及
   placement、delete、同步更新的协议顺序；导航策略测试还要求 stale atlas 只能直接 refine
   当前帧，不能进入 preview。
2. `scripts/visual-regression.sh` 在真实 Kitty 和真实 Kitty+tmux 中启动确定性四色 fixture，
   用 `grim` 连续截图并由 ImageMagick 取样。缩放前必须是洋红色 refined tile，缩放后必须是
   红色 atlas crop；过渡中的每一张截图只能属于这两种完整状态。四色原图、背景、上下分裂或
   条带都会直接令测试失败。stale-atlas 用例会通过生产 `draw_kitty` 管线把高细节初始 atlas
   标记为过期、显示纯色当前帧，再在 direct/tmux 中缩放；状态栏不得显示 loading，连续
   截图也不得出现旧 atlas 像素。最后的质量用例先验证 atlas 上传期间的 viewport 控制能在
   1 秒内更新状态栏，再从高缩放回到 1×，比较 atlas 阶段与 refine 截止后的图像区域；
   后者只允许保持一致，不能变糊。
3. 发布前运行 release build、Clippy、Nix flake check，再用真实 `termway view` 做输入延迟和
   compositor 集成 smoke test。视觉测试产物保存在 `target/visual-regression/`，可以直接检查
   `direct-montage.png`、`tmux-montage.png` 与 `stale-*-montage.png`。

在已解锁的 niri 图形会话内运行；脚本检测到 systemd-logind 的图形 session 仍锁定时会直接
退出，避免把覆盖 Kitty 的锁屏误报成渲染失败：

```console
nix develop -c scripts/visual-regression.sh
```

该测试会依次短暂打开多个全屏 Kitty 窗口，完成后自动关闭并恢复原先聚焦的窗口。fixture 的协议
分段被故意延迟 20ms，因此非原子的实现会被稳定放大并捕获，不依赖截图恰好撞上短暂坏帧。
stale-atlas 测试把 fixture 的 preview 窗口延长到 750ms、atlas refresh 延长到 10 秒，使错误
atlas 可以被稳定捕获且不会在断言前变成合法的新 atlas；质量
测试走真实 tmux pacing，并确定性地把候选 refine 设为 360p，避免测试是否覆盖退化分支取决
于某一台机器上的 PNG 压缩率。
