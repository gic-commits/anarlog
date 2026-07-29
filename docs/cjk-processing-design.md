# CJK 后处理架构

## 概述

两种互斥的 CJK 后处理路径：

1. **客户端本地处理（Local）** — `cjk-processor` crate，在转录响应返回后、入库前执行
2. **服务端处理（Server-side）** — 由 Speaches（OpenAI 兼容服务器）Whisper 输出后执行，客户端通过 `cjk_post_process=true` 请求参数开启

默认均关闭。用户可在 OpenAI 提供商的设置面板中分别控制。

---

## 客户端本地处理

### 数据流

```
batch::Response
  → process_response(entries, language, features)
    → split_to_entries()    拆分多字 CJK 词为单字，记录 atomic_ranges
    → Processor::process()  执行 pipeline
      → Layer 1: Gap Punctuation（gaps ≥0.8s → 。，≥0.25s → ，）
      → Layer 2: Jieba segmentation（可选，zh 语言）
      → Layer 3: Acoustic merge（可选，修正 jieba 的 OOV 或纯声学分组）
        → acoustic_verify()          Tier B: jieba 输出上验证 OOV
        → acoustic_only()            Tier C: 纯声学（无 jieba）
      → Layer 4: build_output()     组装 WordGroup
    → collapse_groups()             将 ≤4 字 atomic_range 内的碎片合并回原词
    → reconstruct_from_groups()     还原为 batch::Word
```

### 组件

| 组件 | 文件 | 职责 |
|------|------|------|
| `CjkLayerFlags` | `listener2-core/src/batch/mod.rs:17` | per-layer 开关：punctuation / jieba / acoustic_merge |
| `CjkFeatures` | `cjk-processor/src/config.rs` | Processor 运行时特征标志 |
| `Processor` | `cjk-processor/src/lib.rs` | 持有 jieba 实例 + Config，执行 pipeline |
| `split_to_entries` | `listener2-core/src/batch/cjk.rs:53` | 拆分 batch::Word → WordEntry，返回 atomic_ranges |
| `collapse_groups` | `listener2-core/src/batch/cjk.rs:147` | 将 atomic_range 内的 WordGroup 合并回原词 |
| `process_response` | `listener2-core/src/batch/cjk.rs:230` | 入口，检查 zh 语言 + 调度 pipeline |

### 阈值

- `flags.jieba == true` → `min_cjk_split_len = 5`（≥5 字词自由拆分供 jieba；≤4 字词受 atomic 保护）
- `flags.jieba == false` → `min_cjk_split_len = usize::MAX`（所有多字词受保护）

### 缓存

`with_processor` 使用 `LazyLock<Mutex<(Processor, CjkFeatures)>>` 缓存 Processor 实例。`CjkFeatures` 变化时重建。

---

## 服务端处理

### 数据流

```
UI Switch (cjk_server_side)
  → apps/desktop/src/settings/schema.ts        setting: cjk_server_side = false
  → apps/desktop/src/stt/useRunBatch.ts        读取并传入 TranscriptionParams
  → plugins/transcription/src/api.rs           TranscriptionParams.cjk_server_side
  → listener2-core/src/batch/mod.rs             BatchParams.cjk_server_side
  → owhisper-interface/src/lib.rs               ListenParams.cjk_server_side
  → owhisper-client/src/adapter/openai/batch.rs build_transcription_options() 读取
  → openai-transcription/src/batch/request.rs   CreateCustomTranscriptionOptions.cjk_post_process
  → multipart form field: cjk_post_process=true
```

### 生效范围

所有通过 OpenAI adapter 发送的 batch 请求（包括 Direct Batch 和 Progressive Batch）均受影响。Live 模式（WebSocket）不受影响。

### 设计决策

1. 仅 `CreateCustomTranscriptionOptions` 支持该字段——Speaches 使用 Custom 变体
2. 默认关闭——与 Speaches 默认行为一致
3. 与服务端互斥——开启服务端 CJK 后，不应再开启本地 CJK（否则双重处理导致文本异常）
   但技术上未做强制互斥，UI 也不会自动联动

---

## 配置项

| 设置键 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `cjk_post_process` | boolean | `true` | 本地 CJK 总开关 |
| `cjk_features` | JSON string | `{"punctuation":true,"jieba":true,"acoustic_merge":true}` | 各层子开关 |
| `cjk_server_side` | boolean | `false` | 服务端 CJK 开关（仅 Speaches） |

### UI

- `SttModeSection`: Mode / Segment 时长 / CJK (Server) 三个控件在同一行
- `CjkToggleSection`: 标题 "CJK post-processing (Local)"，含总 Switch + 三个子 Checkbox

---

## 关键文件

| 文件 | 用途 |
|------|------|
| `crates/cjk-processor/src/lib.rs` | Processor pipeline 入口 |
| `crates/cjk-processor/src/layer1.rs` | 间隙标点识别 |
| `crates/cjk-processor/src/layer2.rs` | Jieba 分词 |
| `crates/cjk-processor/src/layer3.rs` | 声学验证 / 纯声学合并 |
| `crates/cjk-processor/src/layer4.rs` | 输出组装 |
| `crates/listener2-core/src/batch/cjk.rs` | batch 层集成 + atomic collapse |
| `crates/openai-transcription/src/batch/request.rs` | `cjk_post_process` 字段 + multipart |
| `crates/owhisper-client/src/adapter/openai/batch.rs` | `build_transcription_options` 贯通 |
| `plugins/transcription/src/api.rs` | `TranscriptionParams.cjk_server_side` |
| `apps/desktop/src/settings/ai/stt/select.tsx` | UI 控件 |
| `apps/desktop/src/stt/useRunBatch.ts` | Frontend → Rust 数据桥接 |
