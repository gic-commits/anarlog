# speaches Realtime API 问题报告 — 最终方案 + 最新测试验证

## 需要你做的改動（就这三点）

1. **信号量**：`_transcription_semaphore = asyncio.Semaphore(2)`，最多 2 个 VAD 段转录同时跑
2. **分块**：**保留 3s chunk + delta 流式事件**，不要按设备/模型条件分块（`_should_chunk()` 回滚或始终返回 true）
3. **心跳 + shield**：保持现有逻辑不变

不需要改其他东西。

## 最新测试（02:19 UTC）验证了并发确实是根因

### 测试条件

两条录音，分别用 `faster-whisper-base` 和 `faster-whisper-small` 模型。由于 batch 请求和 realtime 共享同一模型池，日志清楚展示了并发争抢的影响。

### 并发对推理速度的影响

**Base 模型 3s chunk 的推理时间变化：**

| 时间点 | 并发状态 | 3s chunk 耗时 |
|--------|---------|-------------|
| 02:19:40 | batch(small) + small handler + base handler 三路并发（ref_count=3） | **59.7s** |
| 02:19:42 | batch(small) + base handler 两路 | **32.8s** |
| 02:19:50 | batch(small) 快完成了，只有 base handler | **10.0s** |
| 02:19:58 | batch(small) 完成 → 只剩 base handler 独占 | **6.6s** |
| 02:20:26 | batch(small) 完全结束 | **5.5s** |
| 02:20:42 | base 独占，稳定状态 | **4.5s** |
| 02:20:46 | base 独占 | **3.9s** |
| 02:20:49 | base 独占 | **3.2s** |
| 02:20:53 | base 独占 | **3.6s** |

**结论：当只有一个活跃模型时，3s chunk 稳定在 3-4s（≈1× 实时），完全可以接受。** 59.7s 的极端慢速是 batch 和 realtime 并发争抢同一模型导致的，不是 CPU 本身慢。

信号量 = 2 可以防止此类争抢：batch 通过 REST 接口走不同路径，realtime 内部最多 2 个 VAD 段同时转录，不会再出现 3 路以上并发。

### 分块必须保留

**分块是 delta 事件的唯一来源。** 对方此前提出的"CPU 慢速时不分块"方案，实际上等价于杀掉 delta 流式输出。从测试数据看，base 独占时每块 3-4s，用户每 3-4s 能看到一段文字流式到达，体验远好于等 30s+ 一次性出结果。

### 0.5s 断连问题

测试日志中 WS 在 handler 启动后 0.5s 断开（`Failed to send message due to disconnect`）。经排查，`parse_response` 返回 `vec![]`（包括 `prefix_padding_ms` 过滤的情况）**不会导致断连** — 空的 `vec![]` 在 flat_map 中产生零个元素，下层流继续工作。

断连的根本原因需要排查：
1. **服务端发了 Close 帧** — 可能 `session.update` 里除了 `prefix_padding_ms` 外还有不支持的字段（我方发送了 `include: [InputAudioTranscriptionLogprobs]`），导致服务端关连
2. **客户端主动关连** — 用户停止录音，WS 正常关闭
3. **建议：你检查服务端日志 02:19:24.478-479 之间是否有 error 事件或 Close 帧记录。我方会在 `client.rs` 的 Close 帧处理处加 close reason 日志**

### 客户端改动

我方已加：
- `parse_response` 日志：每条收到的事件都输出 `event_label` + `raw_type`，包括 Unknown 事件的原始 type 值
- `prefix_padding_ms` 降级为 non-fatal warning，不再断连
- 用 `session.update` 而非 URL query params 做初始化握手
- 暂时禁用 batch fallback（`hadLiveWords` 逻辑），减少干扰变量，专注调通实时流

## 预期行为

```
VAD commit → input_audio_buffer.committed
           → [排队，信号量释放后开始转录]
           → delta {delta: "部分"}          ← 每个 3s chunk 完成后发出
           → delta {delta: "部分文字"}
           → delta {delta: "部分文字出"}
           → completed {transcript: "部分文字出来"}
```

## 复测步骤

1. 你部署三项改动：信号量 2 + 保留分块 + 保持心跳/shield
2. 你检查日志中 02:19:24.478 附近是否有 Close 帧或 error 记录
3. 我方跑一次录音
4. 双方拉日志，对齐事件时间线
