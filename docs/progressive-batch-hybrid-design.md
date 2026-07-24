# Progressive Batch Hybrid — 设计文档

## 1. 背景

Speaches（faster-whisper-small）在 batch 模式下实现了 **RTF 0.47×**（处理 264.8s 音频仅需 124s）。消费速度首次超过生产速度，这为分段并行提交提供了数学条件。

当前两种模式各自的局限：

| 模式                      | 优点                     | 缺点                                   |
| ------------------------- | ------------------------ | -------------------------------------- |
| Live (Realtime WebSocket) | 低延迟，边录边出         | 受限于实时推理速度，长段累积延迟大     |
| Batch (HTTP POST)         | 服务端效率高，支持全语言 | 必须等录音结束才开始，长音频等待时间长 |

**目标：** 第三种模式 — **Progressive Batch**。将长录音在客户端切成多段，逐段提交 batch，客户端拼接结果。

---

## 2. 设计原则

1. **不破坏现有逻辑** — Live / Batch 模式完全不变，新用户需要主动选择；Progressive Batch 仅作用于新录音，不回溯已有段落
2. **客户端主导** — 分段、提交、拼接都在客户端完成，服务端无感知
3. **用户可选** — Provider 配置中增加 "Progressive Batch" 选项，与 Live / Batch 平级
4. **渐进可用** — 初始版本不依赖 WebSocket 控制面，纯 HTTP batch 即可运作
5. **阈值激活** — 录音超过 D 分钟（默认 3 分钟）才启用 Progressive Batch；短录音仍走标准 Batch，避免不必要的分段开销

---

## 3. 系统架构

```
┌────────────────────────────────────────┐
│            Anarlog Desktop              │
│                                         │
│  ┌─────────┐    ┌──────────────────┐   │
│  │ 录音引擎  │───▶  ProgressiveBatch │   │
│  │ (capture) │    │  Manager          │   │
│  └─────────┘    │                   │   │
│                 │  ┌─────────────┐  │   │
│                 │  │ Segmenter   │  │   │
│                 │  │ (VAD/固定)   │  │   │
│                 │  └──────┬──────┘  │   │
│                 │         │         │   │
│                 │  ┌──────▼──────┐  │   │
│                 │  │ Queue      │  │   │
│                 │  │ (并发 N)    │  │   │
│                 │  └──────┬──────┘  │   │
│                 │         │         │   │
│                 │  ┌──────▼──────┐  │   │
│                 │  │ Stitcher   │  │   │
│                 │  │ (拼接对齐)   │  │   │
│                 │  └─────────────┘  │   │
│                 └──────────────────┘   │
└──────────────────┬─────────────────────┘
                   │ HTTP POST (multipart)
                   ▼
┌────────────────────────────────────────┐
│         Speaches Server                │
│   POST /v1/audio/transcriptions        │
│   (无感知 — 与普通 batch 请求无异)       │
└────────────────────────────────────────┘
```

---

## 4. 核心组件

### 4.0 Activation（激活条件）

Progressive Batch 仅在录音时长远超 batch 推理时间时才有收益。初始阶段用阈值控制激活：

| 参数                | 默认值                       | 说明                                                                     |
| ------------------- | ---------------------------- | ------------------------------------------------------------------------ |
| `min_duration_secs` | `segment_duration_ms / 1000` | 录音未达到此值时直接走单次 batch 提交（当前实现：等于段长，≥1 段才启用） |

**判断时机（当前实现）：** 通过 `ProgressiveBatchManager` 的 `Accumulating` → `Active` 状态迁移实现。Manager 从 `total_duration()` 获知音频总长，若小于 `segment_duration_ms` 则直接走单次 `submit_file_direct`（原文件字节 POST）。若 ≥ 段长则流式解码 PCM（逐块 10s），`on_audio_frame` 馈送给 `Segmenter`。录音结束后 `finish()` 等待队列 drain 并 stitch。

**⚠️ 当前局限：** 仅用于 `run_progressive_batch_from_file`（从已有音频文件跑），未与实时录音 Source pipeline 集成。实时录音场景下，PCM 流尚未通过 Source actor 管道发送到 `ProgressiveBatchManager`。

**未来扩展：** 可在录音过程中做渐进式评估——如果录音进行到 30s 时已预计超过段长，提前启动分段器。

### 4.1 Segmenter（分段器）

**输入：** PCM 音频流（从 capture 引擎实时到达）
**输出：** `Vec<AudioSegment>`（每段包含 samples + 时间范围）

**分段策略（二选一，可配置）：**

