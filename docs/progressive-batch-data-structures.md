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
[Audio Hardware]
  → Arc<[f32]> PCM frames, ~20ms each, 48kHz
  → Source Pipeline (pipeline.rs)
      ├──→ Recorder Actor → audio.wav (full backup)
      ├──→ Listener Actor (仅 Live 模式)
      └──→ [NEW] ProgressiveBatchManager (仅 ProgressiveBatch 模式)
              │  PCM 累积（流式 10s chunks）
              │  ↓ (30s 边界)
              │  Segmenter → AudioSegment (samples in memory)
              │  └──→ Queue.enqueue(index, samples, sample_rate)
              │           ↓ N=2 并发
              │  BatchClient → POST audio/pcm (16000 Hz s16le)
              │           ↓
              │  response → Stitcher.add_segment(index, response)
              │           ↓ (所有段完成)
              │  Stitcher.stitch() → batch::Response
              │           ↓
              │  [缓存结果] ← startTranscription 时立即返回
              │
              录音结束时: flush() → 等待 InFlight 完成 → stitch → emit BatchResponse
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
    'max_concurrency', 2,
    'min_duration_secs', 180
  )
) WHERE id = ?;

-- transcripts 记录最终结果（同标准 Batch）
-- 无变化 —— words_json 已包含所有段的全局时间戳词
```

### v2: 新增持久化表（下游公平）

```sql
-- progressive_batch_jobs: 每个 session 一次
CREATE TABLE IF NOT EXISTS progressive_batch_jobs (
    id                TEXT PRIMARY KEY NOT NULL,
    session_id        TEXT NOT NULL REFERENCES sessions(id),
    status            TEXT NOT NULL DEFAULT 'running',       -- 'running' | 'completed' | 'failed'
    provider          TEXT NOT NULL DEFAULT '',
    model             TEXT NOT NULL DEFAULT '',
    base_url          TEXT NOT NULL DEFAULT '',
    api_key           TEXT NOT NULL DEFAULT '',
    language          TEXT NOT NULL DEFAULT '',
    segment_duration_ms INTEGER NOT NULL DEFAULT 30000,
    overlap_ms        INTEGER NOT NULL DEFAULT 1000,
    max_concurrency   INTEGER NOT NULL DEFAULT 2,
    min_duration_secs INTEGER NOT NULL DEFAULT 180,
    total_segments    INTEGER NOT NULL DEFAULT 0,
    completed_segments INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at        TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at      TEXT,
    error             TEXT
);

-- progressive_batch_segments: 每个段一行
CREATE TABLE IF NOT EXISTS progressive_batch_segments (
    id                TEXT PRIMARY KEY NOT NULL,
    job_id            TEXT NOT NULL REFERENCES progressive_batch_jobs(id),
    segment_index     INTEGER NOT NULL,
    global_start_ms   INTEGER NOT NULL,
    global_end_ms     INTEGER NOT NULL,
    file_path         TEXT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'pending',       -- 'pending' | 'uploading' | 'processing' | 'completed' | 'failed'
    retry_count       INTEGER NOT NULL DEFAULT 0,
    error             TEXT,
    response_words_json  TEXT NOT NULL DEFAULT '[]',
    response_text     TEXT NOT NULL DEFAULT '',
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
);

