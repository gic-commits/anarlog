# Overview

Tauri desktop note-taking app (`apps/desktop/`) with a web app (`apps/web/`).
Uses pnpm workspaces.
SQLite is the primary data store (schema and migrations in `crates/db-app/`, desktop transport in `plugins/db/`), Zustand is used for UI state, and TipTap powers the editor. Sessions are the core entity — all notes are backed by sessions.

## Commands

- Format: `pnpm exec dprint fmt`
- Typecheck (TS): `pnpm -r typecheck`
- Typecheck (Rust): `cargo check`
- Desktop dev: `pnpm -F @hypr/desktop tauri:dev`
- Web dev: `pnpm -F @hypr/web dev`
- Dev docs: https://docs.anarlog.so

## Guidelines

- Format via dprint after making changes.
- JavaScript/TypeScript formatting runs through `oxfmt` via dprint's exec plugin.
- Run `pnpm -r typecheck` after TypeScript changes, `cargo check` after Rust changes.
- After editing files, run the relevant verification commands before finishing.
- For `apps/desktop/` TypeScript changes, prefer `pnpm -F desktop typecheck` to match CI.
- After edits, run `pnpm exec dprint fmt`.
- Use `useForm` (tanstack-form) and `useQuery`/`useMutation` (tanstack-query) for form/mutation state. Avoid manual state management (e.g. `setError`).
- For `plugins/db` live queries, keep schema creation, migrations, and DB initialization on the Rust side; TypeScript should only consume `execute`/`subscribe` APIs.
- Branch naming: `fix/`, `chore/`, `refactor/` prefixes.

## Code Style

- Avoid creating types/interfaces unless shared. Inline function props.
- Do not write comments unless code is non-obvious. Comments should explain "why", not "what".
- Use `cn` from `@hypr/utils` for conditional classNames. Always pass an array, split by logical grouping.
- Use `motion/react` instead of `framer-motion`.

## CLI TUI Command Architecture

Choose the lightest command structure that fits the workflow.

Use the full reducer/effect/runtime split only when the command has async orchestration, a multi-step workflow, or substantial state transitions that benefit from reducer-style tests.

```
commands/<name>/
  mod.rs        -- Screen impl, Args, run()          [glue]
  app.rs        -- App or screen-local state          [optional]
  action.rs     -- Action enum                        [optional]
  effect.rs     -- Effect enum                        [optional]
  runtime.rs    -- Runtime, RuntimeEvent              [async I/O]
  ui.rs         -- draw(frame, app)                   [rendering]
```

Naming rules:

- Types drop the command prefix: `App`, `Action`, `Effect`, `Runtime`, `RuntimeEvent`
- `app.rs` → `app/mod.rs` with private submodules when state is complex
- `ui.rs` → `ui/mod.rs` with sub-files when rendering is complex
- `action.rs`/`effect.rs` are siblings of `mod.rs` when they exist; do not create them by default for simple list/detail screens
- `app.rs` contains no rendering logic, no API calls, no async code when using the reducer pattern
- Prefer `screen.rs` plus a small local state struct for simple browse/select flows
- Do not add parent-level action/effect translation layers that proxy child workflows through another command's reducer

## Session Context (Jul 2026)

### 终极目标

让 anarlog 通过 OpenAI provider 配置，接入自建 speaches 服务器（`wss://speaches.example.com`），实现**实时语音转录**（WebSocket Realtime API 模式），类似 Deepgram 的实时体验。

Speaches 确认支持的能力：Realtime API (WebSocket)、SSE streaming transcription、OpenAI API 完全兼容。

### 已完成的关键里程碑（禁止改乱）

1. **动态模型列表** - `apps/desktop/src/settings/ai/shared/list-stt.ts` 调用 Rust `fetch_stt_models` 命令，从 `/v1/models` 拉取并过滤 STT 模型，在 `select.tsx` 中用 `useQuery` 展示。用户选择的模型名会保持原样发送。

2. **AudioModel::Custom** - `crates/openai-transcription/src/batch/model.rs` 添加 `Custom(String)` 变体，`FromStr`/`Display` 手动实现。未知模型名透传，不会 fallback 到 `gpt-4o-transcribe-diarize`。

3. **provider 字段贯通** - `CaptureParams` → `SessionParams` → `ListenerArgs` 全部添加 `provider: Option<String>`：
   - `plugins/transcription/src/api.rs` `CaptureParams` + `From<CaptureParams> for SessionParams` 传递
   - `crates/listener-core/src/actors/session/types.rs` `SessionParams`
   - `crates/listener-core/src/actors/listener/mod.rs` `ListenerArgs`
   - `crates/listener-core/src/actors/session/supervisor/children.rs` 构造 `ListenerArgs` 时传入

4. **default_transcription_mode 修复** - `plugins/transcription/src/api.rs:77-93`：
   - `from_url_and_languages` 传 `self.provider.as_deref()` 作为 `provider_hint`
   - Speaches 自定义 URL 正确解析为 `AdapterKind::OpenAI`（而不是回退到 `Deepgram`）
   - OpenAI 适配器 `has_live_mode() == true` 且 `is_supported_languages_live() == true` → 走 Live 模式
   - 测试 `31/31 通过`

5. **should_stop_on_listener_failure** - `crates/listener-core/src/actors/session/supervisor.rs:383` 同样传 `provider_hint`

### 关键文件

- `plugins/transcription/src/api.rs` - `CaptureParams`, `default_transcription_mode`, `TranscriptionParams`
- `crates/listener-core/src/actors/session/supervisor.rs` - `should_stop_on_listener_failure`
- `crates/listener-core/src/actors/session/supervisor/children.rs` - `spawn_listener`
- `crates/owhisper-client/src/adapter/openai/live.rs` - OpenAI WebSocket 实时适配器
- `crates/owhisper-client/src/adapter/openai/batch.rs` - OpenAI HTTP batch 适配器
- `crates/owhisper-client/src/adapter/openai/mod.rs` - `resolve_batch_model`, `supports_progressive_batch_model`
- `crates/openai-transcription/src/batch/response.rs` - `CreateTranscriptionResponse`, `deserialize_vec_or_null`

### 路线图

Phase 1 — 实时转录通路打通（已结案 ✅）
  [x] Provider/model 配置 UI + 动态模型列表
  [x] 自定义模型名透传 (AudioModel::Custom)
  [x] provider_hint 贯通使 AdapterKind 正确识别
  [x] default_transcription_mode 返回 Live
  [x] 编译 + WebSocket 连接成功验证（speaches Realtime API 成功连接）
  [x] Batch 转录验证成功（curl 直接调 `/v1/audio/transcriptions` 可正确转写）
  [x] Realtime 转录：裸测验证成功，返回中文转录 "謝謝大家"
  [x] 样本率修复：OpenAI Realtime API 要求 24000 Hz（`live.rs:16`）
  [x] session.update 结构修复：`input_audio_transcription` 在 session 顶层而非 `audio.input` 内（`realtime.rs:14-21`）
  [x] URL 修复：移除 `intent=transcription`，URL model 用对话模型而非 STT 模型（`live.rs:50`）
  [x] VAD 开启：`threshold=0.9`, `silence_duration_ms=1500`, `create_response=false`
  [x] 测试通过：48/48, 0 warnings
  [x] 最终方案：`intent=transcription` + URL 模型名 + `finalize_message` 发 `commit`（匹配工作测试流程）
  [x] `logprobs: null` 反序列化修复（speaches 用 null 代替 []，`realtime.rs:213,222`）
  [x] 实时转录中断后自动重连
  [x] Batch fallback `logprobs: null` 反序列化修复（`batch/response.rs` `deserialize_vec_or_null`）
  [x] 纯 URL 参数连接验证成功（`initial_message: None`）：WebSocket ✅ VAD ✅ 转写 ✅
  [x] `initial_message` 握手（`session.update` 通过 WS 发送）
  [x] `prefix_padding_ms` 降级为 non-fatal warning
  [x] `hadLiveWords` batch fallback 参数