| 策略                 | 优点                   | 缺点         |
| -------------------- | ---------------------- | ------------ |
| VAD 分段             | 段落语义完整           | 段长不可控   |
| 固定时长（默认 30s） | 段长均匀，便于估算进度 | 可能切断句子 |

**建议初始实现：** 固定 30s 分段 + **1s 段间重叠**（减少切句概率）。后续可升级到 VAD 分段。

> **服务端反馈：** batch 端点额外有一层 VAD（`stt.py:154`），会在客户端分段之上再跑 VAD 检测静音并生成 `clip_timestamps`。这意味着：
>
> 1. 固定 30s 分段后，服务端 VAD 会进一步裁剪段内静音 — 客户端拼接时的时间轴校准需考虑
> 2. 重叠分段的两段中，静音部分会被 VAD 跳过，但**语音部分会重复转录** — stitcher 需用时间戳去重
> 3. 分段越大效率越高（HTTP 开销占比更小），建议允许用户配置 `segment_duration_ms`（60s 比 30s 更优）

```
固定 30s + 1s overlap:
[0-30s] [29-59s] [58-88s] ... [N-N+30s]
  ↑ overlap 1s → 服务端 VAD 跳过静音，stitcher 按时间戳去重
    每个 k 段起始时间: k × (segment_ms − overlap_ms) = k × 29s

预期耗时（4min 音频，~8 段，N=2 并发，RTF 0.47×）：
  录音结束 = 240s
  最后一段提交后 ~14s 出结果（30s × 0.47）
  总等结果时间 = 240s + 14s = 254s
  对比单次 batch = 240s + 124s = 364s  ✅ 提升 ~30%

  > 注：最后一段等 14s 是因为最后一段长 30s，RTF 0.47× = 14.1s 处理。
  > 前 7 段在录音过程中就已提交并完成，用户无需额外等待。
  > 实际用户体验：录音结束后约 14s 看到完整结果（vs 标准 batch 124s）。
```

### 4.2 Queue（提交队列）

**职责：**

- 管理 N 个并发 HTTP batch 请求（N 默认 2，可配置）
- 排队等待中的分段
- 重试失败的提交（max_retries=3）
- 进度跟踪

**状态机：**

```
Pending → InFlight → Completed
                  → Failed → Pending (retry)
                  → Failed → Failed (exhausted)
```

**并发控制：** N 个并发段同时提交。

> **服务端反馈：** batch 端点**不受信号量限制**（Semaphore 仅限 live 模式）。但 N100 服务器仅 4 核，`WHISPER__CPU_THREADS=2`、`WHISPER__NUM_WORKERS=2`，N=2 已是 CPU 上限。建议 `max_concurrency` 默认值 2，硬上限 2。

### 4.3 Stitcher（拼接器）

**输入：** 多个 `BatchResponse`（每段一个）
**输出：** 合并后的完整转录 + 对齐的时间轴

**拼接规则：**

- 按时间顺序排列段落
- segment 边界直接衔接（无重叠）
- word 级别时间戳：保持每个 word 的原始时间偏移 + 段落偏移
- `provider_segment_index` 保持各段独立，不跨段合并（已有 `propagate_identity` 边界检查）

**数据格式（长音频 ≥ 段长）：** PCM f32 样本在提交时直接重采样为 16000 Hz s16le，以 `audio/pcm` Content-Type 和 `audio.raw` 文件名 POST。不做 WAV 封装，不写临时文件。
**数据格式（短音频 < 段长）：** 原始文件字节 + 扩展名判 MIME 直接 POST（复用 Direct Batch 方式）。

**时间戳全局对齐：**

每个分段的服务端响应时间戳是**相对于该分段起始位置**的偏移量。stitcher 需要将每段的偏移量加上该段的全局起始时间，才能得到全局时间轴上的正确位置：

```
Segment 2: 全局 [29s–60s)
  └─ 服务端返回 word.start = [0.5, 1.2, ...]
  └─ 全局对齐后 word.start = [29.5, 30.2, ...]

Segment 3: 全局 [59s–90s)
  └─ 服务端返回 word.start = [0.3, 0.8, ...]
  └─ 全局对齐后 word.start = [59.3, 59.8, ...]
```

对齐时机：在 stitcher 收到每个分段结果后立即应用段偏移量，后续排序和去重全都基于全局时间戳。

**关于 1s overlap 的去重策略：**

由于服务端额外 VAD 会跳过静音，重叠段中只有语音部分会被重复转录。stitcher 需要去重。

**选择：segment 级去重（而非 word 级）**

