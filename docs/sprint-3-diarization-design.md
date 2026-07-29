# Sprint 3 — 本地说话人分离 (Speaker Diarization)

## 1. 概述

在 batch 转录前对原始音频做 **说话人分离预处理（染色）**，让转录结果带上 `speaker` 标签，实现类似 "Speaker 0: ... \nSpeaker 1: ..." 的分段展示。

## 2. 最终 Pipeline

```
原始音频 (PCM f32/i16, 16kHz)
  │
  ├─ VAD (pyannote Segmenter 5.7MB ONNX)
  │    ─ 输出: Vec<Segment {start, end, samples}>
  │
  ├─ 短段合并
  │    ─ <1.5s 合并到相邻段
  │
  ├─ Embedding (wespeaker-cnceleb-LM 25MB ONNX, 256-dim)
  │    ─ 每段提取 256-dim speaker embedding
  │    ─ 短段(>=1.5s)跳过，标记为继承相邻段
  │
  ├─ Clustering (average-linkage agglomerative)
  │    ─ cosine distance matrix → threshold=0.35
  │    ─ 输出说话人标签
  │
  ├─ [NEW] 时长调度 (Merge/Split)
  │    ─ 以 max_duration=30s 为参考，±20% 水线
  │    ─ 合并短 VAD 段 → 接近 max_duration
  │    ─ 超限强制切分
  │
  ├─ SubmitSegment 队列
  │    ─ 每个 segment 携带本地元数据 (speaker, time, batch_id)
  │    ─ 仅发送纯标准 OpenAI 请求到服务端
  │
  └─ 响应匹配
       ─ 按提交顺序匹配回 SubmitSegment
       ─ 同 speaker 相邻段合并显示
       ─ 不同 speaker 段之间加分隔
```

## 3. 模型选择（已完成对比验证）

### 3.1 最终决策

