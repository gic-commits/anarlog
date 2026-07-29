# Progressive Batch — 数据结构与数据库设计

## 1. 设计概览

### 核心决策

| 决策       | v1 方案                                                         | v2 方向                                                         |
| ---------- | --------------------------------------------------------------- | --------------------------------------------------------------- |
| 持久化     | 不新增 DB 表，段状态全在内存                                    | 新增 `progressive_batch_jobs` + `progressive_batch_segments` 表 |
| 分段时机   | 录音过程中实时分段（PCM 流切分）                                | 同左                                                            |
| 监听器模式 | 新增 `TranscriptionMode::ProgressiveBatch`                      | 同左                                                            |
| PCM 接入点 | Source 管道 → 可选 channel → ProgressiveBatchManager            | 同左                                                            |
| 前端交互   | 不变 — 走相同 `runBatch` + `TranscriptionEvent::Completed` 路径 | 增加进度事件                                                    |
| 结果返回   | Manager 内存缓存，`startTranscription` 立即/短等返回            | 同左                                                            |
| 临时文件   | 无（PCM 分段在内存中直接 POST `audio/pcm`）                     | 可写恢复文件                                                    |

### v1 约束

- 不改前端代码（`useRunBatch.ts` / `queries.ts` / `batch.ts`）
- 不改现有 `BatchProvider` 枚举（复用 `BatchProvider::OpenAI` 路径）
- 不改 `BatchRuntime` / `BatchEvent` 事件类型（复用现有事件）
- 段状态丢失 = 重跑（crash 时录音文件仍在，可 fallback 标准 batch）

---

## 2. 数据流

```
[Audio File]
  → rodio 流式解码（10s chunks）
  → Source.collect() 已废弃（内存 4GB → ~15MB）
  → on_audio_frame(&mono_pcm)
      ↓
  ProgressiveBatchManager
      │  PCM 累积（流式 10s chunks）
      │  ↓ (30s 边界)
      │  Segmenter → AudioSegment (samples in memory)
      │  └──→ Queue.enqueue(index, samples, sample_rate)
      │           ↓ N=2 并发
      │  submit_segment_http → POST audio/pcm (16000 Hz s16le)
      │           ↓
      │  response → Stitcher.add_segment(index, response)
      │           ↓ (poll_completed 循环)
      │  runtime.emit(BatchSegmentResult { session_id, segment_index, response })
      │           ↓ (实时发送给前端)
      │  [前端] handleBatchSegmentResult → batchSegments Map
      │           ↓ SegmentPreview 展示
      │
      │  (所有段完成)
      │  Stitcher.stitch() → batch::Response + segment_boundaries
      │           ↓
      │  runtime.emit(TranscriptionEvent::Completed)
      │           ↓
      │  前端渲染完整结果 + 虚线分隔（segment_boundaries 标记）
      │
      录音结束时: finish() → drain → stitch → emit
```

---

## 3. DB Schema

### v1: 无新增表

充分利用现有表：

```sql
-- sessions.metadata_json 记录模式
UPDATE sessions SET metadata_json = json_set(metadata_json,
  '$.transcription_mode', 'progressive_batch',
  '$.progressive_config', json_object(
    'segment_duration_ms', 30000,
    'overlap_ms', 1000,
    'max_concurrency', 2
  )
) WHERE id = ?;

-- transcripts 记录最终结果（同标准 Batch）
-- 无变化 —— words_json 已包含所有段的全局时间戳词
```

### v2: 新增持久化表

```sql
-- progressive_batch_jobs: 每个 session 一个 job
CREATE TABLE IF NOT EXISTS progressive_batch_jobs (
    id                  TEXT PRIMARY KEY NOT NULL,
    session_id          TEXT NOT NULL REFERENCES sessions(id),
    status              TEXT NOT NULL DEFAULT 'running',
                        -- 'running' | 'completed' | 'partial' | 'interrupted' | 'failed'
    provider            TEXT NOT NULL DEFAULT '',
    model               TEXT NOT NULL DEFAULT '',
    base_url            TEXT NOT NULL DEFAULT '',
    language            TEXT NOT NULL DEFAULT '',
    segment_duration_ms INTEGER NOT NULL DEFAULT 30000,
    overlap_ms          INTEGER NOT NULL DEFAULT 1000,
    max_concurrency     INTEGER NOT NULL DEFAULT 2,
    total_segments      INTEGER NOT NULL DEFAULT 0,
    completed_segments  INTEGER NOT NULL DEFAULT 0,
    failed_segments     INTEGER NOT NULL DEFAULT 0,
    abandoned_segments  INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at          TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at        TEXT,
    error               TEXT
);

-- progressive_batch_segments: 每个段一行（不存 PCM 样本）
CREATE TABLE IF NOT EXISTS progressive_batch_segments (
    id                  TEXT PRIMARY KEY NOT NULL,
    job_id              TEXT NOT NULL REFERENCES progressive_batch_jobs(id),
    segment_index       INTEGER NOT NULL,
    global_start_ms     INTEGER NOT NULL,
    global_end_ms       INTEGER NOT NULL,
    status              TEXT NOT NULL DEFAULT 'pending',
                        -- 'pending' | 'in_flight' | 'completed' | 'failed' | 'abandoned'
    retry_count         INTEGER NOT NULL DEFAULT 0,
    max_retries         INTEGER NOT NULL DEFAULT 3,
    error               TEXT,
    response_json       TEXT,            -- 完整 batch::Response JSON（成功时填充）
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_pbj_session ON progressive_batch_jobs(session_id);
CREATE INDEX idx_pbs_job ON progressive_batch_segments(job_id);
CREATE INDEX idx_pbs_status ON progressive_batch_segments(status);
```