-- 索引
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
  minDurationSecs: integer("min_duration_secs").notNull().default(180),
  totalSegments: integer("total_segments").notNull().default(0),
  completedSegments: integer("completed_segments").notNull().default(0),
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
    filePath: text("file_path").notNull(),
    status: text("status").notNull().default("pending"),
    retryCount: integer("retry_count").notNull().default(0),
    error: text("error"),
    responseWordsJson: text("response_words_json").notNull().default("[]"),
    responseText: text("response_text").notNull().default(""),
    createdAt: text("created_at").notNull().default(currentTimestamp),
    updatedAt: text("updated_at").notNull().default(currentTimestamp),
  },
);
```

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
    pub async fn drain(&mut self) -> Vec<SegmentResult>;

    pub fn progress(&self) -> QueueProgress;

    /// 取消所有 inflight
    pub fn cancel(&mut self);
}

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
    // 重采样到 16000 Hz s16le（Speaches batch 端点要求）
    let resampled = resample_linear(&segment.samples, segment.sample_rate, TARGET_SAMPLE_RATE);
    let pcm_bytes = convert_to_s16le(&resampled);

    let file_part = reqwest::multipart::Part::bytes(pcm_bytes)
        .file_name("audio.raw")
        .mime_str("audio/pcm")?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("response_format", "json")
        .text("timestamp_granularities", "word");

    if let Some(ref model) = config.model {
        form = form.text("model", model.clone());
    }
    if let Some(ref lang) = config.language {
        form = form.text("language", lang.clone());
    }

    // 使用 url::Url 的 path_segments_mut() 避免字符串拼接双 /v1/
    let mut url: url::Url = config.base_url.parse().map_err(|_| "invalid base_url")?;
    if !url.path().trim_end_matches('/').ends_with("audio/transcriptions") {
        url.path_segments_mut()
            .map_err(|_| "cannot modify base_url path segments")?
            .pop_if_empty()
            .push("audio/transcriptions");
    }

    let client = reqwest::Client::new();
    let mut req = client.post(url).multipart(form);
    if !config.api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", config.api_key));
    }

    let resp = req.send().await.map_err(|e| format!("HTTP request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {}", status, body));
    }

    let body = resp.text().await.map_err(|e| format!("failed to read response body: {e}"))?;

    // OpenAI 兼容服务器（如 Speaches）可能返回 null logprobs，
    // 使用 OpenAIAdapter::parse_batch_response 而非直接 serde_json
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

impl Stitcher {
    pub fn new(config: StitcherConfig) -> Self;

    pub fn add_segment(&mut self, segment: CompletedSegment);

    pub fn is_complete(&self) -> bool;

    /// 合并所有段为单个 batch::Response
    pub fn stitch(&self) -> Result<batch::Response, StitcherError>;
}

pub enum StitcherError {
    MissingSegments(Vec<usize>),
    EmptyResponse,
}
```

**Stitcher 合并规则：**

```
stitch():
  按 index 遍历所有段（已排序）
  对每个段:
    ① 全局偏移:
       for word in segment.response.results.channels[0].alternatives[0].words:
         word.start = word.start + segment.global_start_ms / 1000.0
         word.end   = word.end   + segment.global_start_ms / 1000.0

    ② Segment 级去重:
       if index > 0:
         prev = segments[index - 1]
         prev_end = max(prev.response.results.channels[0].alternatives[0].words.last().end)
         cur_start = min(cur.response.results.channels[0].alternatives[0].words.first().start)
         if cur_start - prev_end < overlap_ms / 1000.0:
           # overlap 窗口内，丢弃当前段的重复部分
           # 策略：保留从 prev_end + epsilon 开始的 word
           cur.words.retain(|w| w.start > prev_end + 0.05)

    ③ 拼接 transcript 字符串:
       合并所有段保留的 word，按 start 排序，拼接 punctuated_word

    ④ 合并 metadata:
       total_duration = max(all segments' max(end)) - min(all segments' min(start))

  返回新的 batch::Response
```

**去重示例：**

```
Segment 0: words=[{start=0, end=2.5, text="你好"}, {start=2.5, end=3.2, text="世界"}]
           global offset +0 → [{start=0, end=2.5}, {start=2.5, end=3.2}]
           保留全部

Segment 1: words=[{start=0, end=2.8, text="你好"}, {start=2.8, end=5.1, text="今天天气"}]
           global offset +29 → [{start=29, end=31.8}, {start=31.8, end=34.1}]
           比较: prev_end=30.2 (对齐后 Segment 0 最后的 word 2.5+0?

不对，让我重新算：
Segment 0: global [0s, 30s)
  word: start=0.0, end=2.5 → 偏移后 global {0.0, 2.5}
  word: start=2.5, end=3.2 → 偏移后 global {2.5, 3.2}
  ↓
Segment 1: global [29s, 59s)
  word: start=0.0, end=2.8 → 偏移后 global {29.0, 31.8}
  word: start=2.8, end=5.1 → 偏移后 global {31.8, 34.1}
  ↓
比较: Segment 0 的 max_end = 3.2 (global)
     Segment 1 的 min_start = 29.0 (global)
     29.0 - 3.2 = 25.8s > 1s overlap → 无重叠，两段都保留

实际上 1s overlap 在语音连续时，Segment 1 包含了 29s-30s 的重复内容
Segment 0 到 29s-30s 也有语音
Segment 0 word: start=29.0, end=29.5 (global offset +0)
Segment 1 word: start=29.0, end=29.5 (global offset +29)
这两个是同一个语音片段 → 保留 Segment 0 的，丢弃 Segment 1 的

修正去重规则：
  for 每对相邻段 (i, i+1):
    if segment[i+1].first_word.start - segment[i].last_word.end < overlap_s:
      # 有重叠
      overlap_start = segment[i+1].first_word.start
      discard_words_from = segment[i+1].words
                        .iter()
                        .position(|w| w.start >= segment[i].last_word.end - 0.05)
                        .unwrap_or(0)
      segment[i+1].words = segment[i+1].words[discard_words_from..]
```

