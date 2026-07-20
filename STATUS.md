# Speaches 实时转录接入 — 进展跟踪

## 目标

通过 OpenAI provider 配置接入自建 Speaches 服务器（`wss://ztnas.2113111.xyz:28010`），实现 WebSocket Realtime API 实时转录。

## 验证方式

- Speaches 服务端日志：`GET /v1/logs?lines=500`（UTC 时间）
- 本地 tracing 日志：`pnpm -F @hypr/desktop tauri:dev` 终端输出

---

## 重要发现

### `initial_message: None` 方案已可行

不发送 `session.update`，完全依赖 URL 参数：
- WebSocket 连接 ✅
- VAD 检测 ✅（Speaches 默认 threshold=0.9, silence_duration_ms=550）
- 实时转写结果返回 ✅

三个测试会话：
| 会话 | 代码 | 结果 |
|------|------|------|
| 04:31 UTC | `session.update` 旧代码 | ❌ `prefix_padding_ms` error → 重连循环 |
| 05:58 UTC | `initial_message: None` | ⚠️ 连接 ✅ VAD ✅ 但转录在 `break_timeout` 后才返回 |
| **06:00 UTC** | `initial_message: None` | **✅ URL 直连 ✅ VAD ✅ 收到转写 "肯配置方案的零十二零 配置速度方式"** |

### URL 参数有效

```python
# Speaches 正确解析 URL 参数
model=Systran/faster-whisper-base  # ✅ 模型名透传
intent=transcription                # ✅ 转写模式
```

### session.update 破坏连接

❌ 发送 `session.update`（含 `turn_detection` 或 `input_audio_transcription`）导致 Speaches 发送 error event 后断开连接。

即使 VAD 值被成功应用（`threshold=0.7`, `silence_duration_ms=2000`），Speaches 仍在 `session.updated` 后发送 error event → 客户端 `listener_terminated` → 无限重连循环。

✅ **修复：`initial_message()` 返回 `None`，不发送 `session.update`，完全依赖 URL 参数。**

### prefix_padding_ms 不被支持

Speaches 不支持 `prefix_padding_ms` 参数。在 `session.update` 中发送该字段会触发未知模型错误。

### Speaches 默认 VAD

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `threshold` | 0.9 | VAD 敏感度 |
| `silence_duration_ms` | 550 | 静音判定时长 |
| `prefix_padding_ms` | 0 | 添加的音频前缀（客户端无效） |
| `create_response` | false | 不生成回复（转写模式） |

### 转写性能 (CPU-only)

- **模型**: `Systran/faster-whisper-base` → 14s 音频处理 ~3.6s
- **VAD 处理**: 每次 ~0.005–0.01s，碎片化严重（"More than one speech timestamp" 警告）
- 转写慢于 session 生命周期 → 用户停止录制后 ~4s 才返回结果

### 样本率

OpenAI Realtime API 要求 **24000 Hz**（`live.rs:16`），蓝牙耳机麦克风实际 16000 Hz。

### MicAndSpeaker 模式

`AudioDual` → `Single` 时将 mic+speaker 混合为单声道 PCM16（`mix_pcm16` 函数），而非丢弃 speaker 通道。

### Batch fallback 失败根因

Speaches `/v1/audio/transcriptions` 返回 `"logprobs":null`，而 Rust `TranscriptionResponse` 的 `logprobs: Vec<TranscriptionLogprob>` 用 `#[serde(default)]` 只处理缺失字段，不处理显式 `null`。

**✅ 修复**：改为 `#[serde(default, deserialize_with = "deserialize_vec_or_null")]`，与 Realtime API 同一模式（`response.rs:124`）。

测试验证：`curl` 返回 `{"text":"产品配置方案零十二零配置數據中心終於清島清水中心","logprobs":null,"usage":null}` → deserialization 成功。

### Speaches 源码发现

`transcription_response_to_http_response` 函数总是先打 ERROR 日志再处理响应类型（Speaches 的 bug，不影响功能）。

---

## 修改记录

| 日期 | 文件 | 变更 |
|------|------|------|
| Jul 20 | `batch/response.rs` | `TranscriptionResponse.logprobs` 修复 `null` 反序列化 |
| Jul 20 | `batch/response.rs` | `TranscriptionStreamEvent.logprobs` 同样修复 |
| Jul 20 | `live.rs` | 修复 `session.update`：移除 `prefix_padding_ms`、`input_audio_transcription`；仅保留 `turn_detection` |
| Jul 20 | `live.rs` | `initial_message()` 返回 `None`，不再发送 `session.update` |
| Jul 20 | `adapters.rs` | OpenAI 适配器禁用 split dual |
| Jul 20 | `mod.rs` + `listener-core` | `AudioDual` → `Single` 混音（`mix_pcm16`） |
| Jul 20 | `supervisor.rs` | 自动重连：`SessionMsg::RetryLive` + 指数退避（10s→60s cap） |
| Jul 20 | `children.rs` | shutdown 时取消重连任务 |
| Jul 19 | `api.rs` | `CaptureParams` → `SessionParams` → `ListenerArgs` 贯通 `provider` 字段 |
| Jul 19 | `batch/model.rs` | `AudioModel::Custom(String)` 变体 |
| Jul 19 | `select.tsx` + `list-stt.ts` | 动态模型列表 UI |

---

## 下一步

1. [ ] 重新编译测试：batch fallback 不再报 `logprobs: null` 反序列化错误
2. [ ] 验证实时转录结果是否正常展示到 UI
3. [ ] Phase 2: VAD 参数调优、多语言、说话人分离

