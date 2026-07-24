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

### Gap A: PCM 实时流未集成 Source pipeline

| 字段         | 值                                                                                                  |
| ------------ | --------------------------------------------------------------------------------------------------- |
| **设计参考** | `progressive-batch-data-structures.md §5.2-5.4`                                                     |
| **现象**     | `ProgressiveBatchManager.on_audio_frame` 已实现，但未连接到 live recording 的 Source actor pipeline |
| **当前行为** | Progressive Batch 仅通过 `run_progressive_batch_from_file` 从已有音频文件跑，不用于实时录音         |
| **影响**     | 新录音 + Progressive Batch 模式 → 实际走了 Live 模式（见 Gap B）                                    |
| **修复方向** | SourceArgs 新增 `pcm_tx` 字段，Pipeline dispatch 发送 PCM，Session Supervisor 创建 Manager          |
| **优先级**   | 🟡 中（Gap B 修复后成为必要条件）                                                                   |

### Gap B: `effective_transcription_mode()` 不识别 `ProgressiveBatch` ✅ 已修复

| 字段         | 值                                                                                                                                      |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------------- |
| **模块**     | `crates/listener-core/src/actors/session/types.rs`                                                                                      |
| **现象**     | `stt_mode=progressive` → Rust 端 `TranscriptionMode::ProgressiveBatch` → live session 的 `effective_transcription_mode()` 回退为 `Live` |
| **根因**     | `effective_transcription_mode()` 仅判断 `== Batch` 和 `!= Batch`，`ProgressiveBatch` 落入 `else` → `Live`                               |
| **影响**     | 新录音即使设了 Progressive Batch，仍启动 WebSocket listener                                                                             |
| **修复**     | ✅ **已修复** — Jul 24                                                                                                                  |
| **修复内容** | `effective_transcription_mode()` 增加 `ProgressiveBatch` 分支，返回自身，不落入 `Live`                                                  |
| **优先级**   | 🔴 高 → ✅                                                                                                                              |

### Gap F: ProgressiveBatch 短音频未使用 Direct Batch 编码方式 ✅ 已修复

| 字段         | 值                                                                                                                                                                                                           |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **模块**     | `crates/listener2-core/src/batch/progressive_batch/integration.rs`                                                                                                                                           |
| **现象**     | Re-transcript 短音频（< 3min）时：① `hound::WavReader` 无法读 MP3 文件 → `"Ill-formed WAVE file: no RIFF tag found"`；② PCM f32 → s16le 重编码后 POST `audio/pcm`，与 Direct Batch 的"原文件字节 POST"不一致 |
| **根因**     | ProgressiveBatch 路径未复用 Direct Batch 的 `streaming_file_part` 方式                                                                                                                                       |
| **影响**     | MP3 文件无法转录；多了一道无意义的编码/解码 roundtrip                                                                                                                                                        |
| **修复**     | ✅ **已修复** — Jul 24                                                                                                                                                                                       |
| **修复内容** | `run_progressive_batch_from_file`：短音频直接 `tokio::fs::read` 原文件字节 + `mime_type_for_extension` 判 MIME → POST。长音频保留 PCM 解码（分段需要）。见 `docs/progressive-batch-hybrid-design.md` 附录 B  |
| **优先级**   | 🔴 高 → ✅                                                                                                                                                                                                   |

### Gap G: ProgressiveBatch 长音频分段 WAV 编码未对齐 ✅ 已修复

| 字段       | 值                                                                                                                                                    |
| ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| **模块**   | `crates/listener2-core/src/batch/progressive_batch/queue.rs`                                                                                          |
| **现象**   | `submit_segment_http` 之前用 `audio/wav` POST（写临时 WAV 文件再读取），与短音频的原始文件字节 POST 不一致                                            |
| **修复**   | **Jul 25 — PCM direct POST**：PCM f32 样本在内存中重采样至 16000 Hz + 编码为 s16le，以 `audio/pcm` Content-Type 直接 POST。不写临时文件，不封装 WAV。 |
| **收益**   | 消除磁盘 I/O（大音频时明显）；POST 格式对齐 Speaches batch 端点原生 PCM 输入能力；内存峰值 ~15MB（原 ~4GB）                                           |
| **优先级** | 🔴 高 → ✅                                                                                                                                            |