实际上，对于 30s + 1s overlap 的设置，非静音连续语音下重叠的 word 会被服务端转录两次，stitcher 需要去重。但用 segment 级去重（保留先到达段的全部 word），因为重叠区只有 1s，丢弃后段前 1s 的 word 只会丢失极少内容（已被前一段覆盖）。

**简化去重规则（v1）：**

- 如果后段第一条 word 的 start 与前段最后一条 word 的 start 之差 < overlap_s（1s），则整段丢弃后段
- 否则保留后段全部 word

不对，这样会丢失太多内容。让我重新想。

正确做法：对于 overlaps，丢弃后段中 start 小于前段最后一条 word 的 start + overlap 的所有 word。

```
保留条件: word.global_start >= prev_last_word.global_end - overlap_ms
```

这样就不需要 `timestamp_granularities[]=word` 参数，因为 batch 响应天然包含 word 时间戳。

### 4.5 Manager（`crates/listener2-core/src/batch/progressive-batch/mod.rs`）

```rust
pub struct ProgressiveBatchConfig {
    pub sample_rate: u32,
    pub segment_duration_ms: u32,
    pub overlap_ms: u32,
    pub max_concurrency: usize,
    pub min_duration_secs: u32,
    pub base_url: String,
    pub api_key: String,
    pub model: Option<String>,
    pub language: Option<String>,
    pub provider: BatchProvider,
    pub session_dir: PathBuf,
}

impl Default for ProgressiveBatchConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            segment_duration_ms: 30000,
            overlap_ms: 1000,
            max_concurrency: 2,
            min_duration_secs: 180,
            base_url: String::new(),
            api_key: String::new(),
            model: None,
            language: None,
            provider: BatchProvider::OpenAI,
            session_dir: PathBuf::new(),
        }
    }
}

/// Manager 状态机
pub enum ManagerState {
    /// 录音中，未达到分段阈值
    Accumulating {
        buffer: Vec<f32>,
        total_samples: u64,
    },
    /// 录音中，正在分段提交
    Active {
        segmenter: Segmenter,
        queue: BatchQueue,
        stitcher: Stitcher,
    },
    /// 录音已结束，等待 InFlight 完成
    Finalizing {
        queue: BatchQueue,
        stitcher: Stitcher,
    },
    /// 全部完成，结果已就绪
    Completed {
        result: batch::Response,
    },
    /// 失败
    Failed {
        error: String,
    },
}

/// 顶层管理器
pub struct ProgressiveBatchManager {
    session_id: String,
    config: ProgressiveBatchConfig,
    runtime: Arc<dyn BatchRuntime>,
    state: ManagerState,

    /// 临时段文件目录（{session_dir}/progressive-batch/）
    segments_dir: PathBuf,

    /// 从 Source 管道接收 PCM 帧
    pcm_rx: tokio::sync::mpsc::Receiver<Arc<[f32]>>,
    /// 通知任务退出
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// PCM 接收任务句柄
    frame_task: Option<JoinHandle<()>>,

    /// 完成信号：stitch 完成时通知
    completion_tx: Option<oneshot::Sender<Result<batch::Response>>>,
    completion_rx: Option<oneshot::Receiver<Result<batch::Response>>>,
}

impl ProgressiveBatchManager {
    /// 创建 Manager 并启动 PCM 接收任务
    pub fn new(
        session_id: String,
        config: ProgressiveBatchConfig,
        runtime: Arc<dyn BatchRuntime>,
    ) -> (Self, PcmSender);

    /// 录音中：送入 PCM 帧（由 PCM 接收任务调用）
    fn on_audio_frame(&mut self, samples: Arc<[f32]>);

    /// 录音结束：冲刷 segmenter，等待队列 drain，stitch
    pub async fn finish(&mut self) -> Result<batch::Response>;

    /// 取结果（若已完成则立即返回）
    pub async fn result(&mut self) -> Result<batch::Response>;

    /// 取消
    pub fn cancel(&mut self);

    /// 当前进度
    pub fn progress(&self) -> QueueProgress;
}

/// PCM 发送端，给 Source 管道用
pub type PcmSender = tokio::sync::mpsc::Sender<Arc<[f32]>>;
```