Phase 2 — 功能完善（已收敛 ⏸️）
  [-] 推理速度优化 — 由 Phase 4 Progressive Batch 方案解决（batch 分段并行抵消推理延迟）
  [-] 多语言实时转录支持 — batch 模式天然支持，不阻塞
  [ ] 自定义 prompt/关键词实时生效

Sprint 3 — 本地说话人分离（Phase A/B/C/D ✅ 基本完成，录音流集成待真机验证）
  [x] Speaker segmentation（pyannote ONNX 5.7MB）
  [x] Speaker embedding（选定 wespeaker-cnceleb-LM 256-dim）
  [x] 聚类算法实现（cosine + average-linkage）
  [x] 预处理染色：给音频帧打说话人标签
  [x] VAD Min-Cut + Merge 时长调度（替换 DurationScheduler）
  [x] 词级 speaker 标注 + 前后向填充（integration.rs + runtime.rs）
  [x] UI toggle / 数据流（设置页 + threshold slider）
  [x] 录音流集成 `IncrementalDiarizationEngine`（feed_pcm → finalize）— 待真机验证
  [x] 方案 B 能量停顿切分子段 + 自适应聚类 threshold + 最小时长过滤（Aug 1 Night，5fdd76a7 验证 4 speakers）

Phase 3 — UI 增强（待定）
  [ ] 如果 OpenAI provider + 自建服务器与原生 OpenAI 行为差异过大，加 toggle 开关
  [ ] 实时转录延迟/状态指示器

Phase 4 — Progressive Batch 混合模式（Sprint 2 Phase A/B/C ✅ 已实现）
  [x] 设计文档定稿（见 `docs/progressive-batch-hybrid-design.md`）
  [x] 固定时长分段（30s + 1s overlap，支持 60s）
  [x] PCM raw → WAV header 转换（已改用 memory PCM direct POST）
  [x] 提交队列 + 重试 + Partial Stitch（N=2 并发硬上限，retry 3 次，drain timeout）
  [x] 客户端结果拼接与时间轴去重对齐（stitcher segment_boundaries + abandoned_segments）
  [x] Provider 选项：Live / Batch / Progressive Batch 三种模式可选
  [x] 持久化 + Continue（重启后从 DB 恢复）
  [x] UI 右键菜单分裂（Phase D）

### Jul 22 — 状态总结

**服务器端最近一轮部署（回合 3-7）：**
- Semaphore(2) 并发限制
- `WHISPER__CPU_THREADS=2`, `OMP_NUM_THREADS=2` (chunk time 80-100s → 7-10s, 仍为 4× 实时)
- 移除 `asyncio.shield` → WS cancel 正确释放信号量
- `MAX_SPEECH_DURATION_MS=30000` 强制 30s 分段
- 模型预加载（session create 时）
- 启动时 heartbeat delta（非空）
- keepalive 空 `delta:""` 每 ~2s
- `prefix_padding_ms` 降级 warning-only
- 代码行号变化：`_handler:281` → `_handler:237`（handler 逻辑修改）
- **`Delta transcription chunk timed out` 不再出现** — chunk timeout 问题已解决

**客户端侧改动（已编译，未部署上线）：**
- `FINALIZE_STREAM_TIMEOUT`: `mod.rs:24` 5s → 10s
- `TRAILING_MESSAGE_GRACE`: `client.rs:11` 5s → 10s
- `live.rs` item_id 时序修复：`build_transcript_response` 用 `item_id` 生成 fake timing
- 事件类型日志：`parse_response` 加 `event_label` + `raw_type` 日志

**最新测试 Session `7f365064`（14:39 UTC）：**
- VAD 正常：speech_started → speech_stopped → committed，第二段 3.68s 音频
- 非空 delta 到达：`"是不是很兴趣嗎?有個Settings是什麼?"` — 转录端到端可用
- 连接稳定：keepalive 每 ~2s 空 delta，无 proxy timeout 断开
- samples_dropped 持续出现 — 音频缓冲区跟不上
- **第一段转录延迟 ~100s**（13.6s 语音，whisper 4× 实时推理）
- 会话仍在运行（14:41:48 UTC 最新活动）

### Jul 22 PM — Batch 段落分段修复

- `propagate_identity` 在 `collect.rs:62-84` 合并同 key 段前新增 `provider_segment_index` 边界检查，防止把 server 分好的段落又合并
- 修复后 UI 正确显示段落分段，单词点击跳转正常
- 71/71 测试通过

### Jul 23 — 本地 ggml 兜底 + Speaches 远端模型对比结案 + Progressive Batch 设计评审

**服务端评审结论：** 设计方案可行，服务端不需要改任何东西。
- batch 端点不受信号量限制，N=2 并发直接用
- N100 仅 4 核，建议 `max_concurrency` 默认 2、硬上限 2
- 服务端 batch 端点额外有一层 VAD，会在客户端分段之上再裁剪静音
- 1s overlap + stitcher 时间戳去重即可，无服务端配合需求
- 分段越大效率越高，允许用户配置 30s / 60s

**已完成：**
- 本地 Whisper ggml（Tiny 42MB / Small 252MB）可作为无网络兜底方案，基础链路跑通
- 模型显示名修复：`shared.tsx` 中 `displayModelLabel` 改为优先用 `displayName`
- `prefix_padding_ms` 降级修复（`live.rs` 检查 `"not supported"`），服务端反馈不再复现
- Speaches 远端模型横向对比（同一段 264.8s 中文音频）：

  | 模型 | RTF | 备注 |
  |------|-----|------|
  | ggml-tiny-q8_0 (本地) | 0.69× | 质量差，仅保底 |
  | ggml-small-q8_0 (本地) | 3.85× | 慢且质量不如远端 |
  | faster-whisper-small (远端) | **0.54×** | ⭐ 最优解 |
  | faster-whisper-medium (远端) | 1.7× | 质量提升不明显 |
  | Systran/faster-distil-whisper-large-v3 (远端) | 极慢 | 不支持中文，不可用 |

**结论：** Speaches faster-whisper-small 是当前中文 STT 最优解。

### Jul 22 PM — ⭐ 关键里程碑

Batch 模式下，Speaches 首次实现 **处理时间 < 音频时间**（4m24s 音频 → 124s 推理，约 2.1× 实时）。消费速度首次超过生产速度，为后续优化提供了最基本的数学和逻辑条件。

这意味着：
- 长音频分段并行提交 batch 成为可能
- 实时转录的 VAD 段可以积累到阈值后走 batch 兜底，不再依赖 WebSocket 低延迟
- 可以设计 hybrid 策略：VAD 段短时走 Realtime，超过长度/空闲走 Batch 打平延迟

**剩余问题：**
- 推理速度慢：whisper 4-8× 实时（CPU），3s 音频需 11.7s 推理
- samples_dropped：音频缓冲区下溢
- 客户端改动尚未部署上线

## Jul 25 — 关键里程碑

### 内存优化：流式 PCM 解码 + PCM Direct POST

