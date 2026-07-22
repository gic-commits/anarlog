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

让 anarlog 通过 OpenAI provider 配置，接入自建 speaches 服务器（`wss://ztnas.2113111.xyz:28010`），实现**实时语音转录**（WebSocket Realtime API 模式），类似 Deepgram 的实时体验。

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

Phase 1 — 实时转录通路打通（当前）
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
  [x] Batch API 验证成功：curl 直接调返回正确中文
  [x] `initial_message` 握手（`session.update` 通过 WS 发送）
  [x] `prefix_padding_ms` 降级为 non-fatal warning
  [x] `hadLiveWords` batch fallback 参数

Phase 2 — 功能完善（当前）
  [ ] 推理速度优化 — server 端 whisper 4-8× 实时，需优化 CPU 线程或换 GPU
  [ ] 多语言实时转录支持
  [ ] Speaker diarization（说话人分离）
  [ ] 自定义 prompt/关键词实时生效

Phase 3 — UI 增强（按需）
  [ ] 如果 OpenAI provider + 自建服务器与原生 OpenAI 行为差异过大，加 toggle 开关
  [ ] 实时转录延迟/状态指示器

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

## Misc

- Do not create summary docs or example code files unless requested.