### Drizzle ORM 定义（v2）

```ts
// packages/db/src/schema.ts

export const progressiveBatchJobs = sqliteTable("progressive_batch_jobs", {
  id: text("id").primaryKey().notNull(),
  sessionId: text("session_id")
    .notNull()
    .references(() => sessions.id),
  status: text("status").notNull().default("running"),
  provider: text("provider").notNull().default(""),
  model: text("model").notNull().default(""),
  baseUrl: text("base_url").notNull().default(""),
  language: text("language").notNull().default(""),
  segmentDurationMs: integer("segment_duration_ms").notNull().default(30000),
  overlapMs: integer("overlap_ms").notNull().default(1000),
  maxConcurrency: integer("max_concurrency").notNull().default(2),
  totalSegments: integer("total_segments").notNull().default(0),
  completedSegments: integer("completed_segments").notNull().default(0),
  failedSegments: integer("failed_segments").notNull().default(0),
  abandonedSegments: integer("abandoned_segments").notNull().default(0),
  createdAt: text("created_at").notNull().default(currentTimestamp),
  updatedAt: text("updated_at").notNull().default(currentTimestamp),
  completedAt: text("completed_at"),
});

export const progressiveBatchSegments = sqliteTable(
  "progressive_batch_segments",
  {
    id: text("id").primaryKey().notNull(),
    jobId: text("job_id")
      .notNull()
      .references(() => progressiveBatchJobs.id),
    segmentIndex: integer("segment_index").notNull(),
    globalStartMs: integer("global_start_ms").notNull(),
    globalEndMs: integer("global_end_ms").notNull(),
    status: text("status").notNull().default("pending"),
    retryCount: integer("retry_count").notNull().default(0),
    maxRetries: integer("max_retries").notNull().default(3),
    error: text("error"),
    responseJson: text("response_json"),
    createdAt: text("created_at").notNull().default(currentTimestamp),
    updatedAt: text("updated_at").notNull().default(currentTimestamp),
  },
);
```

### v2 重试协议

| 阶段 | 行为 | 状态变化 |
|------|------|----------|
| 初始提交 | HTTP POST 段音频到 batch 端点 | `pending` → `in_flight` |
| 成功 | 解析 response，保存 | `in_flight` → `completed` |
| HTTP 失败 | 1s/2s/4s backoff 重试，最多 3 次 | `in_flight` → `pending`（重试） |
| 3 次重试耗尽 | 标记为放弃，不影响其他段 | → `abandoned` |
| 所有段跑完 | 检查是否有 `abandoned` | job → `partial` |
| 用户点击 Continue | 重新打开音频文件，只提交未完成段 | 新建 job 会话 |

**不 fallback 到标准 batch。** 放弃的段在 response metadata 中记录为 `abandoned_segments`，UI 显示 "部分段转录失败" 提示。

### v2 持久化策略

- **Manager 不直接依赖 DB。** DB 写入由 plugin 层的 `TauriBatchRuntime` 在 emit `BatchSegmentResult` 时同步完成。
- **PCM 数据不持久化。** 完整音频已由 Recorder 保存到文件。恢复时重新读文件 + 确定性重切段。
- **段状态持久化。** 每次 `poll_completed` 收到完成结果时，plugin 层写一行 `progressive_batch_segments`。
- **Job 状态持久化。** Manager 进入 `Completed`/`Failed` 状态时，plugin 层更新 `progressive_batch_jobs`。
- **中断检测。** 会话异常结束时（crash/退出），job status 标记为 `interrupted`。应用启动时扫描所有 `status=running` 的 job，自动转为 `interrupted`。

---

## 4. Rust 数据结构

### 4.1 `TranscriptionMode` 枚举（`crates/listener-core/src/lib.rs`）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TranscriptionMode {
    Live,
    Batch,
    ProgressiveBatch,   // ← 新增
}
```

### 4.2 Segmenter（`crates/listener2-core/src/batch/progressive-batch/segmenter.rs`）

```rust
pub struct SegmenterConfig {
    pub sample_rate: u32,            // 48000
    pub segment_duration_ms: u32,    // 30000
    pub overlap_ms: u32,             // 1000
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self { sample_rate: 48000, segment_duration_ms: 30000, overlap_ms: 1000 }
    }
}

/// 分段器输出的单个音频段
pub struct AudioSegment {
    pub index: usize,
    pub global_start_ms: i64,
    pub global_end_ms: i64,
    pub samples: Vec<f32>,
}

/// 分段器状态机：累积 PCM → 边界触发 → 产出段
pub struct Segmenter {
    config: SegmenterConfig,

    /// 当前累积缓冲区的样本数（未满一段）
    buffer: Vec<f32>,
    /// 总样本计数（用于全局时间戳）
    total_samples: u64,
    /// 下一段的序号
    next_index: usize,

    // 以下作为输出端缓存
    ready: Vec<AudioSegment>,
}

impl Segmenter {
    pub fn new(config: SegmenterConfig) -> Self;

    /// 送入 PCM 帧，返回新产出段（可能为空）
    pub fn feed(&mut self, samples: &[f32]) -> Vec<AudioSegment>;

    /// 录音结束，冲刷剩余缓冲区
    /// 如果剩余不足一段，加 1s 前置重叠后仍产出一段；
    /// 如果剩余为 0，返回空。
    pub fn flush(&mut self) -> Vec<AudioSegment>;

