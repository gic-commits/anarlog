use std::collections::HashMap;
use std::sync::Mutex;

use hypr_ws_client::client::Message;
use owhisper_interface::ListenParams;
use owhisper_interface::stream::{Alternatives, Channel, Metadata, StreamResponse};

use openai_transcription::realtime::{
    AudioConfig, AudioFormat, AudioFormatType, AudioInputConfig, ClientEventType,
    InputAudioBufferAppendEvent, InputAudioBufferCommitEvent, ServerEvent, SessionConfig,
    SessionInclude, SessionUpdateEvent, TranscriptionConfig, TurnDetectionConfig,
    TurnDetectionType,
};

use crate::adapter::RealtimeSttAdapter;
use crate::adapter::parsing::{WordBuilder, parse_speaker_id};

use super::OpenAIAdapter;

const WORD_DURATION_SECS: f64 = 0.1;

static ITEM_TIMING: Mutex<Option<ItemTimingState>> = Mutex::new(None);

struct ItemTimingState {
    items: HashMap<String, (f64, f64)>,
    cursor: f64,
}

impl ItemTimingState {
    fn new() -> Self {
        Self {
            items: HashMap::new(),
            cursor: 0.0,
        }
    }

    fn ensure_cursor_after(&mut self, time: f64) {
        if time > self.cursor {
            self.cursor = time;
        }
    }

    fn get_or_alloc(&mut self, item_id: &str, word_count: usize) -> (f64, f64) {
        if let Some(&range) = self.items.get(item_id) {
            return range;
        }
        let total_duration = (word_count as f64) * WORD_DURATION_SECS;
        let start = self.cursor;
        let end = start + total_duration;
        self.items.insert(item_id.to_string(), (start, end));
        (start, end)
    }

    fn finalize_item(&mut self, item_id: &str, word_count: usize) -> (f64, f64) {
        let range = self.get_or_alloc(item_id, word_count);
        let total_duration = (word_count as f64) * WORD_DURATION_SECS;
        self.ensure_cursor_after(range.0 + total_duration);
        self.items.remove(item_id);
        range
    }
}

fn with_item_timing<F, T>(f: F) -> T
where
    F: FnOnce(&mut ItemTimingState) -> T,
{
    let mut guard = ITEM_TIMING.lock().unwrap();
    let state = guard.get_or_insert_with(ItemTimingState::new);
    f(state)
}