- `source.collect()` 全文件解码（3h 录音 ~4GB）→ 流式逐块 10s chunks（~15MB）
- `total_duration()` 用于短文件预检（metadata only，不解码全部）
- PCM f32 在内存中直接重采样 16000 Hz → s16le → POST `audio/pcm`，不写临时 WAV 文件
- 消除磁盘 I/O，降低内存峰值 99.6%

### 代码可靠性修复

- URL 构造：`format!("{}/v1/audio/transcriptions", ...)` → `url::Url::path_segments_mut()`，消除双 `/v1/` 拼接 bug
- Response 解析：OpenAI 兼容服务器（Speaches）统一走 `OpenAIAdapter::parse_batch_response`，处理 `logprobs: null` 反序列化
- 阈值参数化：`MIN_DURATION_SECS = 180` 硬编码 → `segment_duration_ms / 1000.0`（阈值由段长决定）

### Bug 修复状态

| # | Bug | 状态 |
|---|-----|------|
| 1 | `getLiveTranscriptionConfig` 忽略 `stt_mode` | ✅ 已修复 (Jul 24) |
| 2 | Re-transcription batch target 回退 whispercpp | 📝 待验证（依赖 Bug 3） |
| 3 | Re-transcription (`startTranscription`) 无响应 | ✅ 已修复 (Jul 25) — URL 构造 + response 解析 |
| - | `progressive_batch` 短音频 WAV 读取失败（mp3） | ✅ 已修复 (Jul 24 — Gap F: 原文件字节 POST) |
| - | 长音频分段 WAV 编码 | ✅ 已修复 (Jul 25 — Gap G: PCM direct POST) |

### UI 扩展

- `segment_duration_ms` 选项：30s / 60s / 3m / 5m / 10m（设置页面）

### 文档同步

- `progressive-batch-hybrid-design.md` — threshold、编码格式、Appendix B 对比表、5 个新增差异条目
- `progressive-batch-data-structures.md` — struct 定义、数据流图、HTTP POST 细节、内存管理 §7
- `progressive-batch-bug-list.md` — Gap G ✅

### ⚠️ 待明天验证（Jul 26）

1. `cargo check` + 全部测试通过
2. Bug 2（batch target fallback）在 Bug 3 修复后是否可复现
3. 端到端测试 Progressive Batch 长音频通路（URL 正确 / 无 double /v1/）
4. 确认内存峰值 <50MB（流式解码）
5. 删除 `min_duration_secs` 字段（冗余，由 `segment_duration_ms` 替代）

## Jul 26 — 分段增量显示 + 分段分隔显示

### 完成

1. **BatchSegmentResult 事件通路**
   - `listener2-core/events.rs`: 新增 `BatchSegmentResult { session_id, segment_index, response }` 变体
   - `plugins/transcription/src/api.rs`: 新增 `TranscriptionEvent::SegmentResult` + `From` 转换
   - `plugins/transcription/src/listener2/ext.rs`: 更新 `last_activity_tx` 匹配包含 `BatchSegmentResult`
   - 运行 `cargo test export_types` 重新生成 `bindings.gen.ts`

2. **Runtime 贯通 Progressive Batch**
   - `ProgressiveBatchConfig`: 新增 `session_id` 字段
   - `ProgressiveBatchManager`: 新增 `runtime: Option<Arc<dyn BatchRuntime>>` + `with_runtime()` 方法
   - `run_progressive_batch_from_file`: 接收 `runtime` 参数并透传
   - `submit_file_direct`: 同样接收 `runtime`，单段音频也 emit `BatchSegmentResult`
   - 在 `on_audio_frame()` 和 `finish()` 的 `poll_completed` 循环中 emit

3. **Segment boundaries 元数据**
   - `stitcher.rs`: 重构 `stitch()` 用 `TaggedWord` 追踪词源分段索引，输出 `segment_boundaries: Vec<usize>`（各段在拼合结果中的起始词下标）

4. **前端增量显示**
   - `batch.ts`: 新增 `batchSegments: Record<string, Record<number, BatchResponse>>` + `handleBatchSegmentResult` action；`handleBatchResponse` / `clearBatchSession` 自动清理
   - `general-batch.ts`: 新分支 `payload.type === "segmentResult"` → `handleBatchSegmentResult`
   - `state.ts`: `running_batch` screen 新增 `segmentResponses` 字段
   - `index.tsx`: 新增 `SegmentPreview` 组件，在 `running_batch` 进度下方按序展示已完成的片段+虚线分隔
   - `empty.tsx`: 新增 `segmentCount` 属性显示 "N segments transcribed"
   - `segment.tsx`: 检测 `word.metadata.segment_boundary`，渲染虚线分隔标记

5. **代码生成**：`plugins/transcription` codegen 测试（`export_types`）更新 `bindings.gen.ts`

### 验证
- `cargo check`: ✅
- `cargo test -p listener2-core`: 109/109 ✅
- `pnpm -F @hypr/desktop typecheck`: ✅（仅剩一个 pre-existing warning）
- `pnpm exec dprint fmt`: ✅

## Jul 26 — Sprint 2 设计定稿（启动前 Commit）

Sprint 2 分四个 Phase：

| Phase | 内容 | 关键改动 |
|-------|------|----------|
| **A** | Source pipeline PCM 集成 | `ListenerRouting::ProgressiveBatch` 变体 + Pipeline dispatch + Supervisor 创建 channel + Listener2 消费 |
| **B** | 重试 + Drain 超时 + Partial Stitch | `drain(timeout)`、stitch 不报 missing、`finish()` 允许 partial response |
| **C** | 持久化 + Continue | progressive_batch_jobs/segments 表 + Drizzle schema + continue_from_file() |
| **D** | UI 右键菜单 | Re-transcribe 裂为 3 项 + Continue 条件显示 + 部分结果提示 |

设计文档已同步更新：
- `docs/progressive-batch-data-structures.md` — v2 DB schema、重试协议、Continue 流程、UI 菜单
- `docs/progressive-batch-hybrid-design.md` — Sprint 2 Phase 分解
- `docs/progressive-batch-bug-list.md` — Gap A/L/M/N/O/P（Sprint 2）vs ✅ Gaps

## Jul 27 — Sprint 2 Phase A/B/C 全部完成 🔥

### Phase A: Source Pipeline PCM 实时流集成

| 组件 | 改动 |
|------|------|
| `source/mod.rs` | `ListenerRouting::ProgressiveBatch(PcmSender)` 变体替代独立字段 |
| `runtime.rs` | `start_progressive_batch_stream()` — 从 PCM channel 接收帧并馈入 Manager |
| `session/types.rs` | `SessionParams.progressive_batch_pcm_tx` 传递 Sender |
| `supervisor.rs` | 判断 `transcription_mode == ProgressiveBatch` → 创建 channel + 不透传 Listener |
| `supervisor/children.rs` | `spawn_progressive_batch_streamer` 启动 Listener2 + PCM 馈送任务 |
| 前端 | 无需改动（`SegmentPreview` 同文件路径，stream 自动增量展示） |

### Phase B: 重试 + Drain 超时 + Partial Stitch

| 功能 | 实现位置 | 描述 |
|------|----------|------|
| `drain(timeout)` | `mod.rs:241-256` | `tokio::time::timeout(segment_duration_ms * 1.5)` 避免无限等待 |
| Partial stitch | `stitcher.rs` | `stitch()` 永远返回 `Response`，missing 段标记 `abandoned_segments` + `gap_warnings` |
| Partial finish | `mod.rs:382-391` | `finish()` drain 后有 failed 段仍返回可用结果，仅全失败才 Err |
| 测试覆盖率 | `tests/` | retry/drain/partial 场景全覆盖 |