    /// 当前总录音时长（ms）
    pub fn total_duration_ms(&self) -> i64;
}
```

**Segmenter 内部逻辑：**

```
buffer 累积直到 >= segment_samples (30s × 48000 = 1,440,000 samples)
  ① pop 出 segment_samples 个样本 → AudioSegment
  ② 保留最后 overlap_samples (1s × 48000 = 48000) 个样本到 buffer 开头
  ③ buffer 继续累积

例：
  Segment 0: global [0ms, 30000ms)     samples [0, 1440000)
  Segment 1: global [29000ms, 59000ms)  samples [1392000, 2832000)  ← 含 1s overlap
                 ↑ 保留的上段末尾 48000 样本
  Segment k 起始时间: k × (30000 − 1000) = k × 29000ms
  ...

flush():
  如果 buffer 内有 >0 样本：
    产出最后一段（global 位置按 total_samples 计算）
  否则：
    空，结束
```

### 4.3 Queue（`crates/listener2-core/src/batch/progressive-batch/queue.rs`）

```rust
pub struct QueueConfig {
    pub base_url: String,
    pub api_key: String,
    pub max_concurrency: usize,       // 2
    pub model: Option<String>,
    pub language: Option<String>,
    pub provider: BatchProvider,
}

pub enum SegmentStatus {
    Pending,
    InFlight { started_at: Instant },
    Completed { response: batch::Response },
    Failed { error: String, retry_count: u32 },
}

pub struct SegmentEntry {
    pub index: usize,
    pub global_start_ms: i64,
    pub file_path: PathBuf,
    pub status: SegmentStatus,
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

pub struct QueueProgress {
    pub total: usize,
    pub pending: usize,
    pub inflight: usize,
    pub completed: usize,
    pub failed: Vec<(usize, String)>,
}

/// 提交队列：管理 N=2 并发 HTTP 请求
pub struct BatchQueue {
    config: QueueConfig,
    client: reqwest::Client,          // 所有段共享
    segments: Vec<SegmentEntry>,

    // inflight 句柄（cancellation token）
    inflight_tasks: HashMap<usize, JoinHandle<()>>,
    // 完成通知（segment_index → oneshot::Receiver）
    completion_rxs: HashMap<usize, oneshot::Receiver<SegmentResult>>,
}

pub enum SegmentResult {
    Completed { index: usize, response: batch::Response },
    Failed { index: usize, error: String, exhausted: bool },
}

impl BatchQueue {
    pub fn new(config: QueueConfig) -> Self;

    /// 入队一个段（写入 temp WAV 后调用）
    pub fn enqueue(&mut self, segment: QueuedSegment);

    /// 轮询已完成的段
    pub fn poll_completed(&mut self) -> Vec<SegmentResult>;

    /// 等待所有 inflight 完成（block until empty）
    pub async fn drain(&mut self, timeout: Duration) -> Vec<SegmentResult>;

    pub fn progress(&self) -> QueueProgress;

    /// 取消所有 inflight
    pub fn cancel(&mut self);
}

**drain 超时行为（v2 新增）：**
- 超时值 = `segment_duration_ms * 1.5`（从 finish() 传入）
- 超时后标记所有剩余 inflight/pending 段为 `Failed { error: "timeout" }`
- 返回所有已完成的 SegmentResult
- 由 Manager.finish() 调用方决定如何处理（重试 / 标记 abandoned）

/// 准备入队的段
pub struct QueuedSegment {
    pub index: usize,
    pub global_start_ms: i64,
    pub file_path: PathBuf,
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}
```

**Queue 内部逻辑：**

```
enqueue():
  segments.push(Pending)
  try_dispatch()

try_dispatch():
  while inflight < max_concurrency && pending 非空:
    pop Pending → InFlight
    spawn HTTP POST 异步任务:
      ① 读取 WAV 文件
      ② POST multipart/form-data → {base_url}/v1/audio/transcriptions
      ③ 收到响应 → 解析 batch::Response → channel.send(Completed)
      ④ 失败 → 重试（最多 3 次）→ 仍失败 → channel.send(Failed)

drain():
  等待所有 inflight_tasks 完成
  收集所有 SegmentResult
```

**HTTP POST 请求细节：**

```rust
// 与 OpenAI adapter batch.rs 保持一致
const TARGET_SAMPLE_RATE: u32 = 16000;

fn convert_to_s16le(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let i16_val = (clamped * 32767.0) as i16;
        bytes.extend_from_slice(&i16_val.to_le_bytes());
    }
    bytes
}

fn resample_linear(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate {
        return samples.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (samples.len() as f64 / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let lo = src.floor() as usize;
        let hi = (lo + 1).min(samples.len() - 1);
        let t = src - lo as f64;
        out.push((samples[lo] as f64 * (1.0 - t) + samples[hi] as f64 * t) as f32);
    }
    out
}

async fn submit_segment_http(
    config: &QueueConfig,
    segment: &QueuedSegment,
) -> Result<batch::Response, String> {
    let resampled = resample_linear(&segment.samples, segment.sample_rate, TARGET_SAMPLE_RATE);
    let pcm_bytes = convert_to_s16le(&resampled);

    let file_part = reqwest::multipart::Part::bytes(pcm_bytes)
        .file_name("audio.raw")
        .mime_str("audio/pcm")?;

    let form = build_batch_multipart(
        file_part,
        config.model.as_deref(),
        config.language.as_deref(),
    );

    let url = transcription_url(&config.base_url)?;
    let client = create_client()?;
    let mut req = client.post(url).multipart(form);
    req = req.header("Authorization", format!("Bearer {}", config.api_key));

    let resp = req.send().await.map_err(|e| format!("HTTP request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {}", status, body));
    }

    let body = resp.text().await.map_err(|e| format!("failed to read response body: {e}"))?;

    let response: batch::Response = if config.provider == BatchProvider::OpenAI {
        owhisper_client::OpenAIAdapter::parse_batch_response(&body)
            .map_err(|e| format!("failed to parse response: {e}"))?
    } else {
        serde_json::from_str(&body)
            .map_err(|e| format!("failed to parse response: {e}"))?
    };

    Ok(response)
}
```

### 4.4 Stitcher（`crates/listener2-core/src/batch/progressive-batch/stitcher.rs`）

```rust
/// 携带来源分段索引的 word（用于追踪分段边界）
#[derive(Debug, Clone)]
pub struct TaggedWord {
    pub word: batch::Word,
    pub segment_index: usize,
}