impl RealtimeSttAdapter for OpenAIAdapter {
    fn provider_name(&self) -> &'static str {
        "openai"
    }

    fn is_supported_languages(
        &self,
        languages: &[hypr_language::Language],
        _model: Option<&str>,
    ) -> bool {
        OpenAIAdapter::is_supported_languages_batch(languages)
    }

    fn supports_native_multichannel(&self) -> bool {
        false
    }

    fn build_ws_url(&self, api_base: &str, params: &ListenParams, _channels: u8) -> url::Url {
        let (mut url, existing_params) = Self::build_ws_url_from_base(api_base);

        let default = crate::providers::Provider::OpenAI.default_live_model();
        let model = match params.model.as_deref() {
            Some(m) if crate::providers::is_meta_model(m) => default,
            Some(m) => m,
            None => default,
        };

        {
            let mut query_pairs = url.query_pairs_mut();
            for (key, value) in &existing_params {
                query_pairs.append_pair(key, value);
            }
            query_pairs.append_pair("model", model);
            query_pairs.append_pair("intent", "transcription");
            if let Some(lang) = params.languages.first() {
                query_pairs.append_pair("language", lang.iso639().code());
            }
        }

        tracing::info!(
            "[DEBUG] OpenAI live build_ws_url: api_base={} final_url={} model={}",
            api_base,
            url,
            model
        );
        url
    }

    fn build_auth_header(&self, api_key: Option<&str>) -> Option<(&'static str, String)> {
        api_key.and_then(|k| crate::providers::Provider::OpenAI.build_auth_header(k))
    }

    fn keep_alive_message(&self) -> Option<Message> {
        None
    }

    fn finalize_message(&self) -> Message {
        let msg = InputAudioBufferCommitEvent {
            event_id: None,
            event_type: ClientEventType::InputAudioBufferCommit,
        };
        Message::Text(serde_json::to_string(&msg).unwrap().into())
    }

    fn audio_to_message(&self, audio: bytes::Bytes) -> Message {
        use base64::Engine;
        let base64_audio = base64::engine::general_purpose::STANDARD.encode(&audio);
        let event = InputAudioBufferAppendEvent {
            event_id: None,
            event_type: ClientEventType::InputAudioBufferAppend,
            audio: base64_audio,
        };
        Message::Text(serde_json::to_string(&event).unwrap().into())
    }

    fn initial_message(
        &self,
        _api_key: Option<&str>,
        params: &ListenParams,
        _channels: u8,
    ) -> Option<Message> {
        let model = params
            .model
            .clone()
            .unwrap_or_else(|| "whisper-1".to_string());
        let language = params
            .languages
            .first()
            .map(|l| l.iso639().code().to_string());

        let event = SessionUpdateEvent {
            event_id: None,
            event_type: ClientEventType::SessionUpdate,
            session: SessionConfig {
                audio: Some(AudioConfig {
                    input: Some(AudioInputConfig {
                        format: Some(AudioFormat {
                            format_type: AudioFormatType::AudioPcm,
                            rate: Some(24_000),
                        }),
                        noise_reduction: None,
                    }),
                }),
                include: Some(vec![SessionInclude::InputAudioTranscriptionLogprobs]),
                input_audio_transcription: Some(TranscriptionConfig {
                    model,
                    language,
                    prompt: if params.keywords.is_empty() {
                        None
                    } else {
                        Some(params.keywords.join(" "))
                    },
                }),
                turn_detection: Some(TurnDetectionConfig {
                    detection_type: TurnDetectionType::ServerVad,
                    create_response: Some(false),
                    interrupt_response: None,
                    idle_timeout_ms: None,
                    eagerness: None,
                    threshold: Some(0.9),
                    prefix_padding_ms: None,
                    silence_duration_ms: Some(1500),
                }),
            },
        };

        let json = serde_json::to_string(&event).ok()?;
        Some(Message::Text(json.into()))
    }

    fn parse_response(&self, raw: &str) -> Vec<StreamResponse> {
        tracing::info!("[DEBUG] OpenAI parse_response: raw={}", raw);

        let raw_type: Option<String> = serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str().map(String::from)));

        let event: ServerEvent = match serde_json::from_str(raw) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    event_type = ?raw_type,
                    error = ?e,
                    hyprnote.payload.size_bytes = raw.len() as u64,
                    "openai_realtime_json_parse_failed"
                );
                return vec![];
            }
        };

        let event_label = match &event {
            ServerEvent::SessionCreated { .. } => "session.created",
            ServerEvent::SessionUpdated { .. } => "session.updated",
            ServerEvent::InputAudioBufferCommitted { .. } => "input_audio_buffer.committed",
            ServerEvent::InputAudioBufferCleared { .. } => "input_audio_buffer.cleared",
            ServerEvent::InputAudioBufferSpeechStarted { .. } => {
                "input_audio_buffer.speech_started"
            }
            ServerEvent::InputAudioBufferSpeechStopped { .. } => {
                "input_audio_buffer.speech_stopped"
            }
            ServerEvent::InputAudioBufferTimeoutTriggered { .. } => {
                "input_audio_buffer.timeout_triggered"
            }
            ServerEvent::ConversationItemInputAudioTranscriptionCompleted { .. } => {
                "conversation.item.input_audio_transcription.completed"
            }
            ServerEvent::ConversationItemInputAudioTranscriptionDelta { .. } => {
                "conversation.item.input_audio_transcription.delta"
            }
            ServerEvent::ConversationItemInputAudioTranscriptionSegment { .. } => {
                "conversation.item.input_audio_transcription.segment"
            }
            ServerEvent::ConversationItemInputAudioTranscriptionFailed { .. } => {
                "conversation.item.input_audio_transcription.failed"
            }
            ServerEvent::Error { .. } => "error",
            ServerEvent::Unknown => raw_type.as_deref().unwrap_or("unknown"),
        };

        tracing::info!(
            event_label,
            raw_type = ?raw_type,
            "openai_event"
        );

        match event {
            ServerEvent::SessionCreated { .. } | ServerEvent::SessionUpdated { .. } => {
                vec![]
            }
            ServerEvent::InputAudioBufferCommitted { .. }
            | ServerEvent::InputAudioBufferCleared { .. } => vec![],
            ServerEvent::InputAudioBufferSpeechStarted { audio_start_ms, .. } => {
                with_item_timing(|state| {
                    state.ensure_cursor_after(audio_start_ms as f64 / 1000.0);
                });
                vec![StreamResponse::SpeechStartedResponse {
                    channel: vec![0],
                    timestamp: audio_start_ms as f64 / 1000.0,
                }]
            }
            ServerEvent::InputAudioBufferSpeechStopped { audio_end_ms, .. } => {
                with_item_timing(|state| {
                    state.ensure_cursor_after(audio_end_ms as f64 / 1000.0);
                });
                vec![StreamResponse::UtteranceEndResponse {
                    channel: vec![0],
                    last_word_end: audio_end_ms as f64 / 1000.0,
                }]
            }
            ServerEvent::InputAudioBufferTimeoutTriggered { .. } => vec![],
            ServerEvent::ConversationItemInputAudioTranscriptionCompleted {
                transcript,
                item_id,
                ..
            } => Self::build_transcript_response(&transcript, &item_id, true, true),
            ServerEvent::ConversationItemInputAudioTranscriptionDelta {
                delta, item_id, ..
            } => {
                if delta.is_empty() {
                    return vec![];
                }
                Self::build_transcript_response(&delta, &item_id, false, false)
            }
            ServerEvent::ConversationItemInputAudioTranscriptionSegment {
                text,
                start,
                end,
                speaker,
                ..
            } => {
                if text.is_empty() {
                    return vec![];
                }
                Self::build_transcript_response_with_speaker(&text, start, end, true, true, speaker)
            }
            ServerEvent::ConversationItemInputAudioTranscriptionFailed { error, .. } => {
                tracing::error!(
                    error.type = ?error.error_type,
                    error = ?error.message,
                    "openai_transcription_failed"
                );
                let error_message = error.message.unwrap_or_default();
                vec![StreamResponse::ErrorResponse {
                    error_code: error.code.and_then(|c| c.parse().ok()),
                    error_message: format!(
                        "{}: {}",
                        error.error_type.unwrap_or_default(),
                        error_message
                    ),
                    provider: "openai".to_string(),
                }]
            }
            ServerEvent::Error { error, .. } => {
                let msg = error.message.as_deref().unwrap_or_default();
                if msg.contains("prefix_padding_ms") {
                    tracing::warn!(
                        error.type = ?error.error_type,
                        error = ?msg,
                        "openai_non_fatal_config_warning"
                    );
                    return vec![];
                }
                tracing::error!(
                    error.type = ?error.error_type,
                    error = ?msg,
                    "openai_error"
                );
                vec![StreamResponse::ErrorResponse {
                    error_code: error.code.and_then(|c| c.parse().ok()),
                    error_message: format!("{}: {}", error.error_type.unwrap_or_default(), msg),
                    provider: "openai".to_string(),
                }]
            }
            ServerEvent::Unknown => {
                tracing::warn!(
                    raw_type = ?raw_type,
                    hyprnote.payload.size_bytes = raw.len() as u64,
                    "openai_unrecognized_event"
                );
                vec![]
            }
        }
    }
}