### Phase C: 持久化 + Continue

| 组件 | 改动 |
|------|------|
| `progressive_batch_jobs/segments` 表 | DB 迁移 + Drizzle schema + `persist_batch_event` |
| `Manager::resume()` | 从 DB 恢复 job + segment 状态，跳过已完成段 |
| `continue_from_file()` | 从 DB job 重启 ProgressiveBatchManager |
| `ext.rs` | `persist_batch_event` 异步写 DB，`continue_transcription` 桥接 |
| `commands.rs` | `continue_progressive_batch` 命令简化（`get_by_id.id == session_id`）|
| `BatchParams` | `overlap_ms`/`max_concurrency` 贯通，Continue 从 DB 恢复配置 |

### 验证总结

| 命令 | 结果 |
|------|------|
| `cargo check` | ✅ |
| `cargo test -p listener2-core` | ✅ 191/191 |
| `cargo test -p plugins-transcription` | ✅ |
| `cargo test -p db-app` | ✅ |
| `pnpm typecheck` | ✅ |
| `dprint fmt` | ✅ |

### 剩余 Phase D

右键菜单裂为三项：Re-transcribe（Total）/ Re-transcribe（Progressive）/ Continue | 部分结果提示

### 设计文档同步
- `docs/progressive-batch-hybrid-design.md` §9: 标记 A/B/C ✅
- `docs/progressive-batch-hybrid-design.md` §10: 差异表更新（A/L/M/N/O → ✅）
- `docs/progressive-batch-bug-list.md`: Gap A/L/M/N/O 移入 ✅ 已修复，仅剩 Gap P
- `docs/progressive-batch-data-structures.md`: 未改动（设计本身无偏差）

## Jul 27 — Sprint 2 Phase D（UI 右键菜单 ✅）

### 右键菜单裂为三项（仅 stt_mode === "progressive" 时显示）

| 菜单项 | 功能 | 后端调用 |
|--------|------|----------|
| **Re-trans(Total)** | 强制标准 batch（全文件一次性转录） | `runBatch` with `forceProgressive: false` |
| **Re-trans(Progressive)** | 从头跑 progressive batch | `runBatch` with `forceProgressive: true` |
| **Continue(Progressive)** | 续传未完成 progressive batch job（条件显示） | `continueProgressiveBatch` 命令 |

**关键决策：**
- 仅当用户配置 `stt_mode === "progressive"` 时，右键菜单和 Overflow 下拉菜单显示 3 项
- `stt_mode === "batch"` 或 `"live"` 时，菜单保持原有单 "Re-transcribe" 项（等同于 Total）
- "Continue" 仅当 DB 中存在 `interrupted`/`partial` job 时才可见（通过 `useContinuableBatchJob` + `listProgressiveBatchJobs` 查询）
- `useContinueTranscript` 自动从 `useSTTConnection` 获取 API key

### 改动文件

| 文件 | 改动 |
|------|------|
| `apps/desktop/src/stt/useRunBatch.ts` | `RunOptions` 新增 `forceProgressive?: boolean` |
| `apps/desktop/src/.../transcript/actions.ts` | 新增 `useContinuableBatchJob`、`useContinueTranscript`；`useRegenerateTranscript` 接受 `mode` 参数 |
| `apps/desktop/src/.../note-input/header.tsx` | 右键菜单条件分裂 3 项 |
| `apps/desktop/src/.../overflow/index.tsx` | Overflow 下拉菜单同样条件分裂 |

### 验证

| 命令 | 结果 |
|------|------|
| `npx tsc --noEmit` | ✅（仅 1 pre-existing 错误） |
| `dprint fmt` | ✅ |

## Jul 28 — Atomic range collapse for ≤4 CJK multi-char words

### 问题
- `acoustic_only` (no jieba) mode fragments ≤4 CJK words because `split_to_entries` always splits multi-char words into single chars, and `acoustic_only` / `acoustic_verify` can't reliably merge them back.
- 404 words → 663 groups in test session.

### 方案
- `split_to_entries` takes `min_cjk_split_len: usize` param + returns `atomic_ranges: Vec<(usize, usize)>`.
- CJK words with `chars.len() < min_cjk_split_len` are still split into single chars (pipeline compatibility) but their entry index range is recorded as "atomic".
- After pipeline (`processor.process()`), `collapse_groups` merges any `WordGroup`s that fall within the same atomic range back into ONE group with the original multi-char word text.
- Trailing punctuation from the last merged group is re-appended.

### 阈值
- `flags.jieba == true` → `min_cjk_split_len = 5` (≥5 split free for jieba; ≤4 protected)
- `flags.jieba == false` → `min_cjk_split_len = usize::MAX` (ALL multi-char CJK words protected)

### 改动文件

| 文件 | 改动 |
|------|------|
| `crates/listener2-core/src/batch/cjk.rs` | `split_to_entries` takes `min_cjk_split_len` + returns `atomic_ranges`; new `collapse_groups` fn; `process_response` wires both |

### 验证

| 命令 | 结果 |
|------|------|
| `cargo check -p listener2-core` | ✅ |
| `cargo test -p cjk-processor` | ✅ 14/14 |
| `cargo test -p listener2-core` | ✅ 115/115 |

## Jul 28 (PM) — Server-side CJK post-processing toggle for Speaches

**设计文档同步：** `docs/cjk-processing-design.md` 已创建，记录整体 CJK 架构（本地 + 服务端）。`docs/progressive-batch-hybrid-design.md` 差异表新增 `cjk_server_side` 行。

### 背景
Speaches（OpenAI 兼容服务器）更新了 CJK 生效模式：默认不启用 CJK 后处理，需显式传 `cjk_post_process=true` 才会开启服务端 CJK 流程。

### 改动

**数据流：** `cjk_server_side` setting → `TranscriptionParams` → `BatchParams` → `ListenParams` → `build_transcription_options` → `CreateCustomTranscriptionOptions.cjk_post_process` → multipart form field `cjk_post_process=true`

| 层 | 文件 | 改动 |
|------|------|------|
| Setting | `schema.ts:171` | 新增 `cjk_server_side` boolean，默认 `false` |
| UI | `select.tsx:898-967` | SttModeSection 重构：Mode / Segment / CJK (Server) 三个控件共用一行 |
| UI | `select.tsx:1015` | 本地 CJK 区域标题改名 "CJK post-processing (Local)" |
| Frontend | `useRunBatch.ts:185,418` | 读取 `cjk_server_side`，传入 `TranscriptionParams` |
| Tauri API | `plugins/transcription/api.rs:240` | `TranscriptionParams` 增加 `cjk_server_side` 字段，`From` impl 贯通 |
| Batch params | `crates/listener2-core/batch/mod.rs:132` | `BatchParams.cjk_server_side` + `build_listen_params` 传值 |
| HTTP layer | `crates/owhisper-client/adapter/openai/batch.rs:210-215` | `build_transcription_options` 读取 `cjk_server_side`，设置 `cjk_post_process=true` |
| Multipart | `crates/openai-transcription/batch/request.rs:79,269-274` | `CreateCustomTranscriptionOptions.cjk_post_process` 字段，emit form field |

### 验证

| 命令 | 结果 |
|------|------|
| `cargo check -p listener2-core -p tauri-plugin-transcription` | ✅ |
| `cargo test -p listener2-core` | ✅ 115/115 |
| `pnpm -F @hypr/desktop typecheck` | ✅ |

