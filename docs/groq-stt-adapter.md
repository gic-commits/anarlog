# Groq STT 适配设计（Sprint 4）

## 目标

将 Groq 的 Speech-to-Text 接入 anarlog，作为可选的 batch provider（`batch + 客户端分段`）。

Groq 是 **OpenAI 兼容**的 `/v1/audio/transcriptions` 端点，无 streaming（只有 transcriptions / translations 两个 batch 端点）。因此**只能走 batch**（不支持 live / Realtime），长音频用**现有 progressive batch 分段机制**绕过 100MB 限制。

## 端点与模型

```
https://api.groq.com/openai/v1/audio/transcriptions
```

| 模型 ID | 特点 | 速度 | WER |
|---------|------|------|-----|
| `whisper-large-v3-turbo` | 多语言，性价比高 | 216× 实时 | 12% |
| `whisper-large-v3` | 多语言，精度高 | 189× 实时 | 10.3% |

## 关键参数（与 OpenAI 兼容）

- `file`（或 `url` 支持大文件/Base64URL）
- `model`（必填）
- `language`（ISO-639-1，可选）
- `prompt`（≤224 token）
- `response_format`: `json` / `verbose_json` / `text`
- `temperature`: 0（推荐）
- `timestamp_granularities[]`: `segment` / `word`（需 verbose_json）

响应 metadata：`avg_logprob` / `compression_ratio` / `no_speech_prob` —— 与现有解析一致。

## 限制

| 维度 | 限制 | 说明 |
|------|------|------|
| 单文件 | 25MB（free）/ 100MB（dev）| 现有分段 30s-10m = 0.96-19.2MB，天然满足 |
| ASH | 7200s/小时（2h 音频/h）| 触发 429 |
| ASD | 28800s/天（8h 音频/天）| 触发 429 |
| RPM | 20 次/分钟 | **对分段并行影响最大** |
| RPD | 2000 次/天 | |
| 无 streaming | 只有 batch | 不支持 live/progressive-batch API |
| 无 diarization | 文档未提 | 本地 diarization 应仍可用（在客户端）|

## 适配方案

### 1. Provider 识别

- `Provider` enum 新增 `Groq`，`from_url`/`from_host` 识别 `api.groq.com`
- `AdapterKind` 新增 `Groq`（或映射到 `OpenAI`——因为完全 OpenAI 兼容）
- `BatchProvider` 新增 `Groq`

### 2. 能力判定

- `has_live_mode()` = false（无 streaming）
- progressive batch：**需要走客户端分段**（现有 `run_progressive_batch_from_file` 的 OpenAI 分支，只要模型名不被识别为 GPT 系列就会走 `run_direct_batch`，需要特殊处理让 Groq 走 progressive 分段）
- diarization：本地引擎可用（不依赖 provider）

### 3. 请求构造

复用 `OpenAIAdapter::build_batch_multipart` + `parse_batch_response`（完全 OpenAI 兼容，只需 base_url 指向 Groq + Authorization Bearer key）。

### 4. 429 限速处理（关键）

Groq 的 RPM=20/min，触发返回 429 + `retry-after` 头。

现有 `queue.rs` retry 逻辑：
- `1 << attempt`（1s/2s/4s）退避，3 次
- **未区分 429 / retry-after 头**，任何错误同退避

需增强：
- `submit_segment_http` 解析 429 响应，读 `retry-after` 头做指数退避
- 429 时重试次数可放宽（限速是瞬时的，等待后即可成功）
- 分段并发（N=2）+ 30s 段 ≈ 4 段/min，远低于 20 RPM，**天然安全**；但大量短段时需限速

## 改动文件

| 文件 | 改动 |
|------|------|
| `crates/owhisper-client/src/providers.rs` | `Provider` 新增 `Groq`，`from_url` 识别 `api.groq.com` |
| `crates/owhisper-client/src/adapter/mod.rs` | `AdapterKind` 新增 `Groq`（或映射 OpenAI），`from_url_and_languages` 识别 |
| `crates/listener2-core/src/batch/mod.rs` | `BatchProvider` 新增 `Groq`，`run_batch` 路由到 progressive 分段 |
| `crates/listener2-core/src/batch/progressive_batch/queue.rs` | 429/retry-after 处理 |
| 前端 settings | 新增 Groq provider + 模型列表 + API key 配置 |
| `apps/desktop/src/stt/useRunBatch.ts` 等 | provider 选择逻辑 |

## 验证

- 用 Groq API key 对测试音频跑 batch + 分段转录
- 确认 verbose_json + word/segment 时间戳解析
- 确认本地 diarization 生效
- 429 触发时重试生效（可用故意超限验证）
