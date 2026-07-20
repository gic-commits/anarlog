use crate::{AppExt, embedded_cli::EmbeddedCliStatus};

const STAGING_BUNDLE_ID: &str = "com.hyprnote.staging";

#[tauri::command]
#[specta::specta]
pub async fn get_onboarding_needed<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<bool, String> {
    app.get_onboarding_needed().map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn set_onboarding_needed<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    v: bool,
) -> Result<(), String> {
    app.set_onboarding_needed(v).map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn get_dismissed_toasts<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Vec<String>, String> {
    app.get_dismissed_toasts()
}

#[tauri::command]
#[specta::specta]
pub async fn set_dismissed_toasts<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    v: Vec<String>,
) -> Result<(), String> {
    app.set_dismissed_toasts(v)
}

#[tauri::command]
#[specta::specta]
pub async fn get_env<R: tauri::Runtime>(_app: tauri::AppHandle<R>, key: String) -> String {
    std::env::var(&key).unwrap_or_default()
}

fn should_show_devtool(identifier: &str) -> bool {
    cfg!(any(debug_assertions, feature = "dev", feature = "devtools"))
        || identifier == STAGING_BUNDLE_ID
}

#[tauri::command]
#[specta::specta]
pub fn show_devtool<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> bool {
    should_show_devtool(&app.config().identifier)
}

#[tauri::command]
#[specta::specta]
pub fn complete_app_exit<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    crate::mark_exit_flush_complete();
    app.exit(0);
}

#[tauri::command]
#[specta::specta]
pub async fn get_tinybase_values<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Option<String>, String> {
    app.get_tinybase_values()
}

#[tauri::command]
#[specta::specta]
pub async fn get_pinned_tabs<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Option<String>, String> {
    app.get_pinned_tabs()
}

#[tauri::command]
#[specta::specta]
pub async fn set_pinned_tabs<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    v: String,
) -> Result<(), String> {
    app.set_pinned_tabs(v)
}

#[tauri::command]
#[specta::specta]
pub async fn get_recently_opened_sessions<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Option<String>, String> {
    app.get_recently_opened_sessions()
}

#[tauri::command]
#[specta::specta]
pub async fn set_recently_opened_sessions<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    v: String,
) -> Result<(), String> {
    app.set_recently_opened_sessions(v)
}

#[tauri::command]
#[specta::specta]
pub async fn check_embedded_cli<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<EmbeddedCliStatus, String> {
    Ok(crate::embedded_cli::check(&app))
}

#[tauri::command]
#[specta::specta]
pub async fn install_embedded_cli<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<EmbeddedCliStatus, String> {
    crate::embedded_cli::install(&app)
}

#[tauri::command]
#[specta::specta]
pub async fn fetch_stt_models<R: tauri::Runtime>(
    _app: tauri::AppHandle<R>,
    url: String,
    token: Option<String>,
) -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();
    let mut request = client.get(&url);
    if let Some(key) = &token {
        if !key.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", key));
        }
    }

    let response = request.send().await.map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("API returned {}: {}", status.as_u16(), body));
    }

    let json: serde_json::Value = response.json().await.map_err(|e| format!("parse error: {e}"))?;
    let models = json["data"]
        .as_array()
        .ok_or_else(|| "expected 'data' array in response".to_string())?
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let task = m["task"].as_str();
            let is_stt = task == Some("automatic-speech-recognition")
                || id.to_lowercase().contains("transcribe")
                || id.to_lowercase().contains("whisper")
                || id.to_lowercase().contains("speech")
                || id.to_lowercase().contains("audio");
            if is_stt { Some(id.to_string()) } else { None }
        })
        .collect();

    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_devtools_for_staging_bundle() {
        assert!(should_show_devtool(STAGING_BUNDLE_ID));
    }
}
