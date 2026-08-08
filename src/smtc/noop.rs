// Stub SMTC implementation for non-Windows platforms.
use crate::common::SmtcStatus;

pub async fn smtc_status_raw() -> Result<SmtcStatus, String> {
    Ok(SmtcStatus {
        ok: false,
        connected: false,
        error: "SMTC is only available on Windows".to_string(),
        state: "error".to_string(),
        ..Default::default()
    })
}

pub async fn smtc_control(_action: &str, _seek_ms: u64) -> Result<(), String> {
    Err("SMTC control is only available on Windows".to_string())
}

pub async fn smtc_thumbnail() -> Result<(Vec<u8>, String), String> {
    Err("SMTC thumbnail is only available on Windows".to_string())
}