## Jul 29 — Sprint 3 Phase A/B 全部完成 🔥

### 模型对比 (19 tests, all ✅)

经过 5 个模型、中文/韩语/英语多维度对比：

| 模型 | Dim | F-M 分离 | 中文 4-spk avg | 85s Embed 时间 |
|------|-----|:-------:|:-------------:|:------------:|
| **wespeaker-cnceleb-LM** ⭐默认 | 256 | 0.7301 | **0.8803** | 7.04s |
| wespeaker-voxceleb | 256 | 0.7862 | 0.8595 | 8.24s |
| wespeaker-cnceleb | 256 | 0.6914 | 0.8456 | 7.23s |
| campplus-200k ⭐备选 | 192 | **0.8533** | 0.7816 | **3.71s** |
| campplus-zh-en (csukuangfj) | 192 | N/A (x/embedding 未匹配) | — | — |
| pyannote-local | 512 | 0.0995 | — | — |

**最终选择：** 默认 `wespeaker_zh_cnceleb_resnet34_LM.onnx`（CN-Celeb 中文训练 + LM 微调），备选 `campplus_cn_en_common_200k.onnx`（最快）。

### Sprint 3 设计

**Pipeline:** VAD → 染色 → 时长调度(±20% 水线) → SubmitSegment 队列 → 提交(纯 OpenAI 标准) → 响应匹配

**关键设计决策：**
- **方案A**: VAD 染色优先，再 ±20% 水线合并/切分后提交
- **本地元数据**: speaker/时序全部本地管理，不发送给服务端
- **SubmitSegment**: 队列持久化到 DB，支持断线恢复
- **3 Phase**: Phase A (DiarizationManager) → Phase B (DurationScheduler + SubmitManager) → Phase C (UI)

### Phase A: DiarizationManager（~2天 ✅）

| 组件 | 文件 | 描述 |
|------|------|------|
| `DiarizationManager` | `crates/pyannote-local/src/diarization.rs` | VAD → embed → short segment merge → cluster 管线 |
| `SpeakerSegment` | `diarization.rs:27-32` | start/end/speaker/embedding_valid |
| `DiarizationConfig` | `diarization.rs:8-13` | model_path/threshold/min_segment_duration/sample_rate |
| 短段合并 | `diarization.rs:198-218` | `<1.5s` 合并到相邻有效段 |
| Agglomerative clustering | `crates/pyannote-local/src/clustering.rs` | cosine distance + average-linkage + threshold |
| `EmbeddingProvider` trait | `crates/pyannote-local/src/embedding_providers.rs` | compute(⩍) / embedding_dim / name |
| `FbankEmbedding` | `embedding_providers.rs` | 自动 tensor name probe (NAME_SETS) |
| `PyannoteEmbeddingProvider` | `crates/pyannote-local/src/embedding.rs` | 内置 pyannote-local 模型包装 |
| 集成测试 | `tests/diarization_pipeline.rs` | 21 tests (模型对比 + 多语言 + Manager) |

### Phase B: DurationScheduler + SubmitManager + DB（~1天 ✅）

| 组件 | 文件 | 描述 |
|------|------|------|
| `DurationScheduler` | `crates/pyannote-local/src/duration_scheduler.rs` | ±20% 水线调度 (Wait/Submit/SplitAndSubmit) |
| `schedule_segments()` | `duration_scheduler.rs:103-126` | 批量调度入口 |
| `DiarizationSubmitter` | `crates/listener2-core/src/batch/diarization/submit.rs` | 队列 + N=2 并发 + 指数退避重试(3次) + drain 超时 |
| `run_submitter()` | `mod.rs:88-126` | 集成提交 + 事件 emit + speaker 标签合并 |
| `BatchParams` 字段 | `crates/listener2-core/src/batch/mod.rs:134-139` | diarization_enabled/model/threshold |
| `BatchEvent` 变体 | `crates/listener2-core/src/events.rs:62-77` | DiarizationStarted + DiarizationSegmentResult |
| DB 迁移 | `crates/db-app/migrations/20260729000000_diarization_jobs.sql` | diarization_jobs + diarization_segments 表 |
| Drizzle schema | `packages/db/src/schema.ts` | diarizationJobs + diarizationSegments |
| Rust 行类型 | `crates/db-app/src/session_types.rs` | DiarizationJobRow + DiarizationSegmentRow |
| 持久化 handler | `plugins/transcription/src/listener2/ext.rs:474-552` | persist_batch_event 写 diarization 表 |
| `TranscriptionEvent::from` | `plugins/transcription/src/api.rs` | 映射 diarization 事件到前端 |

### 验证总结

| 命令 | 结果 |
|------|------|
| `cargo check -p listener2-core` | ✅ |
| `cargo check -p db-app` | ✅ |
| `cargo test -p listener2-core` | ✅ 115/115 |
| `cargo test -p db-app` | ✅ 44/44 |
| `cargo test -p pyannote-local` | ✅ 36/36 (15 unit + 21 integration) |

## Jul 30 — Sprint 3 Phase C/D（VAD Min-Cut + Merge + Diarization UI）✅

### Phase 0: VAD Min-Cut + Merge（替换 DurationScheduler）✅

- `crates/pyannote-local/src/min_cut_merge.rs` — WhisperX 式 Min-Cut + Merge，超 max_duration 的 VAD 段在 [½τ, τ] 找最低分切开，短段合并到 ≤τ
- `crates/listener2-core/src/batch/progressive_batch/integration.rs` — `schedule_segments` → `min_cut_merge` 分组
- `crates/pyannote-local/src/duration_scheduler.rs` — 已移除（含 8 个 unit tests）
- diarization 路径（batch 模式）：改用 `IncrementalDiarizationEngine.feed_segments()`（复用外部 Segmenter VAD 段，而非引擎内部 VAD）

### 设置页 UI

`apps/desktop/src/settings/ai/stt/select.tsx` DiarizationSection：
- diarization toggle + model 下拉 + threshold slider（min 0.1 / max 0.99 / step 0.01）
- **threshold 默认 0.35 → 0.85**（经真实音频 sweep 验证，见 `tests/find_threshold.rs`）

## Jul 31 — 录音中实时分段回显修复 ✅（当前会话）

### 问题

progressive-batch 新录音模式下，录音进行中的 batch 段结果（`SegmentResult`）被前端丢弃：后端实时路径持续 emit `TranscriptionEvent::SegmentResult`，但前端录音期间**从不订阅 `transcriptionEvent`**（该订阅只在 `general-batch.ts` 的 `runBatchSession`（stop 后）里）。

### 根因

`general-live.ts` 的 `listenToAllSessionEvents` 只订阅 `captureLifecycle` / `captureStatus` / `captureData` 三个事件。后端 `plugins/transcription/src/listener/runtime.rs:137`（`LiveBatchRuntime::emit`）在录音中正常 emit `SegmentResult`，全部被丢弃。

### 修复（4 文件，纯前端）

| 文件 | 改动 |
|------|------|
| `general-live.ts` | `listenToAllSessionEvents` 新增 `listenerEvents.transcriptionEvent.listen`；`transcription` handler 路由 `segmentResult` → `handleBatchSegmentResult`；守卫：`payload.session_id === targetSessionId`、`live.sessionId === targetSessionId`、**`liveTranscriptionActive !== false`** |
| `state.ts` | `batch_fallback` 屏新增 `segmentResponses: Record<number, BatchResponse>` |
| `index.tsx` | `fallbackSegments` 时渲染 `CompactProgress`（recording 变体："Recording... transcription in progress · N segments done"，Stop → `state.stop`）+ `SegmentPreview`；`CompactProgress` 新增 `recording?: boolean` |
| `general.test.ts` | 新增 `listenTranscriptionMock` hoisted mock |

