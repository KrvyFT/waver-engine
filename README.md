# waver-engine

cpal 输出流、音频线程上的 `Engine`（按 `Schedule` 跑 `Process`），以及从 SPSC 队列 drain `RtCommand`。

本仓库是 [waver](https://github.com/KrvyFT/waver) workspace 的一部分，在伞仓中位于 `crates/waver-engine`（git submodule）。

## 仓库

- GitHub：https://github.com/KrvyFT/waver-engine
- 默认分支：`main`
- License：MIT OR Apache-2.0
- 依赖：[`waver-core`](https://github.com/KrvyFT/waver-core)、[`waver-dsp`](https://github.com/KrvyFT/waver-dsp)、`cpal`、`rtrb`

## 在伞仓里开发（推荐）

```bash
git clone --recurse-submodules https://github.com/KrvyFT/waver.git
cd waver/crates/waver-engine
git add -A && git commit -m "…" && git push
cd ../..
./scripts/repos.sh sync
git commit -m "chore: bump waver-engine" && git push
```

在伞仓根目录：`cargo test -p waver-engine`。

## 单独 clone

```bash
git clone https://github.com/KrvyFT/waver-engine.git
```

可独立推送；构建请走伞仓。见 [doc/repos.md](https://github.com/KrvyFT/waver/blob/master/doc/repos.md)。

## 内容概要

| 项 | 说明 |
|----|------|
| `Engine` / `BLOCK` | 内部 64 帧块；与设备回调长度解耦 |
| `spawn_output` | 建流、命令环、原子 `EngineStatus` |
| `SwapSchedule` | rebuild 时 `for_kind` → `Box<dyn Process>` |
| 主输出 | 取最后一个 `Output` 的 `master_slice` |

详见 [doc/audio-thread.md](https://github.com/KrvyFT/waver/blob/master/doc/audio-thread.md)。
