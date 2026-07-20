use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use super::request::AudioResponseFormat;

pub const MODEL_WHISPER_1: &str = "whisper-1";
pub const MODEL_GPT_4O_TRANSCRIBE: &str = "gpt-4o-transcribe";
pub const MODEL_GPT_4O_MINI_TRANSCRIBE: &str = "gpt-4o-mini-transcribe";
pub const MODEL_GPT_4O_MINI_TRANSCRIBE_2025_12_15: &str = "gpt-4o-mini-transcribe-2025-12-15";
pub const MODEL_GPT_4O_TRANSCRIBE_DIARIZE: &str = "gpt-4o-transcribe-diarize";

pub fn supports_timestamp_granularities(model: impl AsRef<str>) -> bool {
    match model.as_ref().parse::<AudioModel>() {
        Ok(m) => m.supports_timestamp_granularities(),
        Err(_) => false,
    }
}

pub fn default_response_format(model: impl AsRef<str>) -> AudioResponseFormat {
    match model.as_ref().parse::<AudioModel>() {
        Ok(m) => m.default_response_format(),
        Err(_) => AudioResponseFormat::Json,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioModel {
    #[serde(rename = "whisper-1")]
    Whisper1,
    #[serde(rename = "gpt-4o-transcribe")]
    Gpt4oTranscribe,
    #[serde(rename = "gpt-4o-mini-transcribe")]
    Gpt4oMiniTranscribe,
    #[serde(rename = "gpt-4o-mini-transcribe-2025-12-15")]
    Gpt4oMiniTranscribe20251215,
    #[serde(rename = "gpt-4o-transcribe-diarize")]
    Gpt4oTranscribeDiarize,
    Custom(String),
}

impl fmt::Display for AudioModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Whisper1 => write!(f, "whisper-1"),
            Self::Gpt4oTranscribe => write!(f, "gpt-4o-transcribe"),
            Self::Gpt4oMiniTranscribe => write!(f, "gpt-4o-mini-transcribe"),
            Self::Gpt4oMiniTranscribe20251215 => write!(f, "gpt-4o-mini-transcribe-2025-12-15"),
            Self::Gpt4oTranscribeDiarize => write!(f, "gpt-4o-transcribe-diarize"),
            Self::Custom(name) => write!(f, "{}", name),
        }
    }
}

impl FromStr for AudioModel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "whisper-1" => Ok(Self::Whisper1),
            "gpt-4o-transcribe" => Ok(Self::Gpt4oTranscribe),
            "gpt-4o-mini-transcribe" => Ok(Self::Gpt4oMiniTranscribe),
            "gpt-4o-mini-transcribe-2025-12-15" => Ok(Self::Gpt4oMiniTranscribe20251215),
            "gpt-4o-transcribe-diarize" => Ok(Self::Gpt4oTranscribeDiarize),
            other => Ok(Self::Custom(other.to_string())),
        }
    }
}

impl AudioModel {
    pub fn supports_timestamp_granularities(&self) -> bool {
        matches!(self, Self::Whisper1)
    }

    pub fn supports_streaming(&self) -> bool {
        !matches!(self, Self::Whisper1)
    }

    pub fn supports_prompt(&self) -> bool {
        !matches!(self, Self::Gpt4oTranscribeDiarize)
    }

    pub fn supports_logprobs(&self) -> bool {
        matches!(
            self,
            Self::Gpt4oTranscribe | Self::Gpt4oMiniTranscribe | Self::Gpt4oMiniTranscribe20251215
        )
    }

    pub fn default_response_format(&self) -> AudioResponseFormat {
        match self {
            Self::Whisper1 => AudioResponseFormat::VerboseJson,
            Self::Gpt4oTranscribe
            | Self::Gpt4oMiniTranscribe
            | Self::Gpt4oMiniTranscribe20251215
            | Self::Gpt4oTranscribeDiarize
            | Self::Custom(_) => AudioResponseFormat::Json,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GptTranscriptionModel {
    Gpt4oTranscribe,
    Gpt4oMiniTranscribe,
    Gpt4oMiniTranscribe20251215,
}

impl From<GptTranscriptionModel> for AudioModel {
    fn from(value: GptTranscriptionModel) -> Self {
        match value {
            GptTranscriptionModel::Gpt4oTranscribe => Self::Gpt4oTranscribe,
            GptTranscriptionModel::Gpt4oMiniTranscribe => Self::Gpt4oMiniTranscribe,
            GptTranscriptionModel::Gpt4oMiniTranscribe20251215 => Self::Gpt4oMiniTranscribe20251215,
        }
    }
}

impl TryFrom<AudioModel> for GptTranscriptionModel {
    type Error = AudioModel;

    fn try_from(value: AudioModel) -> Result<Self, Self::Error> {
        match value {
            AudioModel::Gpt4oTranscribe => Ok(Self::Gpt4oTranscribe),
            AudioModel::Gpt4oMiniTranscribe => Ok(Self::Gpt4oMiniTranscribe),
            AudioModel::Gpt4oMiniTranscribe20251215 => Ok(Self::Gpt4oMiniTranscribe20251215),
            AudioModel::Custom(_) | AudioModel::Whisper1 | AudioModel::Gpt4oTranscribeDiarize => {
                Err(value)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_response_format_matches_model_capabilities() {
        assert_eq!(
            default_response_format(MODEL_WHISPER_1),
            AudioResponseFormat::VerboseJson
        );
        assert_eq!(
            default_response_format(MODEL_GPT_4O_TRANSCRIBE),
            AudioResponseFormat::Json
        );
    }

    #[test]
    fn audio_model_capabilities_match_api_boundaries() {
        assert!(AudioModel::Whisper1.supports_timestamp_granularities());
        assert!(!AudioModel::Whisper1.supports_streaming());
        assert!(AudioModel::Whisper1.supports_prompt());

        assert!(AudioModel::Gpt4oTranscribe.supports_streaming());
        assert!(AudioModel::Gpt4oTranscribe.supports_logprobs());
        assert!(AudioModel::Gpt4oTranscribe.supports_prompt());

        assert!(!AudioModel::Gpt4oTranscribeDiarize.supports_timestamp_granularities());
        assert!(AudioModel::Gpt4oTranscribeDiarize.supports_streaming());
        assert!(!AudioModel::Gpt4oTranscribeDiarize.supports_logprobs());
        assert!(!AudioModel::Gpt4oTranscribeDiarize.supports_prompt());
    }

    #[test]
    fn serializes_model_enum_to_api_string() {
        let json = serde_json::to_string(&MODEL_GPT_4O_MINI_TRANSCRIBE_2025_12_15)
            .expect("serialize model");

        assert_eq!(json, "\"gpt-4o-mini-transcribe-2025-12-15\"");
    }
}