### 模式隔离（不破坏 live / 传统 batch）

- **live 模式**：走 Realtime adapter emit `captureDataEvent.transcript_delta`，不 emit `SegmentResult`；前端 `liveTranscriptionActive === true` → handler 直接 return
- **传统 batch**：录音中无 `SegmentResult`（无 progressive stream），`batchSegments` 空 → `fallbackSegments=false` → 保持原 `BatchState` 等待 UI
- 后端只在 `TranscriptionMode::ProgressiveBatch` 时启动 `start_progressive_batch_stream`（`supervisor.rs:65,75,126`、`children.rs:145`）
- `applyCaptureSnapshot` 用 `markLiveActive(..., snapshot.liveTranscriptionActive ?? true)` 恢复，progressive batch snapshot 为 false → attach 场景正确放行

### 验证

| 命令 | 结果 |
|------|------|
| `pnpm -F desktop typecheck` | ✅ |
| `cargo check -p pyannote-local -p listener2-core -p listener-core -p tauri-plugin-transcription` | ✅ |
| `cargo test -p pyannote-local` | ✅ 21 integration + 1 sweep |
| `cargo test -p listener2-core` | ✅ 118/118 |
| `general.test.ts` | ✅ 27/28（1 失败为 pre-existing：`handleBatchResponse persists transcript-only batch responses`，`timing.ts` 读到 `provider_word` 未 fallback `synthetic_text`，与本次改动无关）|
| `general-batch.test.ts` / `transcript.test.ts` | ✅ 11/11、9/9 |

⚠️ 命令注意：vitest 必须从 `apps/desktop` 目录运行（根目录报 `Cannot find module '~/stt/timing'`）；全量 `dprint fmt` / `vitest --run` 会挂，用 `./node_modules/.bin/dprint fmt <文件>` 与 `./node_modules/.bin/vitest run <文件>`。

## Aug 1 — VadGroupStream 重构：live 分段与 file re-transcribe 对齐 ✅

### 问题

live（ProgressiveBatchManager）用固定时长窗口分段（`segmenter.rs`），而 file re-transcribe（`integration.rs` `run_progressive_batch_from_file`）用 VAD + Min-Cut/Merge 分组。两条路径分段不一致（bbb1eb3b 验证中 live/re 尾部 VAD 漏检即因此）。

### 方案

新增 `crates/listener2-core/src/batch/progressive_batch/vad_group.rs`：

- `VadGroupStream` — 流式 VAD（`IncrementalVad`，pyannote segmentation.onnx）+ 增量 Min-Cut/Merge，逐段复刻 `min_cut_merge::merge_segments` 的判定规则 → 与 batch 全量处理产出完全相同的组。组样本覆盖 `[首段 speech 起点, 末段 speech 终点]`（含组内静音，与 file 路径一致）。
- `VadSource` trait + `IncrementalVadSource` — 隔离 VAD 实现，便于用 `MockVad` 单测分组逻辑（等价性测试对照 batch `min_cut_merge`）。
- `IncrementalVad` 新增 `all_samples_len()` / `sample_slice()`（复用其保留的全局样本缓冲，避免二次存 PCM）。
- `min_cut_merge::split_segment_at_energy` 改 `pub`（跨 crate 复用）。

### 接线

| 层 | 改动 |
|------|------|
| `ProgressiveBatchConfig` | 新增 `vad_groups: bool`（默认 false，测试/continue 保持固定窗口） |
| `ManagerState::Active` | `segmenter: Segmenter` → `segmenter: Box<dyn SegmentProducer>`；新增 `SegmentProducer` trait（feed/flush），`Segmenter` 与 `VadGroupStream` 各自实现 |
| `make_segmenter()` | 按 `vad_groups` 选择；VAD 初始化失败降级回固定窗口（log warning） |
| 插件 `runtime.rs` | `build_progressive_batch_config` → `vad_groups: true` + `overlap_ms: 0`（VAD 组无重叠，stitcher 不得去重） |
| `continue_from_file` | 现 `vad_groups: true` + `overlap_ms: 0` + 流式重采样到 16k（`StreamingResampler`），与 file 路径 VAD 分组对齐（见 Aug 1 PM 收尾） |

### 行为差异（live，静音时）

- 完全静音录音 → `flush()` 兜底整段作为一个组提交（镜像 `submit_file_direct` 空 VAD 分支），不回归为报错。
- 首段产出延迟：VAD 需 10s 窗口才出帧，组在语音结束后才 emit（与 file 路径同约束）。

### 验证

| 命令 | 结果 |
|------|------|
| `cargo check -p listener2-core -p pyannote-local -p tauri-plugin-transcription` | ✅ |
| `cargo test -p listener2-core` | ✅ 125/125（+7 新增 vad_group 测试） |
| `cargo test -p tauri-plugin-transcription` | ✅ 34/34 |
| `cargo test -p pyannote-local --lib` | ✅ 14/14（min_cut_merge 全过） |
| `cargo check -p desktop` | ⚠️ 环境无法完成：`notification-macos` build script 需联网 fetch swift-rs（github 不可达），与本次改动无关 |

## Aug 1 (PM) — 遗留收尾：live diarization 对齐 + Continue 对齐 + VAD 内存 prune ✅

### 遗留①：live diarization 复用 VadGroupStream 的 VAD 段流 ✅

live 路径（`plugins/transcription/src/listener/runtime.rs`）不再用 `IncrementalDiarizationEngine.feed_pcm`（引擎内部 VAD），改用与 file 路径相同的 `feed_segments`：

| 层 | 改动 |
|------|------|
| `SegmentProducer` trait | 新增 `take_vad_segments()`（默认空） |
| `VadGroupStream` | 新增 `vad_segments` 缓冲 + `collect_vad_segments` 开关（默认 false，无消费者时零内存）；feed/flush 收集 VAD 段，`take_vad_segments` 排出 |
| `ProgressiveBatchConfig` | 新增 `collect_vad_segments: bool`（默认 false），`make_segmenter` 透传 |
| `ProgressiveBatchManager` | 新增 `vad_segments` 字段（收 `finish()` flush 尾部）+ `take_vad_segments()`（排空 Active segmenter + 尾部） |
| `runtime.rs` | `build_progressive_batch_config` → `collect_vad_segments: params.diarization_enabled`；帧循环先 `on_audio_frame` 再 `take_vad_segments → feed_segments`；`finish()` 后补喂尾部再 `finalize()` |

结果：live speaker 标注与 file 路径使用完全同一批 VAD 段（`VadGroupStream` 内部 VAD 的段），而非引擎内部独立的 VAD 结果。

### 遗留②：Continue 对齐 vad_groups ✅

`continue_from_file`（`integration.rs`）现在与 file re-transcribe 完全对齐：
- `vad_groups: true` + `overlap_ms: 0`（VAD 组无重叠，stitcher 不去重）
- 新增 `StreamingResampler`（线性插值，输出采样 i 位置 = `i * ratio`，与全量重采样逐点一致）把源流式重采样到 16k 再喂入
- file 路径始终 VAD 分组（`run_progressive_batch_from_file`），live 不持久化 job → 不存在固定窗口 job 需要 Continue 兼容，无需 DB 迁移持久化 segmenter kind

### 遗留③：IncrementalVad 内存 prune ✅