impl OpenAIAdapter {
    pub(crate) fn build_ws_url_from_base(api_base: &str) -> (url::Url, Vec<(String, String)>) {
        crate::adapter::build_ws_url_from_base_with(
            crate::providers::Provider::OpenAI,
            api_base,
            |parsed: &url::Url| {
                let host = parsed
                    .host_str()
                    .unwrap_or(crate::providers::Provider::OpenAI.default_ws_host());
                let mut url: url::Url = format!(
                    "wss://{}{}",
                    host,
                    crate::providers::Provider::OpenAI.ws_path()
                )
                .parse()
                .expect("invalid_ws_url");
                if let Some(port) = parsed.port() {
                    let _ = url.set_port(Some(port));
                }
                crate::adapter::set_scheme_from_host(&mut url);
                url
            },
        )
    }

    fn build_transcript_response(
        text: &str,
        item_id: &str,
        is_final: bool,
        speech_final: bool,
    ) -> Vec<StreamResponse> {
        if text.is_empty() {
            return vec![];
        }

        let word_count = text.split_whitespace().count();
        let (start, end) = if is_final {
            with_item_timing(|state| state.finalize_item(item_id, word_count))
        } else {
            with_item_timing(|state| state.get_or_alloc(item_id, word_count))
        };

        let word_duration = if word_count > 0 {
            (end - start) / word_count as f64
        } else {
            0.0
        };

        let words: Vec<_> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, word)| {
                WordBuilder::new(word)
                    .start(start + word_duration * i as f64)
                    .end(start + word_duration * (i + 1) as f64)
                    .confidence(1.0)
                    .build()
            })
            .collect();

        let channel = Channel {
            alternatives: vec![Alternatives {
                transcript: text.to_string(),
                words,
                confidence: 1.0,
                languages: vec![],
            }],
        };

        vec![StreamResponse::TranscriptResponse {
            is_final,
            speech_final,
            from_finalize: false,
            start,
            duration: end - start,
            channel,
            metadata: Metadata::default(),
            channel_index: vec![0, 1],
        }]
    }

    fn build_transcript_response_with_speaker(
        text: &str,
        start: f64,
        end: f64,
        is_final: bool,
        speech_final: bool,
        speaker: Option<String>,
    ) -> Vec<StreamResponse> {
        if text.is_empty() {
            return vec![];
        }

        let speaker_index = speaker
            .as_deref()
            .and_then(|s| parse_speaker_id(s).map(|id| id as i32));

        let word_count = text.split_whitespace().count() as f64;
        let word_duration = if word_count > 0.0 {
            (end - start) / word_count
        } else {
            0.0
        };

        let words: Vec<_> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, word)| {
                WordBuilder::new(word)
                    .start(start + word_duration * i as f64)
                    .end(start + word_duration * (i + 1) as f64)
                    .confidence(1.0)
                    .speaker(speaker_index)
                    .build()
            })
            .collect();

        let channel = Channel {
            alternatives: vec![Alternatives {
                transcript: text.to_string(),
                words,
                confidence: 1.0,
                languages: vec![],
            }],
        };

        vec![StreamResponse::TranscriptResponse {
            is_final,
            speech_final,
            from_finalize: false,
            start,
            duration: end - start,
            channel,
            metadata: Metadata::default(),
            channel_index: vec![0, 1],
        }]
    }
}

