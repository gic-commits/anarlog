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

## 摸底验证结果（Aug 3）

### 访问方式
- **Groq 限制区域访问**：中国区域直连返回 403 Forbidden。需走系统代理（macOS 网络设置，端口 7890 等）。curl 需 `-x http://127.0.0.1:7890` 或设置 `HTTPS_PROXY` 环境变量。
- Speaches 自建服务器无需代理。

### 请求报文（完全一致）
Groq 与 Speaches 都是 OpenAI 兼容 `/v1/audio/transcriptions`，multipart 字段完全一致（file/model/language/response_format/timestamp_granularities）。**请求构造可直接复用现有 `OpenAIAdapter::build_batch_multipart`**。

### 响应报文对比（同一 20s 音频）

| 维度 | Groq | Speaches | 兼容性 |
|------|------|----------|--------|
| 顶层字段 | `task, language, duration, text, words, segments, x_groq` | `duration, language, text, segments, usage, words` | Groq 多 `task`/`x_groq`，缺 `usage` |
| `language` | **`Chinese`**（全名）| `zh`（ISO 码）| ⚠️ 展示差异，功能无影响 |
| words | `word/start/end` | `word/start/end` | ✅ 一致 |
| segments | 标准字段 | 标准字段 | ✅ 一致 |
| `usage` | `None` | 有值 | ✅ 解析无碍（Option）|
| `logprobs` | 无 | 用 null（已处理）| ✅ |

### 关键结论
1. **`convert_response` 完全兼容 Groq**：Groq words 无 confidence/speaker 字段，但转换器自动填充默认值（confidence=1.0, speaker=None），本地 diarization 补 speaker
2. **`language` 返回全名**（`Chinese`）而非 ISO 码——只进 metadata 展示，不影响功能
3. **`x_groq` 扩展字段**：serde 默认忽略未知字段，无影响
4. **能力**：无 live/streaming（只有 batch）、无原生 diarization（本地可用）、RPM=20/min

### 待验证
- 429 限速（需真实触发）
- 分段长音频端到端（progressive 分段提交）

## 扩展文档摸底（Aug 3）

### OpenAI 兼容性细节
- 音频转录**不支持 `vtt`/`srt` response_format**（我们用 `verbose_json`，OK）
- 不支持 `logprobs`/`logit_bias`/`top_logprobs`（chat 字段，我们不传）
- **`verbose_json` + word/segment granularities 完全支持** ✅
- `language` 输入用 ISO-639-1，响应返回全名（`Chinese`）

### Batch API（远期优化候选）
- 支持音频转录/翻译的**异步批量**（JSONL 到 `/v1/files` + `/v1/batches` 创建，completion_window 24h-7d）
- **50% 成本折扣 + 不影响标准 RPM 限速**
- **音频必须用 `url` 参数**（非 file 上传）→ 需公网/本地托管音频
- JSONL 上限 5 万行 / 200MB
- **结论**：对本地录音不友好（需 URL 托管）；同步分段转录（file 上传）为首选，Batch API 记录为远期优化（若 RPM 成瓶颈）

### Rate Limits
限速维度：RPM/RPD/TPM/TPD/ASH/ASD；具体数值按账号在 console 显示。音频关键约束 RPM≈20/min（文档提及，需实测确认当前账号值）。

### TTS（次要）
Groq 有 TTS（`/v1/audio/speech`，Orpheus），**仅英语/阿拉伯语，无中文**——对当前中文场景价值有限，记录不实现。

### 模型
- whisper-large-v3 / v3-turbo：99+ 语言、实时优化（推理快，非 streaming API）
