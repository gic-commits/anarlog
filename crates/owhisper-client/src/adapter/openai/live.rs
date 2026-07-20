use hypr_ws_client::client::Message;
use owhisper_interface::ListenParams;
use owhisper_interface::stream::{Alternatives, Channel, Metadata, StreamResponse};

use openai_transcription::realtime::{
    ClientEventType, InputAudioBufferAppendEvent, InputAudioBufferCommitEvent, ServerEvent,
};

use crate::adapter::RealtimeSttAdapter;
use crate::adapter::parsing::{WordBuilder, calculate_time_span, parse_speaker_id};

use super::OpenAIAdapter;

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
        _params: &ListenParams,
        _channels: u8,
    ) -> Option<Message> {
        None
    }

    fn parse_response(&self, raw: &str) -> Vec<StreamResponse> {
        tracing::info!("[DEBUG] OpenAI parse_response: raw={}", raw);
        let event: ServerEvent = match serde_json::from_str(raw) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    hyprnote.payload.size_bytes = raw.len() as u64,
                    "openai_realtime_json_parse_failed"
                );
                return vec![];
            }
        };

        match event {
            ServerEvent::SessionCreated { .. } | ServerEvent::SessionUpdated { .. } => {
                tracing::debug!("openai_session_event");
                vec![]
            }
            ServerEvent::InputAudioBufferCommitted { .. }
            | ServerEvent::InputAudioBufferCleared { .. } => vec![],
            ServerEvent::InputAudioBufferSpeechStarted { audio_start_ms, .. } => {
                vec![StreamResponse::SpeechStartedResponse {
                    channel: vec![0],
                    timestamp: audio_start_ms as f64 / 1000.0,
                }]
            }
            ServerEvent::InputAudioBufferSpeechStopped { audio_end_ms, .. } => {
                vec![StreamResponse::UtteranceEndResponse {
                    channel: vec![0],
                    last_word_end: audio_end_ms as f64 / 1000.0,
                }]
            }
            ServerEvent::InputAudioBufferTimeoutTriggered { .. } => vec![],
            ServerEvent::ConversationItemInputAudioTranscriptionCompleted {
                transcript, ..
            } => Self::build_transcript_response(&transcript, true, true),
            ServerEvent::ConversationItemInputAudioTranscriptionDelta { delta, .. } => {
                if delta.is_empty() {
                    return vec![];
                }
                Self::build_transcript_response(&delta, false, false)
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
                vec![StreamResponse::ErrorResponse {
                    error_code: error.code.and_then(|c| c.parse().ok()),
                    error_message: format!(
                        "{}: {}",
                        error.error_type.unwrap_or_default(),
                        error.message.unwrap_or_default()
                    ),
                    provider: "openai".to_string(),
                }]
            }
            ServerEvent::Error { error, .. } => {
                tracing::error!(
                    error.type = ?error.error_type,
                    error = ?error.message,
                    "openai_error"
                );
                vec![StreamResponse::ErrorResponse {
                    error_code: error.code.and_then(|c| c.parse().ok()),
                    error_message: format!(
                        "{}: {}",
                        error.error_type.unwrap_or_default(),
                        error.message.unwrap_or_default()
                    ),
                    provider: "openai".to_string(),
                }]
            }
            ServerEvent::Unknown => {
                tracing::debug!(
                    hyprnote.payload.size_bytes = raw.len() as u64,
                    "openai_unknown_event"
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
        transcript: &str,
        is_final: bool,
        speech_final: bool,
    ) -> Vec<StreamResponse> {
        if transcript.is_empty() {
            return vec![];
        }

        let words: Vec<_> = transcript
            .split_whitespace()
            .map(|word| WordBuilder::new(word).confidence(1.0).build())
            .collect();

        let (start, duration) = calculate_time_span(&words);

        let channel = Channel {
            alternatives: vec![Alternatives {
                transcript: transcript.to_string(),
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
            duration,
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
}