#[cfg(test)]
mod tests {
    use hypr_language::ISO639;

    use super::OpenAIAdapter;
    use crate::ListenClient;
    use crate::test_utils::{UrlTestCase, run_url_test_cases};

    const API_BASE: &str = "wss://api.openai.com";

    #[test]
    fn test_base_url() {
        run_url_test_cases(
            &OpenAIAdapter::default(),
            API_BASE,
            &[UrlTestCase {
                name: "base_url_structure",
                model: None,
                languages: &[ISO639::En],
                contains: &["api.openai.com"],
                not_contains: &[],
            }],
        );
    }

    #[test]
    fn test_basic_url_params() {
        run_url_test_cases(
            &OpenAIAdapter::default(),
            API_BASE,
            &[UrlTestCase {
                name: "basic_params",
                model: Some("gpt-4o-transcribe"),
                languages: &[ISO639::En],
                contains: &[
                    "model=gpt-4o-transcribe",
                    "intent=transcription",
                    "language=en",
                ],
                not_contains: &[],
            }],
        );
    }

    #[test]
    fn test_multiple_languages_uses_first() {
        run_url_test_cases(
            &OpenAIAdapter::default(),
            API_BASE,
            &[UrlTestCase {
                name: "multi_lang",
                model: Some("whisper-1"),
                languages: &[ISO639::En, ISO639::Es],
                contains: &["language=en"],
                not_contains: &["language=es"],
            }],
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_build_single() {
        let client = ListenClient::builder()
            .adapter::<OpenAIAdapter>()
            .api_base("wss://api.openai.com")
            .api_key(std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY not set"))
            .params(owhisper_interface::ListenParams {
                model: Some("gpt-4o-transcribe".to_string()),
                languages: vec![hypr_language::ISO639::En.into()],
                sample_rate: 16000,
                ..Default::default()
            })
            .build_single()
            .await;

        crate::test_utils::run_single_test(client, "openai").await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_build_dual() {
        let client = ListenClient::builder()
            .adapter::<OpenAIAdapter>()
            .api_base("wss://api.openai.com")
            .api_key(std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY not set"))
            .params(owhisper_interface::ListenParams {
                model: Some("gpt-4o-transcribe".to_string()),
                languages: vec![hypr_language::ISO639::En.into()],
                sample_rate: 16000,
                ..Default::default()
            })
            .build_dual()
            .await;

        crate::test_utils::run_dual_test(client, "openai").await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_continuous_streaming() {
        use crate::live::FinalizeHandle;
        use bytes::Bytes;
        use futures_util::StreamExt;
        use hypr_audio_utils::AudioFormatExt;
        use owhisper_interface::MixedMessage;
        use owhisper_interface::stream::StreamResponse;
        use std::time::Duration;

        const SAMPLE_RATE: u32 = 24_000;
        const CHUNK_SAMPLES: usize = 2400;
        const SILENCE_CHUNKS: usize = 30;
        const SEGMENT_CHUNKS: usize = 80;
        const SEGMENTS: usize = 3;

        let _ = tracing_subscriber::fmt::try_init();

        let all_chunks: Vec<Bytes> = rodio::Decoder::new(std::io::BufReader::new(
            std::fs::File::open(hypr_data::english_2::AUDIO_PATH).unwrap(),
        ))
        .unwrap()
        .to_i16_le_chunks(SAMPLE_RATE, CHUNK_SAMPLES)
        .collect::<Vec<Bytes>>()
        .await;

        tracing::info!(
            "Loaded {} chunks from audio ({}s at {} Hz)",
            all_chunks.len(),
            (all_chunks.len() as f64 * CHUNK_SAMPLES as f64) / SAMPLE_RATE as f64,
            SAMPLE_RATE,
        );

        let silence_chunk = Bytes::from(vec![0u8; CHUNK_SAMPLES * 2]);
        let mut messages: Vec<MixedMessage<Bytes, owhisper_interface::ControlMessage>> = Vec::new();

        for seg in 0..SEGMENTS {
            let start = seg * SEGMENT_CHUNKS;
            let end = (start + SEGMENT_CHUNKS).min(all_chunks.len());
            for chunk in &all_chunks[start..end] {
                messages.push(MixedMessage::Audio(chunk.clone()));
            }
            if seg < SEGMENTS - 1 {
                for _ in 0..SILENCE_CHUNKS {
                    messages.push(MixedMessage::Audio(silence_chunk.clone()));
                }
            }
        }

        for _ in 0..SILENCE_CHUNKS {
            messages.push(MixedMessage::Audio(silence_chunk.clone()));
        }

        tracing::info!(
            "Built {} messages (~{}s total)",
            messages.len(),
            (messages.len() as f64 * CHUNK_SAMPLES as f64) / SAMPLE_RATE as f64,
        );

        let audio_stream = futures_util::stream::iter(messages);
        let audio_stream = Box::pin(tokio_stream::StreamExt::throttle(
            audio_stream,
            Duration::from_millis(100),
        ));

        let ws_url =
            std::env::var("OPENAI_WS_URL").unwrap_or_else(|_| "wss://api.openai.com".to_string());
        let api_key = std::env::var("OPENAI_API_KEY").ok();
        let model = std::env::var("OPENAI_STT_MODEL")
            .unwrap_or_else(|_| "Systran/faster-whisper-small".to_string());

        let mut builder = crate::ListenClient::builder()
            .adapter::<OpenAIAdapter>()
            .api_base(&ws_url)
            .params(owhisper_interface::ListenParams {
                model: Some(model),
                languages: vec![hypr_language::ISO639::En.into()],
                sample_rate: SAMPLE_RATE,
                ..Default::default()
            });
        if let Some(key) = &api_key {
            builder = builder.api_key(key);
        }
        let client = builder.build_single().await;

        tracing::info!("Starting continuous streaming test...");
        let (stream, handle) = client.from_realtime_audio(audio_stream).await.unwrap();
        tokio::pin!(stream);

        let timeout = Duration::from_secs(120);
        let mut speech_events = 0u32;
        let mut utterance_events = 0u32;
        let mut transcripts: Vec<String> = Vec::new();

        let test_future = async {
            while let Some(result) = stream.next().await {
                match result {
                    Ok(StreamResponse::TranscriptResponse {
                        channel, is_final, ..
                    }) => {
                        if let Some(alt) = channel.alternatives.first()
                            && !alt.transcript.is_empty()
                        {
                            let text = alt.transcript.clone();
                            transcripts.push(format!("\"{}\" (is_final={})", text, is_final));
                            tracing::info!("delta: \"{}\" (is_final={})", text, is_final);
                        }
                    }
                    Ok(StreamResponse::SpeechStartedResponse { .. }) => {
                        speech_events += 1;
                        tracing::info!("SpeechStarted #{}", speech_events);
                    }
                    Ok(StreamResponse::UtteranceEndResponse { .. }) => {
                        utterance_events += 1;
                        tracing::info!("UtteranceEnd #{}", utterance_events);
                    }
                    Ok(StreamResponse::ErrorResponse {
                        error_message,
                        error_code,
                        ..
                    }) => {
                        tracing::error!("Server error (code={:?}): {}", error_code, error_message);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("Stream error: {:?}", e);
                    }
                }
            }
        };

        let _ = tokio::time::timeout(timeout, test_future).await;
        handle.finalize().await;

        tracing::info!("=== Continuous Streaming Test Results ===");
        tracing::info!(
            "SpeechStarted: {}  UtteranceEnd: {}",
            speech_events,
            utterance_events
        );
        tracing::info!("Non-empty transcripts: {}", transcripts.len());
        for t in &transcripts {
            tracing::info!("  {}", t);
        }
        if transcripts.is_empty() && speech_events > 0 {
            tracing::warn!(
                "VAD triggered {} times but returned empty transcripts — check speaches STT model",
                speech_events
            );
        }

        assert!(
            speech_events >= 2,
            "Expected >= 2 SpeechStarted, got {}. VAD may not be triggering.",
            speech_events,
        );
        assert!(
            utterance_events >= 2,
            "Expected >= 2 UtteranceEnd, got {}. VAD may not be triggering.",
            utterance_events,
        );
    }
}