**Manager 状态机：**

```
Accumulating
  │ on_audio_frame: 累积至 total_samples > min_duration_secs × sample_rate
  │                → 将 buffer 转移到 Segmenter → Active
  │                录音结束: flush()，总时长 < min_duration_secs
  │                → 整段提交标准 Batch（fallback）
  │
  ▼
Active
  │ on_audio_frame: feed → Segmenter → enqueue → try_dispatch
  │ segmenter.ready: 写 temp WAV → Queue.enqueue
  │ queue.poll_completed: Stitcher.add_segment
  │ 录音结束: 调用 finish() → Finalizing
  │
  ▼
Finalizing
  │ segmenter.flush() → 写最后 temp WAV → enqueue
  │ queue.drain() → 等待所有完成
  │ stitcher.stitch() → Completed
  │
  ▼
Completed
  │ result() 立即返回 batch::Response
  │

Accumulating → Failed (录音时长 < min_duration_secs 时整段提交失败)
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
| `crates/listener-core/src/actors/session/supervisor.rs` | `ProgressiveBatch` 模式：不启动 Listener，启动 ProgressiveBatchManager | ❌ 未实现。`effective_transcription_mode()` 只识别 `Batch`，`ProgressiveBatch` 回退 `Live` |

> **⚠️ 当前 `ProgressiveBatch` 仅用于 batch re-transcription 路径。** 在 live recording 场景下，用户的 `stt_mode=progressive` 被 `default_transcription_mode()` 正确识别，但 `SessionParams.transcription_mode` 传给 live session supervisor 后，`effective_transcription_mode()` 在 `types.rs` 将其作为 `Live` 处理，仍启动 WebSocket listener。

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

    /// ← 设计文档描述，尚未实现
    pub progressive_batch_pcm_tx: Option<tokio::sync::mpsc::Sender<Arc<[f32]>>>,
}
```

> **当前状态：** ❌ 本小节所有内容（§5.2-5.6）均为设计目标，尚未实现。当前 Progressive Batch 仅通过 `run_progressive_batch_from_file` 用于从已有音频文件跑 batch（re-transcription 路径），未与 live recording 的 Source pipeline 集成。

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

> **当前状态：** ❌ 未实现。`spawn_source` 不识别 `TranscriptionMode::ProgressiveBatch`。

### 5.5 `Listener2` 集成（`plugins/transcription/src/listener2/`）

```rust
// 设计文档描述，尚未实现
// plugins/transcription/src/listener2/ext.rs
pub struct Listener2 {
    sessions: Arc<Mutex<HashMap<String, BatchSessionControl>>>,
    progressive_batch: Arc<Mutex<HashMap<String, ProgressiveBatchManager>>>,  // ← 新增
}

impl Listener2 {
    /// 创建 ProgressiveBatchManager（由 capture start 调用）
    pub fn start_progressive_batch(
        &self,
        session_id: &str,
        config: ProgressiveBatchConfig,
        runtime: Arc<dyn BatchRuntime>,
    ) -> PcmSender;

    /// 等待结果（由 startTranscription 调用）
    pub async fn wait_progressive_batch_result(
        &self,
        session_id: &str,
    ) -> Result<batch::Response>;

    /// 录音结束时 flush（由 capture stop 调用）
    pub async fn finalize_progressive_batch(
        &self,
        session_id: &str,
    ) -> Result<()>;
}
```

> **当前状态：** ❌ 未实现。当前 `ProgressiveBatchManager` 由 `run_progressive_batch_from_file` 内部创建，不经过 `Listener2`。`transcriptionEvent::Completed` 走标准 `BatchRuntime::emit` 路径。

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

