# speaches Realtime API 转录问题报告（第三轮）

## 修复状态

- **Bug 1** `file.seek(0)` — ✅ 已修复
- **Bug 2** `data_w_vad_applied` assert — ✅ 已修复
- **Bug 3** `transcription_client.create()` 内部调用卡死 — ❌ **仍未修复**

## 当前现象

测试 `wss://ztnas.2113111.xyz:28010/v1/realtime?model=Systran/faster-whisper-small&intent=transcription`：

```
session.created
input_audio_buffer.speech_started  (2176ms)
input_audio_buffer.speech_stopped  (3264ms)
input_audio_buffer.committed
conversation.item.added
conversation.item.done
--- 卡死在这里，永不产生 transcription.completed ---
```

外部 `curl POST /v1/audio/transcriptions` 传同一段音频（2176–3264ms 切片）正常返回 `"No."`。

## 服务器日志分析

你们提供的 `/v1/logs` 端点非常有帮助。以下是完整流程的日志时间线：

```
15:38:35,840 - Creating transcription client with base_url=http://127.0.0.1:8000/v1
15:38:35,862 - Accepted websocket connection with intent: transcription
15:38:35,884 - Transcription-only mode: using Systran/faster-whisper-small
...
15:38:39,856 - Sent input_audio_buffer.speech_started
15:38:41,487 - Sent input_audio_buffer.speech_stopped
15:38:41,488 - Sent input_audio_buffer.committed
15:38:41,489 - Transcription _handler started: model=Systran/faster-whisper-small, language=en, input_audio_duration=3.26s, wav_size=34860
15:38:41,491 - Sent conversation.item.added
15:38:41,493 - Sent conversation.item.done
15:38:41,566~42,578 - VAD processing silence chunks (0.07s~0.73s)
15:38:44,676 - Failed to receive message due to disconnect
```

**关键发现**：
- `_handler started` 之后，`conversation.item.added/done` 正常发出（约 4ms 内完成）
- 然后**没有 STT 模型加载日志、没有 transcription_client.create() 的请求日志、没有错误日志**
- `wav_size=34860` 说明 WAV 文件已经正确生成（file.seek 已修）
- 但 `transcription_client.create(file=file, ...)` 的 HTTP 请求从未到达 STT 路由

## 根因分析

日志显示 `get_transcription_client()` 使用的 URL 是 `http://127.0.0.1:8000/v1`。这说明你们改用了 HTTP 请求（而不是 ASGITransport）。

问题在于：`transcription_client.create()` 通过 httpx 发 POST 到 `http://127.0.0.1:8000/v1/audio/transcriptions` 时，请求**没有到达** STT 端点。可能原因：

1. **连接问题** — httpx 连不上 `127.0.0.1:8000`，但 httpx 默认不设置连接超时，导致 `connect()` 一直挂起
2. **请求被吞掉** — 某些中间件或网络配置导致请求未到达 FastAPI

## 排查建议

### 1. 从容器内部测试 STT 端点

在服务器容器内执行：
```bash
curl -v --max-time 30 http://127.0.0.1:8000/v1/audio/transcriptions \
  -F "file=@test.wav" \
  -F "model=Systran/faster-whisper-small" \
  -F "response_format=text"
```

确认内部 HTTP 调用能正常工作。

### 2. 给转录调用加超时和异常日志

`src/speaches/realtime/input_audio_buffer.py` 的 `_handler()` 中：

```python
try:
    transcript = await asyncio.wait_for(
        self.transcription_client.create(
            file=file,
            model=self.session.input_audio_transcription.model,
            response_format="text",
            language=self.session.input_audio_transcription.language or omit,
        ),
        timeout=30.0
    )
except asyncio.TimeoutError:
    logger.error("Transcription timed out after 30s")
    return
except Exception as e:
    logger.exception(f"Transcription failed: {e}")
    return
```

### 3. 给 httpx 客户端显式设置超时

`src/speaches/dependencies.py` 的 `get_transcription_client()` 中：

```python
http_client = AsyncClient(
    base_url="http://127.0.0.1:8000/v1",
    timeout=httpx.Timeout(60.0, connect=5.0, read=60.0),
)
```

### 4. 或者回退到 ASGITransport

ASGITransport 不经过网络栈，可以避免连接问题：

```python
from fastapi import FastAPI

tmp_app = FastAPI()
tmp_app.include_router(stt_router)
http_client = AsyncClient(
    transport=ASGITransport(tmp_app),
    base_url="http://test/v1",
    timeout=httpx.Timeout(60.0),
)
```

## 测试脚本

你们可以用这个 Node.js 脚本复现（与测试环境一致）：

```javascript
const WebSocket = require('ws');
const fs = require('fs');
const pcmData = fs.readFileSync('audio.wav').subarray(44);
const chunk = pcmData.subarray(0, 128000);

const ws = new WebSocket(
  'wss://<host>/v1/realtime?model=Systran/faster-whisper-small&intent=transcription&language=en',
  { headers: { 'Authorization': 'Bearer <key>' } }
);

ws.on('message', (data) => {
  const msg = JSON.parse(data.toString());
  if (msg.type.includes('transcription')) console.log(msg);
});

ws.on('open', () => {
  let offset = 0;
  const iv = setInterval(() => {
    if (offset >= chunk.length) {
      clearInterval(iv);
      let s = 0;
      const siv = setInterval(() => {
        if (s >= 20) { clearInterval(siv); return; }
        ws.send(JSON.stringify({ type: 'input_audio_buffer.append', audio: Buffer.alloc(3200).toString('base64') }));
        s++;
      }, 100);
      return;
    }
    ws.send(JSON.stringify({ type: 'input_audio_buffer.append', audio: chunk.subarray(offset, offset+3200).toString('base64') }));
    offset += 3200;
  }, 100);
});
```