| 角色 | 模型 | Dim | 来源 | 说明 |
|------|------|-----|------|------|
| **默认** ⭐ | `wespeaker_zh_cnceleb_resnet34_LM.onnx` | 256 | [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) | CN-Celeb 中文训练 + Large Margin 微调 |
| **备选** | `campplus_cn_en_common_200k.onnx` | 192 | [welcomyou HF](https://huggingface.co/welcomyou/campplus-3dspeaker-200k-onnx) | 200k 说话人，速度最快+9% |

后续通过设置界面让用户选择。

### 3.2 中文 4-speaker 测试 (SOND)

| 模型 | 平均跨说话人距离 | t=0.35 聚类 |
|------|:--------------:|:----------:|
| **wespeaker-cnceleb-LM** | **0.8803** | ✅ 4/4 |
| wespeaker-voxceleb | 0.8595 | ✅ 4/4 |
| wespeaker-cnceleb | 0.8456 | ✅ 4/4 |
| campplus-200k | 0.7816 | ✅ 4/4 |

### 3.3 性能基准 (85s 中文音频, 7 segments)

| 模型 | Embed 时间 | 总计 | 相对速度 |
|------|:---------:|:----:|:-------:|
| **campplus-200k** | **3.71s** | **5.69s** | **1.0×** |
| wespeaker-cnceleb | 7.23s | 9.21s | 1.62× |
| wespeaker-cnceleb-LM | 7.04s | 9.02s | 1.58× |
| wespeaker-voxceleb | 8.24s | 10.22s | 1.80× |

> 以上数据已包含模型加载（一次性），Embed 时间为逐段推理的累加。
> Sprint 3 正式开发时可优化：batching、线程数调优、warmup 推理。

### 3.4 下载

```bash
# wespeaker-cnceleb-LM (默认)
curl -L -o tests/models/wespeaker_zh_cnceleb_resnet34_LM.onnx \
  https://hf-mirror.com/csukuangfj/speaker-embedding-models/resolve/main/wespeaker_zh_cnceleb_resnet34_LM.onnx

# campplus-200k (备选)
curl -L -o tests/models/campplus_cn_en_common_200k.onnx \
  https://hf-mirror.com/welcomyou/campplus-3dspeaker-200k-onnx/resolve/main/campplus_cn_en_common_200k.onnx
```

裸测代码：`crates/pyannote-local/tests/diarization_pipeline.rs` (19 tests, all ✅)

## 4. 时长调度（VAD + 固定时长调和）

### 4.1 方案选择：方案A

```
VAD → 染色 → 合并/切分 → 提交
```

不采用"固定分段 → 段内独立 VAD"（方案B），因为方案B会导致同说话人跨段时 Speaker_id 不一致。

### 4.2 ±20% 水线规则

以 `max_duration = 30s` 为例：

| 累计时长 | VAD 状态 | 行为 |
|:-------:|:--------:|------|
| < 24s (80%) | 段已结束 | 合并下一段 |
| 24s ~ 30s (80%~100%) | 段已结束 | ✅ **立即提交**（提前命中） |
| 30s ~ 36s (100%~120%) | 段已结束 | ✅ **立即提交**（允许超限） |
| **> 36s (120%)** | **段未结束** | ⚠️ **强制切分**：按 100% 切，溢出部分留到下一批 |

### 4.3 合并策略

```
pending_queue: Vec<SpeakerSegment>  // 待提交段

for each segment from VAD:
    push to pending_queue
    
    total = sum(seg.end - seg.start for seg in pending_queue)
    
    if segment just ended AND total >= max_duration * 0.8:
        if total <= max_duration * 1.2:
            submit_batch(pending_queue)  // 提前命中或允许超限
            pending_queue.clear()
        elif total > max_duration * 1.2:
            // 强制切分：切到 100%
            split_point = find_split_time(pending_queue, max_duration)
            submit_batch(pending_queue[..split_point])
            pending_queue = pending_queue[split_point..]
    elif segment not ended AND total > max_duration * 1.2:
        // 等不到段结束了，强制切
        split_point = find_split_time(pending_queue, max_duration)
        submit_batch(pending_queue[..split_point])
        pending_queue = pending_queue[split_point..]
```

### 4.4 短段处理

VAD 产生的超短段（<1.5s）：
1. 嵌入提取会失败 → 标记为"无嵌入"
2. 合并到相邻最近的有效段（短段合并）
3. 不单独提交

## 5. 本地元数据 + 服务端纯净协议

### 5.1 核心原则

**所有说话人/时序信息在客户端本地管理，不发送给服务端。**

服务端收到的是标准的 OpenAI `/v1/audio/transcriptions` 请求：
```
POST /v1/audio/transcriptions
Content-Type: multipart/form-data

model: whisper-1
audio: <binary PCM/WAV>
response_format: verbose_json
timestamp_granularities: word
language: zh
```

服务端返回标准的 `CreateTranscriptionResponse`（words[]），不含任何自定义字段。

### 5.2 SubmitSegment

```rust
struct SubmitSegment {
    // 发送给服务端的数据
    audio: Vec<i16>,           // PCM 16kHz s16le
    
    // 本地元数据（不发送）
    start_time: f64,
    end_time: f64,
    speaker: usize,            // cluster ID
    segment_index: usize,      // 在原始音频中的顺序
    batch_id: String,          // 所属批次 ID
    original_offset: usize,    // 在原始音频中的采样点偏移
}
```

### 5.3 响应匹配

```
提交队列: [seg0(speaker=0), seg1(speaker=0), seg2(speaker=1)]
               ↓                    ↓              ↓
响应队列: [resp0(words[]), resp1(words[]), resp2(words[])]
               ↓                    ↓              ↓
stitch: 每个 word 带上 speaker = submit_queue[i].speaker

拼接规则：
- 同 speaker 相邻段 → words 合并成同一显示段落
- 不同 speaker → 中间加分隔标记 + speaker 标签
```

### 5.4 持久化

参考 Progressive Batch 的设计，`SubmitSegment` 队列持久化到 DB：

```sql
CREATE TABLE diarization_submit_segments (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    batch_id TEXT NOT NULL,
    segment_index INTEGER NOT NULL,
    start_time REAL NOT NULL,
    end_time REAL NOT NULL,
    speaker INTEGER NOT NULL,
    audio_offset INTEGER NOT NULL,
    audio_length INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending | submitted | done
    created_at TEXT NOT NULL
);
```

支持断线恢复：
- 重启后加载 `pending`/`submitted` 状态的 segment
- `submitted` 但未收到响应的 → 重新匹配
- `done` 的 → 跳过

## 6. 组件设计

### 6.1 DiarizationManager

```rust
struct DiarizationConfig {
    model: DiarizationModel,     // 默认 wespeaker-cnceleb-LM
    threshold: f32,              // 聚类阈值，默认 0.35
    min_segment_duration_s: f32, // 最短有效段，默认 1.5
    sample_rate: u32,            // 16000
}

enum DiarizationModel {
    WespeakerCncelebLM,
    Campplus200k,
    // 后续可扩展
}

struct SpeakerSegment {
    start: f64,
    end: f64,
    speaker: usize,
    embedding_valid: bool,  // false = 合并过来的短段
}

struct DiarizationResult {
    segments: Vec<SpeakerSegment>,
    n_speakers: usize,
    model: DiarizationModel,
}

impl DiarizationManager {
    fn new(config: DiarizationConfig) -> Self;
    fn process(&mut self, audio_f32: &[f32]) -> Result<DiarizationResult>;
}
```

### 6.2 DurationScheduler（新增）

```rust
struct DurationScheduler {
    max_duration_ms: u32,     // 默认 30000
    watermark_low: f64,       // 0.8
    watermark_high: f64,      // 1.2
}

impl DurationScheduler {
    fn new(max_duration_ms: u32) -> Self;
    
    // 判断当前 pending 队列是否可以提交
    fn should_submit(&self, pending: &[SpeakerSegment], 
                     last_segment_ended: bool) -> SubmitDecision;
    
    // 需要切分时，找到切分点
    fn find_split(&self, pending: &[SpeakerSegment]) -> usize;
}

enum SubmitDecision {
    Wait,                       // 继续合并
    Submit(usize),              // 提交前 N 个段
    SplitAndSubmit(usize),      // 切到第 N 个，提交前半
}
```

### 6.3 SubmitManager（新增）

```rust
struct SubmitSegment {
    audio: Vec<i16>,
    start_time: f64,
    end_time: f64,
    speaker: usize,
    segment_index: usize,
    batch_id: String,
    status: SubmitStatus,
}

enum SubmitStatus {
    Pending,
    Submitted { request_id: String },
    Done,
}

impl SubmitManager {
    fn enqueue(&mut self, segments: Vec<SubmitSegment>);
    fn submit_next_batch(&mut self) -> Result<()>;
    fn match_response(&mut self, response: TranscriptionResponse) -> Result<Vec<WordWithSpeaker>>;
    fn recover_from_db(&mut self, session_id: &str) -> Result<()>;
}

struct WordWithSpeaker {
    word: String,
    start: f64,
    end: f64,
    speaker: usize,
}
```

## 7. 集成路径

### 7.1 集成点：Batch 提交前

```
run_batch / run_progressive_batch
  │
  ├─ [NEW] DiarizationManager::process(audio_f32)
  │     → Vec<SpeakerSegment> (已染色)
  │
  ├─ [NEW] DurationScheduler
  │     → 合并/切分 SubmitSegment 队列
  │
  ├─ [NEW] SubmitManager::submit_next_batch()
  │     → 纯标准 OpenAI 请求
  │
  └─ [NEW] SubmitManager::match_response()
       → enriched WordsWithSpeaker
```

### 7.2 非染色模式（直通）

当 `diarization_enabled = false` 时，走现有逻辑：
```
原始音频 → 固定时长分段 → 直接提交（无 speaker 元数据）
```

## 8. 数据流

```
Settings → diarization_enabled: bool
         → diarization_model: "wespeaker-cnceleb-LM" | "campplus-200k"
         → diarization_threshold: f32 (0.05-0.50)
             ↓
TranscriptionParams (plugins/transcription/api.rs)
             ↓
BatchParams (listener2-core/batch/mod.rs)
             ↓
run_batch() / run_progressive_batch()
             ↓
DiarizationManager::process(audio_f32)
  ├─ Segmenter (VAD)
  ├─ EmbeddingExtractor (wespeaker-cnceleb-LM / campplus-200k)
  ├─ Agglomerative Clustering (threshold)
  └─ short segment merge
             ↓
DurationScheduler (merge/split ±20%)
             ↓
SubmitManager
  ├─ enqueue(SubmitSegment)
  ├─ submit_next_batch() → pure OpenAI API call
  └─ match_response() → WordsWithSpeaker
             ↓
Stitcher → UI display (speaker labels + colors)
```

## 9. Phase 分解

### Phase A: DiarizationManager + 聚类 + 短段合并（~2天）

- [ ] 实现真实 agglomerative clustering（替换 `embedding.rs` 中的 `cluster()` 桩）
- [ ] 实现 `DiarizationManager`，封装 seg→embed→cluster 管线
- [ ] 实现 `EmbeddingProvider` trait（wespeaker-cnceleb-LM + campplus-200k）
- [ ] 短段合并（<1.5s 合并到相邻段）
- [ ] 全部测试通过

### Phase B: 时长调度 + SubmitManager + DB（~1天 ✅）

- [x] 实现 `DurationScheduler`（±20% 水线规则）
- [x] 实现 `SubmitManager`（`DiarizationSubmitter`: 队列 + N=2 并发 + 指数退避重试 + drain 超时 + 响应匹配）
- [x] DB 持久化 schema + 迁移（`diarization_jobs` / `diarization_segments` 表）
- [x] Drizzle schema（`diarizationJobs` / `diarizationSegments`）
- [x] Rust 行类型（`DiarizationJobRow` / `DiarizationSegmentRow`）
- [x] `persist_batch_event` 持久化 handler
- [x] `BatchEvent::DiarizationStarted` + `DiarizationSegmentResult` 变体
- [x] `TranscriptionEvent::from` 映射
- [x] `BatchParams` 字段贯通（`diarization_enabled` / `diarization_model` / `diarization_threshold`）
- [x] 全部测试通过（listener2-core 115/115, db-app 44/44, pyannote-local 36/36）

### Phase C: UI（~1天）

- [ ] Settings 页面新增 Diarization toggle + model 选择 + threshold slider
- [ ] Segment 渲染添加 speaker 标签 + 颜色
- [ ] CJK 后处理兼容 diarization（保留 speaker 标签）

## 10. 开放问题

1. **阈值自适应**: 是否可以根据 embedding 距离分布自动计算最优阈值？

2. **说话人数预设**: 是否允许用户指定 speaker count？如果已知是 2-person 对话，可固定 n_clusters=2。

3. **推理优化**: batching（多段合并为一次 ONNX 调用）、线程数调优、warmup 推理 — 留到 Sprint 3 正式开发

4. **CJK 交互**: 先染色再 CJK 后处理，speaker 变更处的 gap 是否需要插入分段标记？

5. **Caching**: 同一音频重复转录（如阈值调整后重试）是否缓存 diarization 结果？

## 11. 裸测验证数据

### 11.1 模型对比总表

| 模型 | Dim | F-M 分离度 | 中文 4-spk avg | 85s Embed 时间 |
|------|-----|:---------:|:-------------:|:------------:|
| wespeaker-cnceleb-LM | 256 | 0.7301 | **0.8803** | 7.04s |
| wespeaker-voxceleb | 256 | 0.7862 | 0.8595 | 8.24s |
| wespeaker-cnceleb | 256 | 0.6914 | 0.8456 | 7.23s |
| campplus-200k | 192 | **0.8533** | 0.7816 | **3.71s** |
| pyannote-local | 512 | 0.0995 | — | — |

### 11.2 中文 SOND 4-speaker 距离矩阵 (wespeaker-cnceleb-LM)

```
            spk1    spk2    spk3    spk4
spk1.wav   0.0000  0.9698  0.8271  0.9534
spk2.wav   0.9698  0.0000  0.9053  0.7229
spk3.wav   0.8271  0.9053  0.0000  0.9032
spk4.wav   0.9534  0.7229  0.9032  0.0000

平均跨说话人距离: 0.8803
t=0.35 → 4 clusters ✅ (GT=4)
```

裸测代码：`crates/pyannote-local/tests/diarization_pipeline.rs`（19 tests, all ✅）
