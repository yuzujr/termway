# Testing

termway's graphics path cannot be validated by inspecting escape sequences
alone, and it should not push all visual acceptance onto users. Validation is
layered three ways:

1. `cargo test` covers viewport/coordinate mapping, tile diff, the output
   queue, Kitty image lifecycle, and the protocol order of placement, delete
   and synchronized updates; the navigation-strategy tests also require a
   stale atlas to refine the current frame directly without entering preview.
2. `scripts/visual-regression.sh` runs a deterministic four-color fixture in a
   real Kitty and a real Kitty+tmux, screenshots continuously with `grim` and
   samples the frames with ImageMagick. Before zooming the image must be a
   magenta refined tile; after zooming it must be a red atlas crop; every
   screenshot during the transition must be one of these two complete states.
   A four-color source, a background, a vertical split or banding fails the
   test directly. The stale-atlas case marks a high-detail initial atlas stale
   through the production `draw_kitty` pipeline, displays a solid-color current
   frame, then zooms in direct/tmux; the status line must not show loading and
   no screenshot may contain stale-atlas pixels. The final quality case first
   verifies that viewport control during an atlas upload updates the status
   line within 1 s, then zooms back from a high zoom to 1× and compares the
   atlas phase with the post-refine image region; the latter may only stay
   identical, never get blurrier.
3. Before release: run the release build, Clippy and `nix flake check`, then a
   real `termway view` input-latency and compositor-integration smoke test.
   Visual artifacts land in `target/visual-regression/`; check
   `direct-montage.png`, `tmux-montage.png` and `stale-*-montage.png`.

Run inside an unlocked niri graphical session; the script exits if
systemd-logind still reports the graphical session locked, so a lock screen
covering Kitty is not misreported as a render failure:

```console
nix develop -c scripts/visual-regression.sh
```

The test opens several fullscreen Kitty windows in sequence and restores the
previously focused window afterwards. The fixture's protocol chunks are
deliberately delayed by 20 ms so a non-atomic implementation is reliably
amplified and captured rather than depending on a screenshot accidentally
hitting a transient bad frame. The stale-atlas test extends the fixture's
preview window to 750 ms and the atlas refresh to 10 s so a wrong atlas is
stably captured and cannot become a legitimate new atlas before the assertion;
the quality test runs through real tmux pacing and deterministically pins the
candidate refine to 360p so whether a degraded branch is covered does not
depend on the PNG compression ratio on any particular machine.