pub struct StitcherConfig {
    pub overlap_ms: u64,       // 1000
    pub total_segments: usize,  // 预期段数
}

/// 按索引收集段响应，全部到齐后合并
pub struct Stitcher {
    config: StitcherConfig,
    /// 索引 → 完成段
    segments: BTreeMap<usize, CompletedSegment>,
}

pub struct CompletedSegment {
    pub index: usize,
    pub global_start_ms: i64,
    pub response: batch::Response,
}

/// Stitcher 输出：拼合的转录 + 分段边界标记
pub struct StitchOutput {
    pub response: batch::Response,
    /// 各段在全局词表中的起始 word 下标
    /// 例: [0, 15, 31, ...] 表示段 0 从 word[0] 开始，
    /// 段 1 从 word[15] 开始，段 2 从 word[31] 开始
    pub segment_boundaries: Vec<usize>,
    /// 段间间隙警告（padding_ms > GAP_WARNING_THRESHOLD_MS 的间隙）
    pub gap_warnings: Vec<GapWarning>,
}

pub struct GapWarning {
    pub segment_index: usize,
    pub gap_duration_ms: i64,
    pub padding_ms: i64,
}

impl Stitcher {
    pub fn new(config: StitcherConfig) -> Self;

    pub fn add_segment(&mut self, segment: CompletedSegment);

    /// 是否所有预期段都已添加
    pub fn is_complete(&self) -> bool;

    /// 合并所有段为 stitch::Response（含 segment_boundaries + gap_warnings）
    /// **v2 变更：不再返回 Err(MissingSegments)。**
    /// 永远尽量拼合：缺失段在 metadata 中记录 abandoned_segments
    pub fn stitch(&self) -> Result<batch::Response, StitcherError>;
}

pub enum StitcherError {
    /// 仅当没有任何段时返回
    EmptyResponse,
}

**v2 行为变更：** `stitch()` 不再因缺失段而报错。缺失的段在 response.metadata 中记录：
```json
{
  "total_duration": 120.0,
  "segments_stitched": 12,
  "segments_total": 15,
  "abandoned_segments": [3, 7, 11],
  "gap_warnings": [...],
  "segment_boundaries": [0, ...]
}
```
```

**Stitcher 合并规则（TaggedWord + segment_boundaries）：**

```
stitch():
  ① 全局偏移:
     对所有段，将每个 word 的 start/end 加上该段全局起始偏移
     将 word 包装为 TaggedWord { word, segment_index }

  ② 合并所有 TaggedWord，按 global_start 排序

  ③ Word 级去重（overlap 窗口内）:
     遍历排序后的 TaggedWord:
       如果当前 word.global_start - 上一个 word.global_end < DEDUP_EPSILON_S (0.05):
         丢弃当前 word（视为重复）
       否则保留

  ④ 计算 segment_boundaries:
     遍历去重后的 TaggedWord 列表:
       每当 segment_index 变化时，记录当前下标到 segment_boundaries
       首个边界总是 0

  ⑤ 拼接 transcript:
     将所有保留的 word 的 punctuated_text 按序拼接

  ⑥ 合并 metadata + gap_warnings:
     记录间隙 > GAP_WARNING_THRESHOLD_MS 的段间空白
     total_duration = max_word.end - min_word.start

  返回 StitchOutput { response, segment_boundaries, gap_warnings }
```

**关键设计细节：**

- `segment_boundaries` 是全局词表（去重后）中各段的起始下标：`[0, 15, 31]` 表示段 0 从 `word[0]` 起、段 1 从 `word[15]` 起、段 2 从 `word[31]` 起
- 前端 `segment.tsx` 检测 `word.metadata.segment_boundary` 标记，渲染虚线分隔
- `dedup_epsilon = 0.05s` 容差防止浮点误差导致同一词的 start 略微不同
- `GAP_WARNING_THRESHOLD_MS = 100` — 超过此值的段间间隙记录为 `GapWarning`（UI 可展示）
- `propagate_identity` 的 `provider_segment_index` 边界检查确保不在边界处合并不同段的内容

### 4.5 Manager（`crates/listener2-core/src/batch/progressive-batch/mod.rs`）

```rust
pub struct ProgressiveBatchConfig {
    pub session_id: String,          // ← 新增（用于 Runtime 事件）
    pub sample_rate: u32,
    pub segment_duration_ms: u32,
    pub overlap_ms: u32,
    pub max_concurrency: usize,
    pub base_url: String,
    pub api_key: String,
    pub model: Option<String>,
    pub language: Option<String>,
    pub provider: BatchProvider,
    pub session_dir: PathBuf,
}