word 级时间戳需要 `timestamp_granularities[]=word`，实测增加约 20% 服务端开销（124s → ~150s）。去重粒度为 segment 级更简单也更省：

1. 按 `segment.start` 排序所有段落
2. 如果两段的 start 时间差 < 1s（overlap 窗口），保留先到达的段落
3. 同一段内部的 word 保留原样（服务端已校准）

> **服务端反馈建议：** 初始用 30s + 1s overlap，segment 级去重即可。不需要额外服务端改动。

**边界情况处理：**

| 情况                                             | 处理方式                                                              |
| ------------------------------------------------ | --------------------------------------------------------------------- |
| 段间重叠（语音被重复转录）                       | **segment 级去重**：按 segment.start 对齐，overlap 窗口内的重复段丢弃 |
| 段间有间隙（静音被截断）                         | 保留间隙，时间戳连续                                                  |
| 段提交乱序到达（网络延迟）                       | 按 start 时间排序后再拼接                                             |
| 服务端 VAD 返回的 segment 与客户端分段边界不对齐 | stitcher 以 segment.start 为准，不依赖客户端分段边界                  |

---

## 5. 与现有系统的关系

### 5.1 代码结构

建议不修改现有 `listener2-core` 的 live/batch 路径，新增独立模块：

```
crates/
  listener2-core/
    src/
      batch/
        progressive/       ← 已有（whispercpp 渐进式批处理）
          mod.rs
          scheduler.rs
          collector.rs
          ...
      progressive-batch/    ← 新增（通用 HTTP 渐进式批处理）
        mod.rs              — ProgressiveBatchManager
        segmenter.rs        — 分段策略
        queue.rs            — 提交队列 + 并发控制
        stitcher.rs         — 结果拼接
```

### 5.2 已有 Progressive Batch 的复用

当前 `crates/listener2-core/src/batch/progressive/` 是给本地 whispercpp / Argmax 用的，其核心模式（分段→并发提交→拼接）与通用 HTTP batch 相似。可以抽取共用抽象，但 HTTP batch 的提交逻辑不同（multipart POST vs internal server IPC）。

建议：新模块独立实现，未来再考虑抽象公共调度层。

### 5.3 Provider 配置

在 `apps/desktop/src/settings/ai/stt/select.tsx` 中，Provider 详情里增加模式选择：

```
OpenAI (Speaches)
├─ Model: faster-whisper-small
├─ Mode: [Live] [Batch] [Progressive Batch]  ← 新增
└─ Concurrent segments: [2]                    ← 可选
```

---

## 6. 可选扩展：WebSocket 控制面

### 6.1 动机

目前纯 HTTP batch 的方案已经可行。batch 端点无信号量限制，N=2 并发直接可用。控制面扩展仅在以下场景有价值：

- 需要精确感知服务端处理进度的场景
- 未来 batch 端点也引入并发限制时

### 6.2 方案

利用已经打通的 Realtime WebSocket 连接作为轻量控制面：

```
控制面（WebSocket）:
  ── 客户端 → 服务端: submit segment (id, duration)
  ── 服务端 → 客户端: ack (id, queue_position)
  ── 服务端 → 客户端: complete (id, result_url)

数据面（HTTP）:
  ── 客户端 → 服务端: POST /v1/audio/transcriptions (multipart)
```

### 6.3 优缺点

|            | 纯 HTTP batch        | + WebSocket 控制面      |
| ---------- | -------------------- | ----------------------- |
| 实现复杂度 | 低                   | 高（需服务端配合）      |
| 进度感知   | 只能猜测             | 精确感知                |
| 服务端改动 | 无                   | 需要 WebSocket 消息扩展 |
| 通用性     | 任何 OpenAI 兼容 API | 仅 Speaches             |

### 6.4 建议

**Phase 1：纯 HTTP batch** — 不做控制面，客户端简单并发 N=2，用超时和重试兜底
**Phase 2：控制面（可选）** — 如果纯 HTTP 模式遇到瓶颈，再引入 WebSocket 控制面

---

## 7. 开/关选项

| 参数                  | 默认值                       | 说明                                                        |
| --------------------- | ---------------------------- | ----------------------------------------------------------- |
| `mode`                | `"live"`                     | `"live"` \| `"batch"` \| `"progressive"`                    |
| `min_duration_secs`   | `segment_duration_ms / 1000` | 录音时长阈值（秒），当前实现恒等于段长；≥1 段才启用分段提交 |
| `max_concurrency`     | `2`                          | 最大并行分段数（硬上限 2）                                  |
| `segment_duration_ms` | `30000`                      | 分段时长（ms，建议可选 30000/60000）                        |
| `segment_overlap_ms`  | `1000`                       | 段间重叠时长（ms）                                          |
| `segment_strategy`    | `"fixed"`                    | `"fixed"` \| `"vad"`                                        |
| `max_retries`         | `3`                          | 单段最大重试次数                                            |

