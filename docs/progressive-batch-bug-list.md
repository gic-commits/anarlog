# Progressive Batch — Bug & Gap 跟踪

## 活跃 Bug

### Bug 1: `getLiveTranscriptionConfig` 忽略 `stt_mode`

| 字段         | 值                                                                                                                                                                   |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **模块**     | `apps/desktop/src/stt/capabilities.ts:185-227`                                                                                                                       |
| **现象**     | 新录音无视 `stt_mode` 配置，始终走 Live 模式                                                                                                                         |
| **根因**     | `getLiveTranscriptionConfig` 不读取 `stt_mode` 参数，`modeFromConfig` 始终为 `undefined`                                                                             |
| **影响**     | 用户设定了 Batch / Progressive Batch，新录音仍走 Live WebSocket                                                                                                      |
| **修复**     | ✅ **已修复** — Jul 24                                                                                                                                               |
| **修复内容** | 添加 `sttMode` 参数，映射为 `TranscriptionMode`（`"live"`→`"live"`，`"batch"`→`"batch"`，`"progressive"`→`"progressiveBatch"`）                                      |
| **验证结果** | ✅ 日志确认：`getLiveTranscriptionConfig: sttMode=batch modeFromConfig=batch` → Rust `transcription_mode=Some(Batch)` → `default_transcription_mode: explicit Batch` |
| **时间**     | Jul 24 (已验证)                                                                                                                                                      |

### Bug 2: Re-transcription 可能走错 batch target

| 字段         | 值                                                                                                                                           |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------- |
| **模块**     | `apps/desktop/src/stt/useRunBatch.ts:356-362`                                                                                                |
| **现象**     | 模式选 Batch 时 re-transcribe，`progressiveBatch` 可能为 `true`（走 Progressive Batch）                                                      |
| **根因**     | `shouldUseProgressiveBatch` 对 Speaches OpenAI（非 openai.com URL）返回 `true`，但用户期望 Batch 模式走普通 Batch                            |
| **影响**     | Batch 模式的 re-transcribe 走了 Progressive Batch 路径                                                                                       |
| **修复**     | ✅ **已修复**                                                                                                                                |
| **修复内容** | `sttMode === "batch"` → `progressiveBatch = false`，`sttMode === "live"` → 保留 `shouldUseProgressiveBatch` fallback（Speaches 返回 `true`） |
| **验证方式** | Console 日志出现 `[DEBUG] useRunBatch params: sttMode=batch progressiveBatch=false ...`                                                      |
| **时间**     | Jul 24                                                                                                                                       |

### Bug 3: Re-transcription (`startTranscription`) 转圈消失无结果

| 字段         | 值                                                                                                                                                      |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **模块**     | `plugins/transcription/src/listener2/ext.rs:22-158` + `general-batch.ts:83-291`                                                                         |
| **现象**     | 点击 Re-transcribe → 转圈闪一下 → 消失 → 转录结果不变 → 无错误提示                                                                                      |
| **根因**     | 未复现。首次编译后测试时 JS 热更新未生效，`getLiveTranscriptionConfig` 未传 `sttMode` → `transcriptionMode=undefined` → 状态机异常                      |
| **优先级**   | ✅ **已修复**                                                                                                                                           |
| **修复**     | 重新编译后 JS 热更新生效，Batch 模式完整走通：`startTranscription → ok → BatchStarted → batch transcription completed → BatchResponse → BatchCompleted` |
| **验证结果** | ✅ 35.9s 音频正确转录为 "读标知识读标人财务情况读标设备销售业绩"，31 个 segments 全部完成                                                               |
| **时间**     | Jul 24 (已验证)                                                                                                                                         |

---

## 设计-实现差异（Gaps）

### Gap A: PCM 实时流未集成 Source pipeline — Sprint 2 Phase A 🎯

| 字段         | 值                                                                                                  |
| ------------ | --------------------------------------------------------------------------------------------------- |
| **设计参考** | `progressive-batch-data-structures.md §5.2-5.4`                                                     |
| **现象**     | `ProgressiveBatchManager.on_audio_frame` 已实现，但未连接到 live recording 的 Source actor pipeline |
| **当前行为** | Progressive Batch 仅通过 `run_progressive_batch_from_file` 从已有音频文件跑，不用于实时录音         |
| **影响**     | 前端增量展示组件已就绪但无数据源 — 无法演示                                                        |
| **修复方向** | SourceArgs 新增 `pcm_tx` 字段，`ListenerRouting` 新变体，Pipeline dispatch，Supervisor 创建 channel |
| **Sprint 2** | Phase A — 必要前提                                                                                  |

### Gap L: Stitcher 不支持 partial stitch — Sprint 2 Phase B 🎯