/// Manager 状态机
pub enum ManagerState {
    /// 录音中，未达到分段阈值
    Accumulating {
        buffer: Vec<f32>,
    },
    /// 录音中，正在分段提交
    Active {
        segmenter: Segmenter,
        queue: BatchQueue,
        stitcher: Stitcher,
    },
    /// 全部完成，结果已就绪
    Completed {
        result: batch::Response,
        partial: bool,          // true = 部分段被 abandoned
        abandoned_indices: Vec<usize>,
    },
    /// 失败
    Failed {
        error: String,
    },
}

/// 顶层管理器
pub struct ProgressiveBatchManager {
    config: ProgressiveBatchConfig,
    state: ManagerState,
    segments_dir: PathBuf,
    write_wav_fn: WriteWavFn,
    submit_fn: SubmitSegmentFn,
    runtime: Option<Arc<dyn BatchRuntime>>,
}

impl ProgressiveBatchManager {
    /// 创建 Manager（live recording PCM 路径）
    pub fn new(config: ProgressiveBatchConfig) -> Self;

    /// 从 DB 恢复（Continue 路径）
    /// completed_segments: 已完成的段列表（从 DB 加载）
    /// 只重新提交 status≠completed 的段
    pub fn resume(
        config: ProgressiveBatchConfig,
        completed: Vec<PersistedCompletedSegment>,
    ) -> Self;

    /// 注入 Runtime（用于 emit BatchSegmentResult 事件）
    pub fn with_runtime(self, runtime: Arc<dyn BatchRuntime>) -> Self;

    /// 录音中：送入 PCM 帧
    pub fn on_audio_frame(&mut self, samples: &[f32]);

    /// 录音结束：冲刷 segmenter，等待队列 drain（带超时），stitch
    /// 超时后标记剩余段为 failed，尽可能 stitch
    pub async fn finish(&mut self) -> Result<batch::Response>;

    pub fn state(&self) -> &ManagerState;
    pub fn progress(&self) -> QueueProgress;
}

pub type PcmSender = tokio::sync::mpsc::Sender<Arc<[f32]>>;

/// 从 DB 恢复时使用的已完成段记录
pub struct PersistedCompletedSegment {
    pub index: usize,
    pub global_start_ms: i64,
    pub response: batch::Response,
}
```

**Manager 状态机：**

```
Accumulating
  │ on_audio_frame: 首个 frame 到达时立即跃迁
  │                → 将 buffer 转移到 Segmenter → Active
  │                录音结束（finish 前无 frame）: buffer 为空 → 报错
  │
  ▼
Active
  │ on_audio_frame: feed → Segmenter → enqueue → try_dispatch
  │ segmenter.ready: 写 temp WAV → Queue.enqueue
  │ queue.poll_completed: Stitcher.add_segment
  │                      → if runtime.is_some():
  │                          emit BatchSegmentResult { session_id, segment_index, response }
  │ 录音结束: 调用 finish() → Finalizing
  │
  ▼
Finalizing
  │ segmenter.flush() → 写最后 temp WAV → enqueue
  │ queue.drain() →  等待所有完成
  │                  poll_completed 循环 → 同上 emit BatchSegmentResult
  │ stitcher.stitch() → StitchOutput { response, segment_boundaries, gap_warnings }
  │                  → emit Completed（前端拼合完整结果 + 虚线分隔）
  ▼
Completed
  │ result() 立即返回 batch::Response

Accumulating → Failed (buffer 为空时 finish 报错)
Active → Failed (段提交耗尽重试)
Finalizing → Failed (stitch 失败)
```

### 4.6 PCM 接收任务（Manager 内部）

```rust
// 在 Manager::new() 中启动：
let (tx, mut rx) = tokio::sync::mpsc::channel::<Arc<[f32]>>(256);
let mut manager_clone = self.clone(); // Arc<Mutex<...>>
let handle = tokio::spawn(async move {
    loop {
        tokio::select! {
            Some(frame) = rx.recv() => {
                let mut mgr = manager_clone.lock().await;
                mgr.on_audio_frame(frame);
            }
            _ = shutdown_rx.closed() => break,
        }
    }
});
self.frame_task = Some(handle);
```

**channel 容量说明：** PCM 帧每 ~20ms 一次，每次 960 samples (f32) ≈ 3840 bytes。256 容量足够缓冲 ~5s 的音频。如果 Manager 处理慢（如 Segmenter 判断、写文件），channel 会暂时积压，但不会有丢失风险。

---

## 5. 集成点

### 5.1 `TranscriptionMode` 扩展

| 文件                                                    | 改动                                                                   | 实现状态                                                                                   |
| ------------------------------------------------------- | ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `crates/listener-core/src/lib.rs:31`                    | `TranscriptionMode` 新增 `ProgressiveBatch` 变体                       | ✅                                                                                         |
| `plugins/transcription/src/api.rs:52`                   | `default_transcription_mode()` 处理 `ProgressiveBatch` 选择            | ✅                                                                                         |
| `crates/listener-core/src/actors/session/supervisor.rs` | `ProgressiveBatch` 模式：不启动 Listener，创建 PCM channel 转发给 ProgressiveBatchManager | ✅ `supervisor.rs:64-75` 检测 `ProgressiveBatch` 模式，创建 `(tx, rx)`；`post_start` 通过 `runtime.start_progressive_batch_stream()` 消费 PCM |

> **当前状态：`ProgressiveBatch` 已完全集成到 live recording 路径。** `effective_transcription_mode()` (`types.rs:52-54`) 正确返回 `ProgressiveBatch`；supervisor 不启动 Listener，而是创建 PCM channel → `start_progressive_batch_stream()` → `ProgressiveBatchManager`。`runtime.rs:137-189` 消费 PCM 流并经理完整的状态机生命周期。

### 5.2 SourceArgs 变更

```rust
// crates/listener-core/src/actors/source/mod.rs
pub struct SourceArgs {
    // ... 现有字段 ...
    pub mic_device: Option<String>,
    pub onboarding: bool,
    pub runtime: Arc<dyn ListenerRuntime>,
    pub audio: Arc<dyn AudioProvider>,
    pub session_id: String,
    pub listener_routing: ListenerRouting,
    pub recorder: Option<ActorRef<RecMsg>>,

    }