`IncrementalVad` 新增 `origin` 绝对起点偏移 + `prune_before(sample_idx)`；`VadGroupStream.emit_group` 在组发出后 prune 掉组结束前的样本。live 不再保留完整 PCM（i16，1h≈115MB），只保留最后一个已发组终点之后的样本。`all_samples_len()` / `sample_slice()` 改为 absolute 语义（origin + len）。

### 验证

| 命令 | 结果 |
|------|------|
| `cargo check -p listener2-core -p pyannote-local -p tauri-plugin-transcription` | ✅ |
| `cargo test -p listener2-core` | ✅ 127/127（+2 vad_group take_vad_segments 测试） |
| `cargo test -p tauri-plugin-transcription` | ✅ 34/34 |
| `cargo test -p pyannote-local --lib` | ✅ 14/14 |
| `cargo test -p pyannote-local --test diarization_pipeline --test find_threshold` | ✅ 21 + 1 sweep |
| dprint fmt | ✅ |

## Aug 1 (Night) — 方案 B：能量停顿切分 + 自适应 threshold + 最小时长过滤 ✅

### 问题根因（实证定位）

用户实测"diarization 1 unique speakers"的**真正根因是聚类 threshold=0.85 太松**，不是分块方式：
- 71 个 2s 子段 embedding 两两余弦距离：p10=0.416 p50=0.676 p90=0.838 → 一半以上距离 < 0.85 → 全并成 1 簇。
- 最优 threshold 随音频变化：5fdd76a7 需 ~0.70（中位距 0.687），c5ee333b 需 ~0.60（中位距 0.468）。固定阈值无法跨音频通用。
- 两音频距离尺度差异巨大，任何固定分位数都不可行；"合并距离最大间隙/相对间隙"启发式也失败（最大间隙总在最后 2→1 合并）。

### 落地改动

| 组件 | 改动 |
|------|------|
| `min_cut_merge.rs` | 新增 `split_into_turn_chunks`（:112）：短时能量(30ms RMS)找静音谷 → 停顿中点切子段（切分点由音频决定）；阈值 = `max(noise_floor*3, median*0.3).min(peak*0.5)`；无停顿区段回退 `max_chunk_s`(4s) 封顶。替换固定 2s 窗口 |
| `incremental_diarization.rs` | `feed_one_segment` 改用 `split_into_turn_chunks`（MIN_PAUSE_SECS=0.2, MAX_CHUNK_SECS=4.0）；`recluster` 在 threshold=默认0.85 时改调 `clustering::estimate_threshold` 自适应；新增 `smooth_speakers` 按累计时长过滤（MIN_SPEAKER_SECS=2.0，删除 <2s 的孤立 speaker 归并到最近邻居） |
| `clustering.rs` | 新增 `estimate_threshold`（:101）：median + 0.15·MAD 的两两距离，clamp [0.4, 0.9] |
| `tests/find_threshold.rs` | 新增 `adaptive_threshold_recovers_multiple_speakers` 回归测试（默认 0.85 → 自适应，断言 ≥3 speakers） |

### 实证结果（真实音频 5fdd76a7，已知 4 说话人）

| 配置 | 结果 |
|------|------|
| fixed 0.85（旧默认） | **1 speaker**（bug 复现） |
| fixed 0.70 | 4 speakers |
| **adaptive + 2s 过滤** | **4 speakers** ✓ spk0(0.6-2.7s 片头)/spk1(3.1-67.2s 主讲)/spk2(67.9-72.0+78.5-83.9+110.4-119.0 Q&A)/spk3(72.0-78.5 Q&A)，与人工验证 [1,2,0,3] 一致 |
| c5ee333b（未知真值，264s） | adaptive 18→(2s 过滤后)7；fixed0.70 仍塌成 1 |

### 验证

| 命令 | 结果 |
|------|------|
| `cargo check -p pyannote-local -p listener2-core -p tauri-plugin-transcription -p listener-core` | ✅ |
| `cargo test -p pyannote-local --lib` | ✅ 26/26（+3 estimate_threshold、+3 smooth_speakers） |
| `cargo test -p pyannote-local --test diarization_pipeline --test find_threshold` | ✅ 21 + 1 sweep + 1 adaptive 回归 |
| `cargo test -p listener2-core` | ✅ 127/127 |
| `cargo test -p tauri-plugin-transcription` | ✅ 34/34 |
| dprint fmt | ✅ |

### 遗留

- c5ee333b 的真实说话人数未确认（DB 里无 ground truth，transcript speaker 全是 provider 的 speaker_index:0）。adaptive+过滤给 7，若实际更少可再调 MIN_SPEAKER_SECS。
- threshold 仍是用户可配置（设置滑块 0.1-0.99）；engine 仅在默认 0.85 时走自适应，显式调过滑块的值优先。

## Aug 2 — 服务端丢段确认修复 + 1:05-1:30 段 whisper loop/500 调查（报给服务端）

### ① 服务端丢段确认修复 ✅

之前"seg5-from-mp3 只转写前 8.6s"的服务端问题，服务端声明已修复。用可复现截断的 `/tmp/hypr-dump/seg5-from-mp3.raw`（796800B）重新 POST：
- 现在**完整转写 24.9s**（97 words，覆盖到 24.9s），与 `seg5-full.raw` 结果一致。
- 无需客户端改动。

### ② 新问题：1:05-1:30 段被跳过（whisper loop / 500）⏸️ 已报服务端

re-transcript 实测：1:05-1:30 段（女声中文 → 男声带东南亚口音英语切换区）仍缺失。日志 `progressive_batch-test.log` metadata：`segments_total: 7, segments_stitched: 6, abandoned_segments: 3, gap_warnings: after_segment: 3, before_segment: 4, gap_ms: 11261` → **group3（67.88-87.45s）被 abandoned**。

**根因（客户端复现 + 对照实验定位）：**
- `integration.rs` 用 `min_cut_merge` 分 7 组；group3=67.88-87.45s，group4=87.45-110.24s，两组合并（42.36s）提交 → HTTP 200 但返回 100+ 次重复"對"的 garbage。
- 客户端 rodio 解码的 group3（626240B）单独 POST → **稳定 HTTP 500**（3/3 次，0.2s 立即失败，`{"detail":"Internal server error"}`）；拆半段后 200 但前半 garbage。
- 同段 **ffmpeg 解码**（字节数相同 626240B）→ **完全正常**（"我覺得是有的就是...因為中國是一個非常大的國家..."）。
- 两解码器内容等价（能量包络相关 0.9947，RMS/peak/zcr 一致，时间对齐偏移 0）但**逐字节不同**（626240B 中 ~519KB ≈83% 不同）。
- rodio 版 whisper 陷入**重复输出 loop**（`avg_logprob=-0.078` 异常高 = 自洽循环；正常转写 `avg_logprob=-0.50`）→ worker 内部异常 → 500。
- rodio 开头无静音填充（mp3 第 0 采样即内容），ffmpeg 开头 ~60ms 静音 —— 可能是触发差异之一。

**结论**：服务端 whisper 对输入字节细微差异敏感（等价音频一个 loop/500、一个正常）。客户端重试 3 次无效（稳定复现，不是瞬态）。**已报给服务端**，证据文件见问题清单 #8。

**客户端侧候选防御（若服务端短期不修）**：检测响应中重复词比例 / `avg_logprob` 异常高 → 对该组换 ffmpeg 解码重试。

### 验证 / 提交