### v2: 进度事件（可选）

新增 `TranscriptionEvent` 变体：

```ts
// 前端
type TranscriptionEvent =
  | { type: "started" }
  | { type: "completed"; response: BatchResponse; mode: BatchRunMode }
  | { type: "progress"; event: BatchStreamEvent }
  | { type: "progressive_progress"; progress: ProgressiveBatchProgress } // ← 新增
  | { type: "failed"; code: BatchErrorCode; error: string };

interface ProgressiveBatchProgress {
  totalSegments: number;
  completedSegments: number;
  percentage: number;
}
```

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

**异常 fallback：** 录音文件本身由 Recorder 独立维护（`audio.wav` / `audio.mp3`）。若 Progressive Batch 失败，`startTranscription` 可直接从文件跑标准 Batch，无数据丢失。

**临时目录仅用于 session_dir：** `std::env::temp_dir().join("progressive-batch-{session_id}")` 由 `run_progressive_batch_from_file` 创建，用于存放可能的 debug 日志，不写音频文件。

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
| 进度前端            | ❌ 无（用户只看等待）                                       | ✅ 进度条                               |
| PCM 实时流集成      | ❌ 仅从文件读取                                             | ✅ Source pipeline 馈送                 |
| live recording 模式 | ❌ `effective_transcription_mode()` 忽略 `ProgressiveBatch` | ✅ 正确路由                             |
| WebSocket 控制面    | ❌ 纯 HTTP                                                  | 🔜 待定                                 |
| 音视频多分段策略    | ❌ 固定                                                     | ✅ VAD + 固定可选                       |

---

## 10. 实现状态总览

### 已实现的组件（所有在 `crates/listener2-core/src/batch/progressive_batch/`）

| 模块                      | 文件             | 关键功能                                                                 | 测试数            |
| ------------------------- | ---------------- | ------------------------------------------------------------------------ | ----------------- |
| `ProgressiveBatchManager` | `mod.rs`         | 状态机：Accumulating → Active → Completed/Failed，on_audio_frame，finish | 14                |
| `Segmenter`               | `segmenter.rs`   | 固定时长分段 + 1s overlap，feed/flush                                    | 29                |
| `BatchQueue`              | `queue.rs`       | N=2 并发，HTTP multipart POST，重试 3 次，drain                          | 17                |
| `Stitcher`                | `stitcher.rs`    | word 级全局偏移，segment 级去重，gap 检测                                | 15                |
| Integration               | `integration.rs` | `run_progressive_batch_from_file` — 公共入口                             | 嵌入 Manager 测试 |
| **合计**                  | **5 文件**       | **~113 K 代码**                                                          | **75+**           |

### 未实现的部分（设计文档有但代码尚无）

| #   | 需要                                                  | 设计参考章节 | 说明                                                                                                 |
| --- | ----------------------------------------------------- | ------------ | ---------------------------------------------------------------------------------------------------- |
| 1   | live recording Source pipeline 集成                   | §5.2-5.4     | `SourceArgs.pcm_tx`、Pipeline dispatch、Session Supervisor 均未改动                                  |
| 2   | `effective_transcription_mode()` 修复                 | §5.1         | `ProgressiveBatch` 当前回退为 `Live`，应触发 batch 路径                                              |
| 3   | 前端 progress 事件                                    | §6 v2        | `TranscriptionEvent.progressive_progress` 未定义                                                     |
| 4   | v2 持久化表                                           | §3 v2        | `progressive_batch_jobs` / `progressive_batch_segments` 表未创建                                     |
| 5   | 用户可配置 segment_overlap_ms / max_retries           | §9 v2        | 当前硬编码 1000ms / 3                                                                                |
| 6   | 实时录音时 ProgressiveBatch 模式走通                  | §5.1-5.4     | §5.1 修复 + §5.2-5.4 实现 + 前端 `stt_mode=progressive` → `TranscriptionMode::ProgressiveBatch` 贯通 |
| 7   | `min_duration_secs` 与 `segment_duration_ms` 逻辑重叠 | §4.0         | 当前实现阈值恒等于段长，`min_duration_secs` 字段存在但实际由段长决定                                 |