```

> **当前状态：** ✅ 设计目标已实现，但有实现差异。SourceArgs 未新增 `progressive_batch_pcm_tx` 字段 — PCM 通道通过 `ListenerRouting::ProgressiveBatch(PcmSender)` 枚举变体传递（`source/mod.rs:54`），supervisor 在 `children.rs:151` 设置 routing，pipeline dispatch 在 `pipeline.rs:156` 发送 PCM 帧到该 channel。`start_progressive_batch_stream` (`runtime.rs:137-189`) 消费 PCM 流。

### 5.3 Pipeline 变更

```rust
// crates/listener-core/src/actors/source/pipeline.rs
fn dispatch(&mut self, ctx, frame) {
    // ... 现有 VAD/replay/amplitude 逻辑 ...

    // 写入 Recorder（不变）
    if let Some(ref recorder) = self.recorder {
        recorder.do_send(RecMsg::AudioSingle(mic.clone()));
    }

    // ← 设计文档描述，尚未实现
    if let Some(ref tx) = self.progressive_batch_pcm_tx {
        let _ = tx.try_send(mic.clone());
    }

    // 发送到 Listener（不变）
    // ...
}
```

### 5.4 Session Supervisor 变更

```rust
// crates/listener-core/src/actors/session/supervisor/children.rs
pub fn spawn_source(args: SourceArgs) -> ... {
    // ← 设计文档描述，尚未实现
    if args.transcription_mode == TranscriptionMode::ProgressiveBatch {
        manager_tx = ProgressiveBatchManager::new(...).pcm_sender();
    }
    SourceActor::start(SourceArgs {
        progressive_batch_pcm_tx: manager_tx,
        ..existing_args
    })
}
```

> **当前状态：** ✅ 已实现。`supervisor.rs:64-75` Supervisor 在 `pre_start` 中检测 `ProgressiveBatch` 模式，创建 PCM channel `(tx, rx)`，`children.rs:145-151` 设置 `ListenerRouting::ProgressiveBatch(tx)` 传入 SourceArgs。Source 不再传递 `progressive_batch_pcm_tx` 独立字段 — PCM 通道通过 `ListenerRouting` 枚举变体传递。

### 5.5 `Listener2` 集成（`plugins/transcription/src/listener2/`）

> **当前状态：** ✅ 设计目标已实现，但采用不同的架构。ProgressiveBatchManager **不经过 `Listener2`**。live PCM 流由 `start_progressive_batch_stream()` (`plugins/transcription/src/listener/runtime.rs:137-189`) 直接创建并管理 `ProgressiveBatchManager`，在内存中消费 PCM 帧 → `on_audio_frame` → `finish()`。re-transcription 路径通过 `run_progressive_batch_from_file` / `continue_from_file` 在 `listener2-core` 内创建 Manager。两种路径的 `BatchSegmentResult`/`Completed` 事件统一走 `BatchRuntime::emit`。

### 5.6 `startTranscription` 命令变更

```rust
// 设计文档描述，尚未实现（当前实现见下方简化方案）
// plugins/transcription/src/listener2/commands.rs
pub async fn start_transcription(
    app: ..., params: TranscriptionParams,
) -> Result<TranscriptionOutput, ...> {
    // ← 新增：先检查是否有 Progressive Batch 结果
    if let Some(result) = app.listener2().try_get_progressive_result(&params.session_id) {
        return Ok(TranscriptionOutput {
            session_id: params.session_id,
            mode: BatchRunMode::Direct,  // 对外表现为 Direct（完整响应）
            response: result,
        });
    }

    // 原有逻辑
    let control = app.listener2().start_transcription(params);
    // ...
}
```

> **当前状态：** ✅ 实现了更简化的方案。`params.progressive_batch: true` 时，`run_batch_inner` 直接调用 `run_progressive_batch_from_file`。不缓存结果，不走 `Listener2` 快捷路径，但整体流程兼容现有 `TranscriptionEvent` 事件体系。

---

## 6. 前端变更

### v1: 无变更

前端完全不感知 Progressive Batch。流程：

```
Recording ends → CaptureLifecycleEvent::Stopped
  → useStartListening calls runBatch(sessionId)
  → runBatch calls startTranscription(params)
    → (Rust) 检测到 ProgressiveBatchManager，直接返回缓存的结果
  → transcriptionEvent "completed" received
  → persist callback writes to transcripts table (same as standard batch)
```

### v2: 增量展示事件（已实现 ✅）

新增 `TranscriptionEvent` 变体：

```ts
// Rust 端: BatchSegmentResult { session_id, segment_index, response }
// TS 端映射为:
type TranscriptionEvent =
  | { type: "started" }
  | { type: "completed"; response: BatchResponse; mode: BatchRunMode }
  | { type: "progress"; event: BatchStreamEvent }
  | { type: "segmentResult"; sessionId: string; segmentIndex: number; response: BatchResponse }
  | { type: "failed"; code: BatchErrorCode; error: string };
