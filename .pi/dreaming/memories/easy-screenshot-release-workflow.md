---
slug: "easy-screenshot-release-workflow"
name: "Easy Screenshot release workflow"
description: "The streamlined README documents the standard release build, binary location, and KDE permission workflow."
kind: "project"
confidence: 0.99
sensitivity: "normal"
tags: ["easy-screenshot","rust","release","documentation","kde"]
createdAt: "2026-09-25T09:35:13.243Z"
updatedAt: "2026-09-25T09:35:13.243Z"
lastSeenAt: "2026-09-25T09:35:13.243Z"
sources: [{"sessionKey":"/home/dianqwq/.pi/agent/sessions/--tummy-projects-easy-screenshot--/2026-09-25T09-12-29-466Z_01a0d7d6-6e19-7713-b47a-c8621e9290a5.jsonl","signature":"0f228cdaca7f0585b1838ca9","messageCount":9,"excerpt":"user: 这个程序启动特别慢，卡顿 请想办法解决 assistant tool_call: bash {\"command\":\"ls -la && find . -maxdepth 2 -type f | head -100\",\"timeout\":10000} assistant tool_call: read {\"path\":\"Cargo.toml\",\"offset\":1,\"limit\":100} assistant tool_call: read {\"path\":\"README.md\",\"offset\":1,\"limit\":100} assistant tool_call: read {\"path\":\"src/main.rs\",\"offset\":1,\"limit\":200} assistant tool_call: read {\"path\":\"src/portal.rs\",\"offset\":1,\"limit\":200} assistant tool_call: read {\"path\":\"src/overlay.rs\",\"offset\":1,\"limit\":250} assista","capturedAt":"2026-09-25T09:34:49.999Z"}]
---

# Easy Screenshot release workflow

Commit `ad593f7` streamlines `README.md` around usage, options, dependencies, KDE authorization, troubleshooting, and module layout.

Build and verify the standard release artifact with:

```bash
cargo build --release
./target/release/easy-screenshot --version
```

The output is `target/release/easy-screenshot`. On KDE, `--install-kwin-permission` should be run using the same absolute binary path that will launch the program; repeat it if the binary is moved. For diagnosing remaining capture-stage latency, use this permission workflow together with `--verbose` and [[easy-screenshot-performance]].