- `git log`: `c495cd5ba`（min_duration_secs 清理确认）、`f82712ccb`（服务端丢段修复确认）、Aug 1 三个 commit（diarization 方案B/后端/前端）
- 工作区干净，无未提交代码（仅临时诊断文件已删除）

## 下一步工作排布

### 未解决问题清单（截至 Aug 2 提交后）

| # | 问题 | 状态 | 后续动作 |
|---|------|------|----------|
| 1 | **服务端丢段**（seg5-from-mp3 截断，只转写前 8.6s）| ✅ 已修复（Aug 1 验证）| 服务端已解决；用之前可复现截断的 `seg5-from-mp3.raw` 重新 POST，现在完整转写 24.9s（97 words，覆盖到 24.9s），与 `seg5-full.raw` 结果一致。无需客户端改动 |
| 2 | **c5ee333b 说话人数未确认** | ⚠️ DB 无 ground truth（transcript speaker 全是 provider 的 speaker_index:0）| adaptive+2s 过滤给 7；若实际更少，调 `MIN_SPEAKER_SECS`（现在 2.0）或 MAD 系数（现在 0.15）|
| 3 | **Bug 2**：Re-transcription batch target 回退 whispercpp | 📝 待验证 | 依赖 Bug 3（已修 Jul 25），需复测确认不再回退 |
| 4 | **`min_duration_secs` 冗余字段删除** | ✅ 已确认（Aug 1）| 代码里已 0 处引用（全库搜索无匹配）——Jul 25 已由 `segment_duration_ms` 替代（`integration.rs:55`：`duration_secs < segment_duration_ms/1000 → submit_file_direct`）。`min_duration_secs` 从未是配置项，原为硬编码常量 `MIN_DURATION_SECS=180`。问题清单残留记录已清理，无需改代码 |
| 5 | **真机端到端**：live 录音 speaker 标签是否随 `SegmentResult` 正确显示 | 待验证 | 需真机录音测试（Sprint 3 Phase D 收尾）|
| 6 | **内存峰值确认** <50MB（流式解码 + VAD prune）| 待实测 | 长录音（~1h）跑一遍验证 |
| 7 | **MAD 系数 0.15 标定单一音频** | ⚠️ 需更多音频样本 | 当前只基于 5fdd76a7（4 说话人）标定；收集更多多说话人音频验证自适应阈值泛化 |
| 8 | **服务端 whisper 对字节敏感**：1:05-1:30 段（group3=67.88-87.45s，女声中文→男声东南亚口音英语切换区）客户端 rodio 解码字节提交稳定 HTTP 500 或返回重复"對"的 whisper loop garbage；同段 ffmpeg 解码字节完全正常 | ⏸️ 已报给服务端（Aug 2）| 两个解码器内容等价（能量包络相关 0.9947，RMS/peak/zcr 一致，时间对齐 0）但逐字节不同（626240B 中 ~83% 不同）→ 服务端 whisper 对输入字节细微差异敏感。rodio 版本 whisper 陷入重复 loop（avg_logprob=-0.078 vs 正常 -0.50）→ worker 内部异常 → 500。证据文件：`/tmp/hypr-dump/client/group3.raw`(500) vs `/tmp/hypr-dump/g3-ffmpeg.raw`(200)、`client/g3a.raw`(200但garbage) vs `client/g3a-ffmpeg.raw`(200正常)。客户端重试 3 次无效（稳定复现）。**等服务端修复**；若长期不修，可加客户端防御：检测重复词比例/avg_logprob 异常高的响应 → 该组换 ffmpeg 解码重试 |

### Sprint 3 Phase D（录音流 diarization）— ✅ live 对齐 + 自适应修复完成，仅剩真机验证

录音流（`plugins/transcription/src/listener/runtime.rs`）已集成 `IncrementalDiarizationEngine`：
- 录音期间 VAD 段（`VadGroupStream.take_vad_segments`）实时 `feed_segments`（与 file 路径同一批 VAD 段）
- `finish()` 时补喂 flush 尾部 + `finalize()` 重聚类，按词中位时间 `speaker_at_time()` 标注 speaker
- "1 unique speakers" bug 已修复（Aug 1 Night：能量停顿切分 + 自适应 threshold + 最小时长过滤，5fdd76a7 验证 4 speakers）
- **live 与 file re-transcript 共用引擎核心 → 双路径自动获得修复，无需额外改动**
- 待验证：真实设备端到端（speaker 标签是否随 `SegmentResult` 正确显示）

### 近期待办（按优先级）

1. 真机端到端验证 live 录音 diarization（问题清单 #5）
2. 复测 Bug 2（batch target fallback，问题清单 #3）
3. 收集更多多说话人音频验证自适应 threshold 泛化 + 确认 c5ee333b 真实说话人数（#2/#7）
4. 长录音内存峰值实测（#6）
5. 服务端 whisper 字节敏感：等 speaches 修复（#8）；若长期不修，实现客户端防御（重复词/avg_logprob 检测 + 换 ffmpeg 解码重试）

### 已有但未落库的 diarization 改动（Jul 30-31，已随 Aug 1 commit 提交）

- `crates/pyannote-local/src/incremental_vad.rs`（新）— 有状态流式 VAD（同 segmentation.onnx 模型，跨 feed 保持状态）
- `crates/pyannote-local/src/incremental_diarization.rs`（新）— 流式引擎：feed_pcm / feed_segments / finalize / speaker_at_time / recluster
- `crates/pyannote-local/tests/find_threshold.rs`（新）— 真实音频 threshold sweep 工具
- `BatchParams` / `CaptureParams` / `SessionParams` / `ProgressiveBatchParams` — 新增 `diarization_enabled` / `diarization_model` / `diarization_threshold` 字段贯通
- `integration.rs` — VAD 先行（Segmenter）→ diarization 用 `feed_segments` 复用同一批 VAD 段 → 词级 `speaker_at_time` 标注 + `propagate_speaker_to_none` 前后向填充；大量 `[vad-batch]` / `[diarization]` 诊断日志
- 前端 `useStartListening` / `useRunBatch` — diarization 三参数贯通 + useRunBatch deps 修复
- `batch.ts` — DEV-only speaker/hint 调试日志
- threshold 默认 0.85（schema.ts / select.tsx / useRunBatch 三处一致）；engine 在默认值时走自适应估计（Aug 1 Night），显式调过滑块的值优先

### 远期优化方向

**VAD 静音压缩（实测后评估）：**
- 当前 Progressive Batch 提交每组 VAD 段时，组内各 VAD 段之间的静音间隙原封不动送至 Whisper，约 30-50% 的音频长度是无效计算。
- 优化思路：每组改用拼接 `Segment.samples`（纯人声片段，不含静音），Whisper 只需转写有效人声。
- 代价：Whisper 返回的词级时间戳对应压缩后的时间轴，需要在 Stitcher 里用 `global_start_ms` + 组内 gap map 展开回原始时间轴。Whisper 基于内容而非时间均匀分布，展开不是简单线性映射，需要逐字追踪。
- 建议：先用真实录音实测静音占比（如 10 条各 30s 的组），确认收益后再决定是否实现展开逻辑。

**`max_gap_ms`**：Merge 前置检查 VAD 段间 gap（如 >60s 硬分界），可做但收益有限（服务端已跳过静音）。

### 关键文件
- `docs/sprint-3-diarization-design.md` — 完整设计文档
- `crates/pyannote-local/tests/diarization_pipeline.rs` — 21 integration tests

## Misc

- Do not create summary docs or example code files unless requested.

- Do not create summary docs or example code files unless requested.