```

**前端处理流程：**

```
batch.ts:
  state.batchSegments: Record<string, Record<number, BatchResponse>>
    ↑ sessionId → segmentIndex → response

  handleBatchSegmentResult(action):
    batchSegments[sessionId][segmentIndex] = response
    → 自动触发 SegmentPreview 重新渲染
    → handleBatchResponse 时自动清理 batchSegments[sessionId]

general-batch.ts:
  收到 payload.type === "segmentResult" → handleBatchSegmentResult

state.ts (running_batch screen):
  新增 segmentResponses 字段 → SegmentPreview 从中读取按序展示

index.tsx:
  SegmentPreview 组件：在进度下方按顺序展示已完成的片段
  每个片段用 BatchResponse 渲染文字 + 虚线分隔

empty.tsx:
  显示 "N segments transcribed"

segment.tsx:
  检测 word.metadata.segment_boundary → 渲染虚线分隔标记
```

**与进度条方案的区别：**

| 维度     | 进度条（原设计）                          | 增量展示（当前实现）                               |
| -------- | ----------------------------------------- | -------------------------------------------------- |
| 用户看到 | 百分比数字                                | 逐段看到完整转录文字                               |
| 实现位置 | `TranscriptionEvent::progressive_progress` | `BatchSegmentResult` 事件 + 前端 buffer            |
| 等待时间 | 透明度提示，仍需等全部完成                | 每段完成立即展示，消除等待焦虑                     |
| 完成后   | 进度条→100%→完整结果                      | 各段文字→自动接合成完整结果 + 虚线分段标记         |

---

## 7. 内存管理（无临时文件）

PCM 分段**不再写入临时文件**。Manager 使用 `on_audio_frame(&[f32])` 将解码后的浮点样本馈送给 `Segmenter`，产出段后其 `samples: Vec<f32>` 直接入队。Queue 的 `submit_segment_http` 在内存中重采样 → 编码为 16000 Hz s16le，立即 POST。

```
run_progressive_batch_from_file:
  hypr_audio_utils::Source (rodio)
    → total_duration() 预检（metadata only，不解码全部）
    → 流式逐块 10s chunks（48000 * 10 frames）
    → mono 混音（on-the-fly）
    → on_audio_frame(&mono)
      → Segmenter 内部累积 Vec<f32>
      → 产出 AudioSegment { samples: Vec<f32>, ... }
      → Queue.enqueue(QueuedSegment { samples, ... })
        → submit_segment_http: resample + s16le encode + POST audio/pcm
```

**内存峰值分析（3h 录音，48kHz mono f32）：**

- Source 解码缓冲区：10s × 48000 × 4B = ~1.9 MB
- Segmenter 内部缓冲区：30s × 48000 × 4B = ~5.8 MB（最多一段）
- QueuedSegment.samples：30s × 48000 × 4B = ~5.8 MB（最多一个未提交的段）
- s16le 编码临时：30s × 48000 × 2B = ~2.9 MB
- **总峰值：~15 MB**（对比原 `source.collect()` 全文件解码 3h × 48000 × 4B × 2ch ≈ 4 GB）

**异常处理（v2 改进）：** 
- 不再 fallback 到标准 batch。重试协议 + 持久化兜底
- 所有重试放弃后 → job 标记 `partial`，response metadata 记录 `abandoned_segments`
- UI 显示 "部分段转录失败" 提示
- 用户可随时 "Continue Progressive Batch" 重试放弃的段

**临时目录仅用于 session_dir：** `std::env::temp_dir().join("progressive-batch-{session_id}")` 由 `run_progressive_batch_from_file` 创建，用于存放可能的 debug 日志，不写音频文件。

---

## 7.5 Continue 流程（v2 新增）

```
App 启动 / 用户点击 "Continue Progressive Batch"
  │
  ├─ 1. 查询 DB: progressive_batch_jobs WHERE session_id = ? AND status IN ('interrupted', 'partial')
  │
  ├─ 2. 找到 job → 检查 config 一致性
  │       如果当前 provider/model/segment_duration_ms 与 job 不一致:
  │         → 提示 "配置已变化，请使用 Re-transcribe (Progressive)"
  │       如果一致:
  │         → 继续
  │
  ├─ 3. 从 DB 加载已完成段
  │        SELECT * FROM progressive_batch_segments
  │        WHERE job_id = ? AND status = 'completed'
  │        → 反序列化 response_json → Vec<PersistedCompletedSegment>
  │
  ├─ 4. 打开音频文件（从 session_attachments 获取路径）
  │
  ├─ 5. 创建 Segmenter + 初始化 Stitcher（预装已完成段）
  │
  ├─ 6. 流式读取音频文件 PCM（同 run_progressive_batch_from_file）
  │      for each segment produced by Segmenter:
  │        if segment.index in stitcher: skip（已完成）
  │        else: enqueue for submission
  │
  ├─ 7. finish() → drain(timeout) → stitch
  │
  └─ 8. 更新 DB：新完成的段写入 progressive_batch_segments
```

---

## 7.6 UI 增量展示（Sprint 2 核心可视交付）

**已有组件（Sprint 1 实现但未连接 live 路径）：**

| 组件 | 位置 | 功能 |
|------|------|------|
| `batchSegments` state | `batch.ts` | `Record<sessionId, Record<segmentIndex, BatchResponse>>` |
| `handleBatchSegmentResult` | `batch.ts` | 收到每段结果后存入 map |
| `SegmentPreview` | `index.tsx` | 按序展示已完成片段文字 |
| `segmentCount` | `empty.tsx` | "N segments transcribed" |
| `segment_boundary` 标记 | `segment.tsx` | 虚线分隔各段 |

**Sprint 2 激活条件：** PCM 实时流接入 Manager 后，live recording 过程中自动触发 `BatchSegmentResult` 事件 → 前端 buffer 逐段展示。用户不需要额外操作即可看到每段完成后的转写文字。

**右键菜单变更：**

```
当前:
  Copy
  Re-transcribe
  Delete recording