| 字段         | 值                                                                                                            |
| ------------ | ------------------------------------------------------------------------------------------------------------- |
| **现象**     | `stitch()` 返回 `Err(MissingSegments)`，`finish()` 遇到 any missing 立即失败                                  |
| **影响**     | 无法支持"部分段放弃后仍返回可用结果"                                                                          |
| **修复方向** | `stitch()` 改为永远返回 `Response`，missing 段在 metadata 中记录 `abandoned_segments` + `gap_warnings`        |
| **Sprint 2** | Phase B — 重试协议的基础                                                                                      |

### Gap M: `drain()` 无超时 — Sprint 2 Phase B 🎯

| 字段         | 值                                                                            |
| ------------ | ----------------------------------------------------------------------------- |
| **现象**     | `drain()` 无限等待，无 timeout                                                |
| **影响**     | Session 结束时 hang 住                                                        |
| **修复方向** | 加 `drain(timeout)`，超时 = `segment_duration_ms * 1.5`                       |
| **Sprint 2** | Phase B                                                                       |

### Gap N: `finish()` 对 partial 结果太苛刻 — Sprint 2 Phase B 🎯

| 字段         | 值                                                                                          |
| ------------ | ------------------------------------------------------------------------------------------- |
| **现象**     | `finish()` line 382-391：drain 后有 failed 段立即返回 Err，不给 partial 结果                |
| **影响**     | 即使只有一个段失败，整个 session 的转写结果全部丢弃                                          |
| **修复方向** | 允许 partial：只有完全无结果时才 Err，否则返回带 `abandoned_segments` 标记的 Response        |
| **Sprint 2** | Phase B                                                                                     |

### Gap O: v2 持久化表未创建 + Manager 不恢复 — Sprint 2 Phase C 🎯

| 字段         | 值                                                                                                |
| ------------ | ------------------------------------------------------------------------------------------------- |
| **设计参考** | `progressive-batch-data-structures.md §3 v2` + §7.5                                               |
| **当前状态** | 无 `progressive_batch_jobs` / `progressive_batch_segments` 表，Manager 全内存，重启即失           |
| **修复方向** | DB 表 + Drizzle schema + `TauriBatchRuntime` 写 DB + `Manager::resume()` + `continue_from_file()` |
| **Sprint 2** | Phase C                                                                                           |

### Gap P: UI 右键菜单未区分转写模式 — Sprint 2 Phase D 🎯

| 字段         | 值                                                                                     |
| ------------ | -------------------------------------------------------------------------------------- |
| **现象**     | 右键只有 "Re-transcribe"，不区分 Total / Progressive / Continue                        |
| **影响**     | 用户无法触发 partial job 的 Continue                                                   |
| **修复方向** | 菜单裂为三项 + Continue 条件显示                                                       |
| **Sprint 2** | Phase D                                                                                |

### ✅ 已修复 Gaps

| Gap | 内容 | 修复时间 |
|-----|------|----------|
| B | `effective_transcription_mode()` 不识别 `ProgressiveBatch` | Jul 24 |
| F | 短音频未使用 Direct Batch 编码方式 | Jul 24 |
| G | 长音频分段 WAV 编码未对齐 | Jul 25 |
| H | `submit_segment_http` 未复用 shared methods | Jul 25 |
| I | `ProgressiveBatchManager` Runtime 事件贯通 | Jul 26 |
| J | Stitcher `segment_boundaries` 元数据 | Jul 26 |
| K | 前端增量展示 + 分段分隔 UI | Jul 26 |

### 长期 Gaps（Sprint 3+）

| Gap | 内容 | 优先级 |
|-----|------|--------|
| D | segment_overlap_ms / max_retries 配置化 | 🟢 低 |
| C | 前端 progress 事件（已由增量展示替代） | 🟢 已关闭 |

---

## 验证计划

### Sprint 1（Jul 24-26）✅ 已完成

- ✅ 编译 + 三种模式路由
- ✅ Bug 1/2/3 修复验证
- ✅ Gap B/F/G/H/I/J/K 全部修复
- ✅ `cargo test -p listener2-core` 109/109 ✅
- ✅ `cargo test -p openai-transcription` 17/17 ✅
- ✅ `cargo check` + `dprint fmt`
- ✅ `pnpm -F @hypr/desktop typecheck`

### Sprint 2（本轮）

| Phase | 验证标准 |
|-------|----------|
| A | live 录音 + Progressive Batch 模式 → 前端逐段看到转写文字（`SegmentPreview`）|
| A | `cargo check` + `cargo test -p listener2-core` + `pnpm typecheck` 全部通过 |
| B | 段超时后仍产出结果 + `abandoned_segments` 元数据 |
| B | `cargo test` 新增 timeout/partial stitch 测试 |
| C | 重启后 Continue → 只提未完成段 |
| C | `cargo test` DB roundtrip |
| D | 右键菜单正确显示三个选项 + Continue 条件激活 |