---

## 8. 风险评估

| 风险                                           | 概率   | 影响                        | 缓解措施                                                                            |
| ---------------------------------------------- | ------ | --------------------------- | ----------------------------------------------------------------------------------- |
| 分段边界切断语义完整的句子                     | 高     | 中                          | 1s overlap 减少切句概率；stitcher 去重                                              |
| 重叠段语音被重复转录                           | 高     | 低（仅多余 text，不丢内容） | stitcher 基于 segment.start 去重                                                    |
| 多段并发导致服务端过载                         | 低     | 中                          | batch 端点无信号量限制，但 N=2 硬上限匹配 CPU 上限                                  |
| 段间拼接产生时间轴跳跃                         | 低     | 中                          | 充分测试；stitcher 做 gap 检测                                                      |
| 服务端 VAD 输出与客户端分段边界不对齐          | 中     | 低                          | stitcher 以 segment.start 为准，时间戳已全局对齐                                    |
| **分段时间戳未加全局偏移，导致 UI 时间轴错位** | **高** | **高**                      | 每段落地时立即做 `offset = segment_global_start + response_timestamp`，测试用例覆盖 |
| 用户困惑于三种模式的选择                       | 中     | 低                          | UI 加 tooltip 解释；推荐默认值                                                      |

---

## 9. 实施计划

### Sprint 1：基础框架

- [x] 创建 `listener2-core/src/batch/progressive_batch/` 模块（Rust 实际使用下划线，设计文档的 `progressive-batch/` 为路径表示）
- [x] 实现激活条件判断：`ProgressiveBatchManager` 的 `Accumulating` → `Active` 状态迁移（Manager.finish() 在阈值前降级为单次 batch）
- [x] 实现固定时长分段器（默认 30s + 1s overlap，Segmenter 支持配置 `segment_duration_ms`）
- [x] PCM raw → WAV header 转换（`write_wav` 函数在 mod.rs，使用 hound crate 写 i16 WAV）
- [x] 实现提交队列（`BatchQueue`，N=2 硬上限，重试 3 次，`SubmitSegmentFn` 可配置 HTTP 客户端）
- [x] 实现基础拼接器（`Stitcher`，segment 级去重 + word 级全局时间戳偏移）
- [x] Provider 配置：新增 mode 选择（Live / Batch / Progressive）— `stt_mode` 配置项，前端 `select.tsx` UI

### Sprint 2：稳定性

- [x] 1s overlap 去重逻辑单元测试（`stitcher.rs` 含 15 个测试，覆盖重叠、间隙、乱序、边界）
- [x] 服务端双 VAD 场景的时间轴校准测试（Manager 集成测试含中英文字和时间轴验证）
- [x] 边界情况测试（间隙、乱序、失败恢复、重试耗尽 — `queue.rs` 17 个测试含 retry exhaustion）
- [ ] 进度事件 → UI 进度条（数据模型就绪 `QueueProgress`，前端未接入）
- [ ] live session PCM 流集成（`ProgressiveBatchManager.on_audio_frame` 未连接到 Source pipeline）

### Sprint 3：优化（可选）

- [ ] VAD 分段策略
- [ ] 动态 segment_duration_ms 自适应
- [ ] WebSocket 控制面设计评审（如需要）

---

## 10. 实现与设计的关键差异