### Potential Concern: `min_duration_secs` 与 `segment_duration_ms` 逻辑重叠

| 字段       | 值                                                                                                                                                                                            |
| ---------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **模块**   | `crates/listener2-core/src/batch/progressive_batch/mod.rs` + `integration.rs`                                                                                                                 |
| **现象**   | `ProgressiveBatchConfig` 保留了 `min_duration_secs` 字段（默认 180），但 `integration.rs:44` 实际判断硬编码为 `duration_secs < segment_duration_ms as f64 / 1000.0`，忽略 `min_duration_secs` |
| **根因**   | 两个独立概念被合并：阈值恒等于一段音频的时长（≥1 段才需要分段）                                                                                                                               |
| **影响**   | ① `min_duration_secs` 字段是死代码（当前由段长决定）；② 用户无法独立控制阈值（例如设 60s 段长 + 180s 阈值）                                                                                   |
| **方向**   | 后续可删除 `min_duration_secs` 字段，或恢复独立的阈值语义（需讨论）：分段起始阈值应否独立于段长？                                                                                             |
| **优先级** | 🟡 中（待讨论）                                                                                                                                                                               |

### Gap C: 前端 progress 事件未实现

| 字段         | 值                                                                                   |
| ------------ | ------------------------------------------------------------------------------------ |
| **设计参考** | `progressive-batch-data-structures.md §6 v2`                                         |
| **当前状态** | `QueueProgress` 数据结构就绪，Manager.progress() 可调用，但前端无对应事件和进度条 UI |
| **优先级**   | 🟢 低                                                                                |

### Gap D: segment_overlap_ms / max_retries 未配置化

| 字段         | 值                                                                                                        |
| ------------ | --------------------------------------------------------------------------------------------------------- |
| **设计参考** | `progressive-batch-hybrid-design.md §7`                                                                   |
| **当前状态** | `segment_overlap_ms = 1000`、`max_retries = 3` 硬编码在 `ProgressiveBatchConfig::default()` 中，前端无 UI |
| **优先级**   | 🟢 低                                                                                                     |

### Gap E: v2 持久化表未创建

| 字段         | 值                                                                            |
| ------------ | ----------------------------------------------------------------------------- |
| **设计参考** | `progressive-batch-data-structures.md §3 v2`                                  |
| **当前状态** | 无 `progressive_batch_jobs` / `progressive_batch_segments` 表，段状态全在内存 |
| **优先级**   | 🟢 低                                                                         |

---

## 验证计划

### 短期（当前 sprint）✅ 已完成

1. ✅ 编译并测试新录音 + 三种模式（Live / Batch / Progressive Batch）
2. ✅ 检查 Console `[DEBUG]` 日志确认路由
3. ✅ 检查 Rust `[DEBUG]` 日志确认 batch 执行
4. ✅ 验证 Bug 3 根因
5. ✅ 修复 Gap B：`effective_transcription_mode()` 识别 `ProgressiveBatch`
6. ✅ 修复 Gap F：短音频 POST 原文件字节（对齐 Direct Batch）
7. ✅ Speaches 编码附录 + Direct Batch 对比表

### 中期

1. 🔄 修复 Gap A：PCM 流集成（Source pipeline → ProgressiveBatchManager）
2. ⏳ 端到端测试实时录音走 Progressive Batch（依赖 Gap A）

### 长期

1. Gap C：前端 progress 事件
2. Gap D：用户可配置参数
3. Gap E：v2 持久化表
