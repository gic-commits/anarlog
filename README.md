# anarlog

[English](README.md) · [简体中文](README.zh-CN.md)

An open-source AI meeting notetaker that is **local-first**,
**privacy-first**, and yours to fork.

This is a feature fork of
[fastrepl/anarlog](https://github.com/fastrepl/anarlog), forked and
actively developed by [gic-commits](https://github.com/gic-commits).

Granola, rearranged.

## What's new in this fork

Beyond the upstream anarlog base, this fork adds deep speech-to-text
engineering focused on **local-first transcription, speaker
diarization, and self-hosted, OpenAI-compatible STT providers**:

- **Progressive Batch hybrid mode** — during a recording, audio is
  segmented on the fly by a streaming VAD and submitted in parallel to
  a batch transcription endpoint, then stitched into one timeline.
  Long recordings transcribe while you record, with results appearing
  incrementally.
- **Local speaker diarization** — a fully on-device pipeline (pyannote
  segmentation ONNX + Wespeaker speaker embeddings + cosine clustering
  + adaptive threshold) that labels who spoke when, without sending
  audio anywhere.
- **Streaming VAD (Min-Cut + Merge)** — speech-segment grouping with
  energy-based turn chunking, idle-triggered emit, and pause-driven
  submission.
- **CJK post-processing** — optional jieba word segmentation and
  acoustic merging tuned for Chinese, Korean, and Japanese transcripts
  (local and/or server-side).
- **Self-hosted / OpenAI-compatible STT** — connect any
  OpenAI-compatible transcription server (e.g. a self-hosted
  [speaches](https://github.com/gic-commits/speaches) instance) for
  realtime WebSocket or batch transcription.
- **Groq STT support** — batch transcription with client-side
  segmentation, WAV container wrapping, and 429 rate-limit backoff.
  Whisper-large-v3(-turbo) at up to 216× realtime.
- **Local Whisper fallback** — ggml Whisper (tiny/small) runs fully
  offline as a no-network fallback.
- **Realtime (WebSocket) transcription** — OpenAI
  Realtime-API-compatible streaming with VAD, keepalive,
  auto-reconnect, and `samples_dropped` instrumentation.

## How to use it

Download the latest release for your platform:

→ [github.com/gic-commits/anarlog/releases/latest](https://github.com/gic-commits/anarlog/releases/latest)

Open it and join a meeting. anarlog records, transcribes (locally or
via your provider of choice), and saves your notes as markdown on disk.
Bring your own LLM: OpenAI, Anthropic, Gemini, OpenRouter, Ollama,
LM Studio, Groq, or anything OpenAI-compatible.

To self-host, clone the repo, build it, and run it.

## Why use it

- **Your data, your disk.** Every meeting is a `.md` file you can
  inspect, search, and sync through Dropbox, iCloud, Syncthing, or git.
  No cloud backend means no cloud lock-in.
- **Local transcription.** Transcription runs on-device or on your own
  server, so audio can stay on your machine.
- **Speaker diarization on-device.** Who said what is computed locally
  with small ONNX models — no audio leaves your device.
- **Bring your own AI.** Use any LLM and STT provider, including
  OpenAI-compatible services, self-hosted servers, and local models.
- **Open source, MIT.** Fork it, sell it, or self-host it.
- **No accounts or tracking.** There is no hosted account model.

## Development

- Tauri desktop app in `apps/desktop/`, web app in `apps/web/`.
- Rust workspace with pnpm workspaces. SQLite is the primary data store.
- See the upstream docs at [docs.anarlog.so](https://docs.anarlog.so).

## Name history

**anarlog** started as **Hyprnote**, then briefly used the **char** name.

We later split the work into two projects.
**[char](https://char.com)** is the team's current productivity app.
**anarlog** is this open-source, local-first meeting notetaker.

This repository is not the current char codebase, and anarlog is not
being retired. It keeps the open-source path: MIT-licensed, forkable,
self-hostable, and built for local notes you control.

If you came here from Granola, welcome.
If you came here from Hyprnote, welcome back.

Either way, it's yours.

---

**License:** MIT · **Upstream:**
[fastrepl/anarlog](https://github.com/fastrepl/anarlog) · **Maintainers:**
[fastrepl](https://github.com/fastrepl)
