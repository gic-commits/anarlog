# anarlog

[English](README.md) · [简体中文](README.zh-CN.md)

一个**本地优先**、**隐私优先**的开源 AI 会议记录工具，欢迎 fork 使用。

这是 [fastrepl/anarlog](https://github.com/fastrepl/anarlog)
的功能分支（fork），由 [gic-commits](https://github.com/gic-commits)
fork 并持续开发维护。

Granola，重新编排。

## 本分支新增功能

在上游 anarlog 的基础上，本分支围绕**本地优先转录、说话人分离、
以及自建/OpenAI 兼容 STT** 做了深度的语音识别工程：

- **Progressive Batch 混合模式** —— 录音过程中通过流式 VAD 实时
  分段，并行提交到 batch 转录端点，再拼接成统一时间轴。长录音
  边录边转，结果增量显示。
- **本地说话人分离（diarization）** —— 完全本地的流水线（pyannote
  分割 ONNX + Wespeaker 说话人嵌入 + 余弦聚类 + 自适应阈值），
  本地标注"谁在什么时候说话"，音频不出设备。
- **流式 VAD（Min-Cut + Merge）** —— 语音分段分组、基于能量的
  停顿切分、空闲触发提交、按停顿调度提交。
- **CJK 后处理** —— 可选的 jieba 分词与声学合并，为中文、韩语、
  日语转录优化（本地和/或服务端）。
- **自建 / OpenAI 兼容 STT** —— 可接入任意 OpenAI 兼容转录服务器
  （例如自建 [speaches](https://github.com/gic-commits/speaches)
  实例），支持实时 WebSocket 或 batch 转录。
- **Groq STT 支持** —— batch 转录 + 客户端分段 + WAV 容器封装 +
  429 限速退避。Whisper-large-v3(-turbo) 最高可达 216× 实时速度。
- **本地 Whisper 兜底** —— ggml Whisper（tiny/small）完全离线运行，
  作为无网络时的兜底方案。
- **实时（WebSocket）转录** —— OpenAI Realtime API 兼容的流式转录，
  支持 VAD、心跳保活、自动重连，以及 `samples_dropped` 埋点监控。

## 使用方法

从以下地址下载你平台的最新版本：

→ [github.com/gic-commits/anarlog/releases/latest](https://github.com/gic-commits/anarlog/releases/latest)

打开后加入会议。anarlog 会录音、转录（本地或你选的 provider），
并把笔记以 markdown 保存到磁盘。自带 LLM：OpenAI、Anthropic、
Gemini、OpenRouter、Ollama、LM Studio、Groq，或任何 OpenAI 兼容服务。

自托管：clone 仓库，构建并运行即可。

## 为什么用它

- **数据在你手里。** 每场会议都是可检查、可搜索、可同步
  （Dropbox、iCloud、Syncthing 或 git）的 `.md` 文件。无云端后端，
  无云端锁定。
- **本地转录。** 转录在设备端或你自己的服务器上运行，音频可以
  不出你的机器。
- **本地说话人分离。** "谁说了什么"用小型 ONNX 模型在本地计算
  —— 音频不出设备。
- **自带 AI。** 可使用任意 LLM 和 STT provider，包括 OpenAI 兼容
  服务、自建服务器、本地模型。
- **开源，MIT。** 可以 fork、出售，也可以自托管。
- **无账号、无追踪。** 没有托管账号模式。

## 开发

- Tauri 桌面应用位于 `apps/desktop/`，Web 应用位于 `apps/web/`。
- Rust workspace + pnpm workspaces。SQLite 是主要数据存储。
- 上游文档见 [docs.anarlog.so](https://docs.anarlog.so)。

## 名称沿革

**anarlog** 起初叫 **Hyprnote**，之后短暂使用过 **char** 这个名字。

后来我们把工作拆分成两个项目。**[char](https://char.com)** 是团队
的当前效率应用。**anarlog** 是这个开源的、本地优先的会议记录工具。

本仓库不是当前的 char 代码库，anarlog 也不会被废弃。它继续走开源
路线：MIT 协议、可 fork、可自托管，为你能掌控的本地笔记而生。

如果你从 Granola 而来，欢迎。如果你从 Hyprnote 而来，欢迎回来。

无论如何，它都是你的。

---

**License：** MIT · **上游：** [fastrepl/anarlog](https://github.com/fastrepl/anarlog)
· **维护者：** [fastrepl](https://github.com/fastrepl)