Sprint 2 改为:
  Copy
  Re-transcribe (Total)           ← 全文件 batch（现有行为）
  Re-transcribe (Progressive)     ← 从头跑 progressive batch
  Continue Progressive Batch       ← 仅当有未完成 job 时出现
  ───────────
  Delete recording
```

---

## 8. 模块结构

```
crates/listener2-core/src/
  batch/
    progressive-batch/           ← 新增模块
      mod.rs                     — ProgressiveBatchManager, ManagerState, 导出
      segmenter.rs               — Segmenter, AudioSegment
      queue.rs                   — BatchQueue, QueuedSegment, 提交逻辑
      stitcher.rs                — Stitcher, 全局偏移 + 去重
    mod.rs                       — 新增 `mod progressive_batch;`
    progressive/                 — 已有的本地 whisper argmax（不变）
    simple.rs                    — 不变
    accumulator.rs               — 不变
```

---

## 9. v1 → v2 演进路径

| 能力                | v1（本次实现）                                              | v2                                      |
| ------------------- | ----------------------------------------------------------- | --------------------------------------- |
| 分段提交            | ✅ 固定 30s + 1s overlap                                    | 可配置 + VAD 策略                       |
| 并发 N=2            | ✅ 硬编码 2                                                 | 可配置                                  |
| 阈值 3min           | ✅ Manager 状态机内实现                                     | 动态调整                                |
| 临时文件            | ✅ 无（PCM 在内存中直接 POST audio/pcm）                    | 可选写恢复文件                          |
| 持久化              | ❌ 全内存                                                   | ✅ `progressive_batch_jobs/segments` 表 |
| 断点恢复            | ❌ crash 后 fallback 标准 batch                             | ✅ 查表恢复                             |
| 进度前端            | ✅ 增量展示（`BatchSegmentResult` + 前端 buffer + SegmentPreview） | ✅ 进度条（可选扩展）                  |
| PCM 实时流集成      | ❌ 仅从文件读取                                             | ✅ Source pipeline 馈送                 |
| live recording 模式 | ❌ `effective_transcription_mode()` 忽略 `ProgressiveBatch` | ✅ 正确路由                             |
| WebSocket 控制面    | ❌ 纯 HTTP                                                  | 🔜 待定                                 |
| 音视频多分段策略    | ❌ 固定                                                     | ✅ VAD + 固定可选                       |

---

## 10. 实现状态总览

### 已实现的组件（所有在 `crates/listener2-core/src/batch/progressive_batch/`）

| 模块                      | 文件             | 关键功能                                                                                   | 测试数            |
| ------------------------- | ---------------- | ------------------------------------------------------------------------------------------ | ----------------- |
| `ProgressiveBatchManager` | `mod.rs`         | 状态机 + `runtime: Option` + `with_runtime()` + `poll_completed` emit `BatchSegmentResult` | 14                |
| `Segmenter`               | `segmenter.rs`   | 固定时长分段 + 1s overlap，feed/flush                                                      | 29                |
| `BatchQueue`              | `queue.rs`       | N=2 并发，复用 `transcription_url()` + `build_batch_multipart()` + `create_client()`      | 17                |
| `Stitcher`                | `stitcher.rs`    | `TaggedWord` 追踪词源 + `segment_boundaries` + `gap_warnings` + word 级去重                | 15                |
| Integration               | `integration.rs` | `run_progressive_batch_from_file` — 公共入口，透传 `runtime` 参数                          | 嵌入 Manager 测试 |
| **合计**                  | **5 文件**       | **~113 K 代码**                                                                            | **109+**          |

### Sprint 2 已完成（Phase A/B/C）

| #   | 需要                                                        | 状态 | 实现位置                                                                       |
| --- | ----------------------------------------------------------- | ---- | ------------------------------------------------------------------------------ |
| 1   | live recording Source pipeline 集成                         | ✅   | `supervisor.rs:64-75` PCM channel → `ListenerRouting::ProgressiveBatch`        |
| 2   | `effective_transcription_mode()` 处理 `ProgressiveBatch`    | ✅   | `types.rs:52-54` 优先返回 `ProgressiveBatch`                                   |
| 3   | 前端增量展示（替代进度条）                                   | ✅   | `BatchSegmentResult` 事件 + `batchSegments` buffer + `SegmentPreview`          |
| 4   | v2 持久化表 + Continue                                      | ✅   | `20260726000000_progressive_batch_jobs.sql` + `resume()` + `continue_from_file` |
| 5   | 重试 + Drain 超时 + Partial Stitch                          | ✅   | `finish()` drain timeout + stitcher partial + test                             |
| 6   | 实时录音时 ProgressiveBatch 模式走通                         | ✅   | live PCM → runtime `start_progressive_batch_stream()` → Manager 完整周期        |

### Sprint 2 完成（Phase A/B/C/D ✅）

| #   | 需要                                | 设计参考章节 | 说明                                               |
| --- | ----------------------------------- | ------------ | -------------------------------------------------- |
| 1   | UI 右键菜单区分转写模式              | §7.6         | Re-transcribe 裂为3项 + Continue 条件显示 + 部分结果提示 |