| 领域                             | 设计文档                                            | 实际实现                                                            | 状态                           |
| -------------------------------- | --------------------------------------------------- | ------------------------------------------------------------------- | ------------------------------ |
| 模块命名                         | `progressive-batch/`                                | `progressive_batch/`（Rust 模块名规则）                             | ⚠️ 无实质影响                  |
| PCM 流集成                       | 通过 Source pipeline 实时馈送                       | 仅从文件读取（`run_progressive_batch_from_file`）                   | ❌ 未实现                      |
| `effective_transcription_mode()` | 应处理 `ProgressiveBatch`                           | 只识别 `Batch`，`ProgressiveBatch` → 回退 `Live`                    | ❌ 未处理                      |
| `SessionArgs.pcm_tx`             | 新增 `progressive_batch_pcm_tx` 字段                | 不存在                                                              | ❌ 未实现                      |
| listener2 集成                   | `Listener2` 持有 `ProgressiveBatchManager` map      | 直接通过 `run_batch_inner` 路由                                     | ✅ 简化方案                    |
| startTranscription 快捷返回      | 检查已缓存结果即时返回                              | 去重路径调用 `run_progressive_batch_from_file`                      | ✅ 兼容现有流程                |
| 前端 progress 事件               | v2 可选，`TranscriptionEvent::progressive_progress` | 未实现                                                              | ❌ 未实现                      |
| segment_overlap_ms 可见配置      | 用户可配置                                          | 硬编码 1000ms                                                       | ❌ 未配置化                    |
| max_retries 可见配置             | 用户可配置                                          | 硬编码 3                                                            | ❌ 未配置化                    |
| v2 持久化（DB 表）               | `progressive_batch_jobs/segments`                   | 未实现                                                              | ❌ 未实现                      |
| PCM POST 格式（长音频）          | WAV header + audio/wav                              | audio/pcm raw in-memory                                             | ✅ 节约磁盘 I/O                |
| 内存峰值（3h 音频）              | 无所谓（设计时未评估）                              | ~15MB（流式 10s chunks）                                            | ✅ 显著降低                    |
| `min_duration_secs` 取值         | 硬编码 180                                          | = `segment_duration_ms / 1000`                                      | ✅ 更灵活                      |
| URL 构造                         | 字符串拼接                                          | `url::Url::path_segments_mut()`                                     | ✅ 避免双 `/v1/`               |
| response 解析                    | `serde_json::from_str`                              | OpenAI → `OpenAIAdapter::parse_batch_response`，其余 → `serde_json` | ✅ 兼容 Speaches null logprobs |

---

## 附录

### A. Speaches（第三方服务）编码接口约定

Live（WebSocket）路径 `audio.py:64`：

```
sf.read(file, format="RAW", channels=1, ...)
```

- **硬编码 single channel**，多声道传过来直接解析出错或只读第一声道。

Batch 路径 `dependencies.py:94-98`：

```
if file.content_type in ("audio/pcm", "audio/raw"):
    audio_int16 = np.frombuffer(raw_bytes, dtype=np.int16)
```

- `Content-Type: audio/pcm` → 直接解析为 int16 数组，默认单声道
- 其他格式 → ffmpeg 自动混音为单声道

**客户端建议**：capture 引擎输出时直接 `(L+R)/2` 混音为 mono s16le，下游不再关心声道问题。

---

### B. Direct Batch 原生模式的编码处理

`listener2-core/src/batch/mod.rs:173` 起点：

```
run_batch_inner(runtime, params)
  → hypr_audio_utils::audio_file_metadata(path)
    → rodio::Decoder (symphonia 后端)
    → 返回 sample_rate + channels（不读完整 PCM）
  → 分发到对应 adapter:
    ├─ Soniqo   → transcribe_soniqo_file       → 读 PCM，写临时 WAV，走本地 SDK
    ├─ Argmax   → run_progressive_batch_session → PCM 分段 + 流式提交
    ├─ OpenAI   → run_direct_batch::<OpenAIAdapter>
    └─ Deepgram → run_direct_batch::<DeepgramAdapter>
```

`run_direct_batch` 的统一做法 (`owhisper-client/src/adapter/http.rs`):

```
streaming_file_part(file_path)
  → tokio::fs::File::open              ← 读原始文件字节
  → mime_type_from_extension(path)     ← 判 MIME：.mp3→audio/mpeg, .wav→audio/wav
  → POST multipart/form-data           ← 原始字节 + 正确 Content-Type
  → 服务端自行解码                    ← 客户端零编码
```

**关键特征**：文件字节原样发送，客户端不做任何 PCM 转换。服务器（Speaches / OpenAI / Deepgram）各自处理格式解码。

对比 ProgressiveBatch 短音频路径（本次修复前）：

|           | Direct Batch        | ProgressiveBatch（短音频 < 段长）    | ProgressiveBatch（长音频 ≥ 段长）  |
| --------- | ------------------- | ------------------------------------ | ---------------------------------- |
| 文件读取  | 原始字节            | `rodio::source_from_path` → 通用解码 | `rodio` 流式逐块解码（10s chunks） |
| PCM 转换  | 无                  | 无                                   | 重采样 16000 Hz + f32→s16le        |
| POST 格式 | 原文件扩展名判 MIME | 原文件扩展名判 MIME                  | `audio/pcm`（in-memory）           |
| 多声道    | 服务端处理          | 服务端处理                           | 客户端混音为 mono                  |
