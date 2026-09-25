---
slug: "easy-screenshot-performance"
name: "Easy Screenshot performance baseline"
description: "Startup-delay and overlay-redraw optimizations were committed as cd1e492 and validated without recording timing benchmarks."
kind: "project"
confidence: 0.99
sensitivity: "normal"
tags: ["project","rust","wayland","performance","overlay"]
createdAt: "2026-09-25T09:32:26.923Z"
updatedAt: "2026-09-25T09:32:26.923Z"
lastSeenAt: "2026-09-25T09:32:26.923Z"
lastUsedAt: "2026-09-25T09:34:23.549Z"
sources: [{"sessionKey":"/home/dianqwq/.pi/agent/sessions/--tummy-projects-easy-screenshot--/2026-09-25T09-12-29-466Z_01a0d7d6-6e19-7713-b47a-c8621e9290a5.jsonl","signature":"2c53fc460e839deb31a397dd","messageCount":5,"excerpt":"user: 这个程序启动特别慢，卡顿 请想办法解决 assistant tool_call: bash {\"command\":\"ls -la && find . -maxdepth 2 -type f | head -100\",\"timeout\":10000} assistant tool_call: read {\"path\":\"Cargo.toml\",\"offset\":1,\"limit\":100} assistant tool_call: read {\"path\":\"README.md\",\"offset\":1,\"limit\":100} assistant tool_call: read {\"path\":\"src/main.rs\",\"offset\":1,\"limit\":200} assistant tool_call: read {\"path\":\"src/portal.rs\",\"offset\":1,\"limit\":200} assistant tool_call: read {\"path\":\"src/overlay.rs\",\"offset\":1,\"limit\":250} assista","capturedAt":"2026-09-25T09:32:02.985Z"}]
---

# Easy Screenshot performance baseline

Commit `cd1e492` optimizes this Rust/Wayland screenshot application by reducing the initial capture wait from 120 ms to 50 ms, pre-converting frozen-frame images to `ARGB8888`, and replacing per-pixel overlay color conversion with whole-image and selection-region copies during mouse movement.

Validation succeeded with formatting checks, all 32 tests, a release build, and `target/release/easy-screenshot --smoke --verbose`. No startup-latency benchmark was recorded, so further tuning should measure the capture stage separately rather than assume the delay is the remaining bottleneck.

Preserve screenshot correctness while changing rendering: final cropping must use the original Portal/KWin frame, not the visually blurred or dimmed overlay.
