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
  ├─ Embedding (campplus-200k 28MB ONNX, 192-dim; 备选 wespeaker-zh-LM 256-dim)
  │    ─ 每段提取 speaker embedding（intra_threads=2 优化）
  │    ─ 短段(>=1.5s)跳过，标记为继承相邻段
  │
  ├─ Clustering (HDBSCAN density clustering, Aug 8)
  │    ─ L2 normalize → cosine distance matrix → HDBSCAN
  │    ─ 自动按密度定簇数（不依赖 threshold）；噪声(-1)被 smooth_speakers/merge_sandwiched 吸收
  │    ─ 取代 agglomerative + threshold（长音频过度聚类 64→9）
  │
   ├─ [NEW] VAD Min-Cut + Merge（WhisperX 算法）
   │    ─ 超 max_duration 的段在 [½τ, τ] 区间内找 VAD 最低分切开
   │    ─ 短段合并到累积 ≤τ 后提交
   │    ─ 取代 ±20% 水线策略（DurationScheduler）
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
| **默认 ⭐（Aug 8 起）** | `campplus_cn_en_common_200k.onnx` | 192 | [welcomyou HF](https://huggingface.co/welcomyou/campplus-3dspeaker-200k-onnx) | 200k 说话人，**CPU 最快（2×）**，分离度更高（F-M 0.85）|
| **备选（原默认）** | `wespeaker_zh_cnceleb_resnet34_LM.onnx` | 256 | [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) | CN-Celeb 中文训练 + Large Margin 微调，更准但慢（UI 标 accurate）|

后续通过设置界面让用户选择。

> **Aug 8 切换**：默认从 wespeaker-zh-LM 换成 campplus-200k（`59e5972e8`）——回测 84min 音频 campplus 157.5s vs wespeaker 334s（2× 快），speaker 数 8 vs 9（均合理）。wespeaker-zh-LM 保留为 UI "accurate" 选项。

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
> **Aug 8 优化已落地**：intra_threads=2（+1.5×）+ campplus 默认（+2×），合计约 3× 加速（84min 音频 embedding ~3.2min → ~1.5min）。batching 已验证在 CPU 上无收益（0.3-0.8×），warmup 已隐式生效。

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

## 4. 时长调度 — VAD Min-Cut + Merge

### 4.1 决策

采用 **WhisperX (Oxford, Interspeech 2023) 的 VAD Min-Cut + Merge** 方案，取代原定的 DurationScheduler ±20% 水线策略（后者已实现但将被替换）。

核心变化：从"积累后决策"改为"先切大段 + 后合小段"，最终每段 ≈τ 且边界在弱语音区。

### 4.2 Min-Cut

VAD 段 > `max_duration`（默认 30s）时，在 [½τ, τ] 区间内找 VAD 置信度最低点切开：

```
如果 is_active 且 当前段长 ≥ max_dur:
    搜索区间 = [current_pos + max_dur/2, current_pos + max_dur]
    cut_point = argmin(VAD_scores[搜索区间])
```

### 4.3 Merge

相邻短 VAD 段合并到累积 ≤τ 后提交。边界条件与 Progressive Batch §4.1.3 一致。

### 4.4 短段处理（不变）

VAD 产生的超短段（<1.5s）合并到相邻有效段，不单独提交。

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

### 6.2 MinCutMerge（替换 DurationScheduler）

```rust
struct MinCutMergeConfig {
    max_duration_ms: u32,    // 默认 30000
    model: VADModel,         // VAD 得分来源
}

impl MinCutMerge {
    /// 输入 VAD segments，输出提交组（每组 ≈max_duration）
    fn process(
        vad_segments: &[VADSegment],
        vad_scores: &[f64],       // VAD 帧级得分（用于 Min-Cut 找最低点）
        config: &MinCutMergeConfig,
    ) -> Vec<Vec<VADSegment>>;
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
   ├─ [NEW] MinCutMerge
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
         → diarization_threshold: f32 (0.1-0.99, 默认 0.85)
             ↓
CaptureParams / TranscriptionParams (plugins/transcription/api.rs)
             ↓
SessionParams / BatchParams (listener-core / listener2-core/batch/mod.rs)
             ↓
── 文件路径（run_progressive_batch_from_file）─────────────
Segmenter (VAD) → min_cut_merge 分组
   ↓
IncrementalDiarizationEngine.feed_segments(VAD段)  → finalize()
   ↓  (speaker 段 + 聚类标签)
每组 VAD 段 → 标准 OpenAI batch 转录
   ↓
词级 speaker_at_time(group_start + mid) 标注
   ↓
Stitcher → propagate_speaker_to_none 前后向填充 → UI

── 录音流路径（plugins/transcription/src/listener/runtime.rs）─────────
feed_pcm(i16) 实时喂入 IncrementalDiarizationEngine
   ↓  (内部 IncrementalVad → embedding → 增量重聚类)
manager.on_audio_frame 并行
   ↓
finish() → engine.finalize() 最终聚类
   ↓
按词中位时间 speaker_at_time() 标注 → SegmentResult 事件 → UI
```

## 9. Phase 分解

### Phase 0: Min-Cut + Merge 替换 DurationScheduler（✅ 已完成）

- [x] 新增 `crates/pyannote-local/src/min_cut_merge.rs`（Min-Cut 算法 + Merge 逻辑）
- [x] `integration.rs` 切换为 `min_cut_merge`（替换 `schedule_segments` 调用）
- [x] 替换 Diarization 路径中的 `DurationScheduler`
- [x] 移除 `duration_scheduler.rs`（代码 + 8 个单元测试）

### Phase A: DiarizationManager + 聚类 + 短段合并（~2天 ✅）

- [x] 实现真实 agglomerative clustering（替换 `embedding.rs` 中的 `cluster()` 桩）
- [x] 实现 `DiarizationManager`，封装 seg→embed→cluster 管线
- [x] 实现 `EmbeddingProvider` trait（wespeaker-cnceleb-LM + campplus-200k）
- [x] 短段合并（<1.5s 合并到相邻段）
- [x] 全部测试通过

### Phase B: 时长调度 + SubmitManager + DB（~1天 ✅）

- [x] ~~实现 `DurationScheduler`（±20% 水线规则）~~ → 已替换为 Min-Cut + Merge（Phase 0）
- [x] 实现 `SubmitManager`（`DiarizationSubmitter`: 队列 + N=2 并发 + 指数退避重试 + drain 超时 + 响应匹配）
- [x] DB 持久化 schema + 迁移（`diarization_jobs` / `diarization_segments` 表）
- [x] Drizzle schema（`diarizationJobs` / `diarizationSegments`）
- [x] Rust 行类型（`DiarizationJobRow` / `DiarizationSegmentRow`）
- [x] `persist_batch_event` 持久化 handler
- [x] `BatchEvent::DiarizationStarted` + `DiarizationSegmentResult` 变体
- [x] `TranscriptionEvent::from` 映射
- [x] `BatchParams` 字段贯通（`diarization_enabled` / `diarization_model` / `diarization_threshold`）
- [x] 全部测试通过（listener2-core 115/115, db-app 44/44, pyannote-local 36/36）

### Phase C: UI（✅ 已完成）

- [x] Settings 页面新增 Diarization toggle + model 选择 + threshold slider（`select.tsx` DiarizationSection）
- [x] threshold 默认 0.85（设置项保留；engine 在默认值时改走自适应估计，Aug 1 Night 实证发现固定 0.85 对中位距离 ~0.68 的多说话人音频会塌成 1 簇）
- [x] **threshold 默认 0.85 → 0.5**（Aug 2 网格搜索）：4 音频 ground truth（c5ee333b=1 / 5fdd76a7=6-9 / 4a1092c6=3 / fa087f41=6）全测，0.5 + T8S5G2（min_total=8, span=5, gap=2）最优（3/4 全对）。`smooth_speakers` 改用「最大连续跨度 ≥5s 或 累计时长 ≥8s」双条件。schema/select/useRunBatch/api.rs/batch/mod.rs 五处一致
- [x] Segment 渲染添加 speaker 标签 + 颜色
- [x] CJK 后处理兼容 diarization（保留 speaker 标签）

### Phase D: 录音流集成（✅ 代码完成，待真机验证）

- [x] `IncrementalVad`（`incremental_vad.rs`）— 有状态流式 VAD，跨 `feed` 保持状态（同 segmentation.onnx 模型）
- [x] `IncrementalDiarizationEngine`（`incremental_diarization.rs`）— 流式引擎：
  - `feed_pcm(&[i16])` → 内部 VAD 产出段 → 即时 embedding → 按 `recluster_interval` 增量重聚类
  - `feed_segments(&[Segment])` → 复用外部 Segmenter 的 VAD 段（batch 文件模式使用，跳过内部 VAD）
  - `finalize()` → 收尾 VAD + 最终聚类 + 无效段近邻 speaker 填充
  - `speaker_at_time(t)` → 半开区间 `[start, end)` 查 speaker，静音间隙返回 None（由 `propagate_speaker_to_none` 填充）
- [x] **方案 B 能量停顿切分**（Aug 1 Night）：`feed_one_segment` 改用 `split_into_turn_chunks`（`min_cut_merge.rs`）— 30ms RMS 找静音谷在停顿中点切子段，切分点由音频决定而非固定 2s 常数；无停顿区段回退 4s 封顶。替换原固定 `EMBEDDING_CHUNK_SECS=2.0` 窗口
- [x] **自适应聚类阈值**（Aug 1 Night）：`clustering::estimate_threshold` = 两两距离 median + 0.15·MAD；engine 在 threshold 为默认 0.85 时自动估计，用户显式调过滑块的值优先
- [x] **最小时长过滤**（Aug 1 Night）：`smooth_speakers` 删除累计时长 < 2s 的孤立 speaker（噪声块），归并到最近 temporal 邻居，解决短块 embedding 噪音导致的过度分段
- [x] 文件路径集成（`integration.rs`）：Segmenter VAD 先行 → `min_cut_merge` 分组 → 每段转录后按词中位时间 `speaker_at_time(group_start + mid)` 标注 → stitch 后 `propagate_speaker_to_none` 前后向填充
- [x] 录音流集成（`plugins/transcription/src/listener/runtime.rs`）：录音中复用 `VadGroupStream.take_vad_segments()` → `feed_segments`（与文件路径同一批 VAD 段）→ `finish()` 时补喂尾部 + `finalize()` → 按词中位时间标注 speaker
 - [x] **live diarization 解耦**（Aug 8，`d09623d7d`）：diarization（embedding+聚类）原在 `rx.recv()` 消费循环内同步执行，秒级阻塞导致 PCM channel 满 → 丢帧 → 覆盖不全。现解耦到独立后台 task（unbounded channel + join），PCM 循环只跑便宜 VAD
- [x] 待验证：真实设备端到端（speaker 标签是否随 `SegmentResult` 正确显示）

### Phase E: HDBSCAN 聚类 + 性能优化（Aug 8 ✅）

**背景**：84 分钟长音频 re-transcribe 产出 64 个 fake speakers（实际 ~9 人）。根因：固定 threshold=0.5（Aug 2 网格搜索对短音频最优）在长音频上过度聚类——同一说话人的 chunks 在长音频里 embedding 距离 >0.5，被切碎成多个"假 speaker"，且各自满足 smooth_speakers 的连续 span 条件不被合并 → "一句话切成多人"。

**研究结论（Aug 8）**：WeSpeaker 官方 VoxConverse v2 recipe 用 **UMAP + HDBSCAN + PAHC**（DER 5.4，优于谱聚类 6.3）；谱聚类用**特征值间隙自动定人数**。Rust 生态有 `hdbscan`/`faer`/`fast-umap`。VBx 明确不适合长音频（>30min AHC 慢），pyannote 端到端需 HF token。第二步（UMAP + PAHC + 1.5s 滑窗 embedding）仅 CPU 环境，当前不适合，记远期。

**改动**：
- [x] `clustering.rs` 新增 `hdbscan_cluster_embeddings`：L2 归一化 + cosine 距离矩阵 + HDBSCAN（Precalculated，min_cluster_size=3）
- [x] `incremental_diarization.rs` `recluster` 用 HDBSCAN 替代 agglomerative；噪声(-1)段给唯一临时 label，smooth_speakers/merge_sandwiched 吸收
- [x] `Cargo.toml` 依赖 `hdbscan = "0.12"`
- [x] ONNX 线程优化：`hypr_onnx::load_model_from_bytes_with_threads`；embedding 用 intra_threads=2（1.5×）
- [x] 默认模型切换：`wespeaker_zh_cnceleb_resnet34_LM` → `campplus_cn_en_common_200k`（2× 快 + 分离度更高）

**回测（84min 音频，wespeaker_zh_cnceleb_resnet34_LM）**：
| 指标 | 旧（agglomerative+0.5）| 新（HDBSCAN）|
|------|:---:|:---:|
| unique speakers | 64 | **9** ✅ |
| 处理时间 | ~480s | ~334s（intra_threads=2）|

**模型对比 + 默认切换**：
| 模型 | 维 | threads=2/chunk | diarization(84min) | speakers |
|------|:---:|:---:|:---:|:---:|
| wespeaker-zh-LM（旧默认）| 256 | 104ms | 334s | 9 |
| **campplus-200k（新默认）**| 192 | **52ms** | **157.5s** | 8 ✅ |

**已排除的优化**：批量推理（CPU 上 wespeaker/campplus batch 0.3-0.8×，无收益）；intra_threads=4/8（回归）。

**当前 diarization 耗时（84min）**：解码+VAD ~2min + embedding ~1.5min + HDBSCAN ~1min ≈ **~4.5min**。

## 10. 开放问题

1. ~~**阈值自适应**~~: ✅ **已被 HDBSCAN 取代（Aug 8）**。固定 threshold（无论 0.85 自适应还是 0.5 网格调优）在长音频上过度聚类（84min → 64 fake speakers）。HDBSCAN 按密度自动定簇数，不再依赖 threshold。残留：短音频/边缘场景的假 speaker 合并仍靠 smooth_speakers/merge_sandwiched 启发式

2. **说话人数预设**: 是否允许用户指定 speaker count？如果已知是 2-person 对话，可固定 n_clusters=2。HDBSCAN 下可通过 min_cluster_size / 强制簇数实现

3. **推理优化**: ✅ 已部分解决（Aug 8）— intra_threads=2（1.5×）+ campplus 模型（2×）。已验证 batch 无收益（CPU）。远期：并行 embedding（多线程 + 多 session，当前 session `&mut` 单线程安全）

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

### 11.3 Aug 1 Night 实证：固定 0.85 塌缩 + 自适应修复（真实录音）

**问题复现**：`5fdd76a7`（149.65s，已知 4 说话人）在默认 threshold=0.85 下 diarization 输出 1 speaker。实证根因是聚类阈值远高于 embedding 距离尺度：

| 指标 | 5fdd76a7 | c5ee333b |
|------|---------:|---------:|
| 子段 embedding 数 | 71 | 115 |
| 两两距离 p10 / p50 / p90 | 0.416 / 0.676 / 0.838 | 0.379 / 0.468 / 0.703 |

一半以上 pairwise 距离 < 0.85 → agglomerative average-linkage 全并成 1 簇。且最优阈值随音频变化（5fdd76a7 需 ~0.70，c5ee333b 需 ~0.60），任何固定阈值无法跨音频通用；"合并距离最大间隙"启发式也失败（最大间隙总在最后的 2→1 合并）。

**修复**（`clustering::estimate_threshold` = median + 0.15·MAD，clamp [0.4, 0.9]）+ `smooth_speakers`（剔除累计 <2s 的孤立 speaker）：

| 配置 | 5fdd76a7 结果 |
|------|------|
| fixed 0.85（旧默认） | **1 speaker**（bug） |
| fixed 0.70 | 4 speakers |
| **adaptive + 2s 过滤** | **4 speakers** ✓ spk0(片头) / spk1(主讲) / spk2(spk3(Q&A)，与人工验证 [1,2,0,3] 一致 |

live（`feed_pcm`/`feed_segments`）与 file re-transcript 共用引擎核心 → 两条路径自动获得修复。
