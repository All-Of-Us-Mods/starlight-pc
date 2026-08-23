use crate::backend::error::AppResult;
use crate::backend::services::http_download::{download_file, extract_zip};
use log::{debug, info, warn};
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BepInExTargetType {
    Profile,
    Cache,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BepInExProgress {
    pub stage: String,
    pub progress: f64,
    pub message: String,
    pub target_type: BepInExTargetType,
    pub target_id: String,
}

fn emit(
    stage: &str,
    progress: f64,
    message: &str,
    target_type: BepInExTargetType,
    target_id: &str,
) {
    crate::backend::events::publish(crate::backend::events::BackendEvent::BepInExProgress(
        BepInExProgress {
            stage: stage.to_string(),
            progress,
            message: message.to_string(),
            target_type,
            target_id: target_id.to_string(),
        },
    ));
}

/// Return the current whole percent only when it differs from the last one
/// emitted. Download callbacks run once per 64 KiB chunk and ZIP extraction
/// callbacks run once per entry, so publishing every callback can keep the UI
/// event loop busy long enough that the progress bar does not repaint.
fn changed_percent(current: u64, total: u64, last_emitted: &mut Option<u8>) -> Option<u8> {
    if total == 0 {
        return None;
    }

    let percent = ((current.saturating_mul(100) / total).min(100)) as u8;
    if *last_emitted == Some(percent) {
        return None;
    }
    *last_emitted = Some(percent);
    Some(percent)
}

/// `download_file` progress callback: emit at most once per whole percentage
/// point. No-op until the total size is known.
fn emit_download_progress(
    downloaded: u64,
    total: Option<u64>,
    target_type: BepInExTargetType,
    target_id: &str,
    last_emitted: &mut Option<u8>,
) {
    if let Some(pct) = total.and_then(|total| changed_percent(downloaded, total, last_emitted)) {
        emit(
            "downloading",
            f64::from(pct),
            &format!("Downloading... {pct}%"),
            target_type,
            target_id,
        );
    }
}

/// `extract_zip` progress callback: emit an "extracting" event for entry
/// `current` of `total`.
fn emit_extract_progress(
    current: usize,
    total: usize,
    target_type: BepInExTargetType,
    target_id: &str,
    last_emitted: &mut Option<u8>,
) {
    let Some(pct) = changed_percent(current as u64, total as u64, last_emitted) else {
        return;
    };
    emit(
        "extracting",
        f64::from(pct),
        &format!("Extracting {current}/{total}"),
        target_type,
        target_id,
    );
}

pub fn install_bepinex(
    url: String,
    destination: String,
    cache_path: Option<String>,
    target_type: BepInExTargetType,
    target_id: &str,
) -> AppResult<()> {
    info!("install_bepinex: {} -> {}", url, destination);
    let dest = Path::new(&destination);

    if let Some(ref cache) = cache_path {
        let cache_file = Path::new(cache);
        if cache_file.exists() {
            info!("Using cached BepInEx");
            emit(
                "extracting",
                0.0,
                "Using cached BepInEx...",
                target_type,
                target_id,
            );
            let mut last_extract_percent = None;
            extract_zip(cache_file, dest, |cur, total| {
                emit_extract_progress(
                    cur,
                    total,
                    target_type,
                    target_id,
                    &mut last_extract_percent,
                )
            })?;
            emit("complete", 100.0, "Complete!", target_type, target_id);
            return Ok(());
        }
    }

    let temp = dest.with_extension("zip.tmp");
    emit("downloading", 0.0, "Downloading...", target_type, target_id);
    let mut last_download_percent = Some(0);
    download_file(&url, &temp, None, None, |dl, total| {
        emit_download_progress(
            dl,
            total,
            target_type,
            target_id,
            &mut last_download_percent,
        )
    })?;

    if let Some(ref cache) = cache_path {
        let cache_file = Path::new(cache);
        if let Some(parent) = cache_file.parent() {
            fs::create_dir_all(parent).ok();
        }
        if let Err(e) = fs::copy(&temp, cache_file) {
            warn!("Failed to cache: {}", e);
        } else {
            debug!("Cached to {:?}", cache_file);
        }
    }

    emit("extracting", 0.0, "Extracting...", target_type, target_id);
    let mut last_extract_percent = Some(0);
    extract_zip(&temp, dest, |cur, total| {
        emit_extract_progress(
            cur,
            total,
            target_type,
            target_id,
            &mut last_extract_percent,
        )
    })?;

    fs::remove_file(&temp).ok();
    emit("complete", 100.0, "Complete!", target_type, target_id);
    Ok(())
}

pub fn download_bepinex_to_cache(
    url: String,
    cache_path: String,
    architecture: String,
) -> AppResult<()> {
    let cache_file = Path::new(&cache_path);

    emit(
        "downloading",
        0.0,
        "Downloading...",
        BepInExTargetType::Cache,
        &architecture,
    );
    let mut last_download_percent = Some(0);
    download_file(&url, cache_file, None, None, |dl, total| {
        emit_download_progress(
            dl,
            total,
            BepInExTargetType::Cache,
            &architecture,
            &mut last_download_percent,
        )
    })?;

    emit(
        "complete",
        100.0,
        "Complete!",
        BepInExTargetType::Cache,
        &architecture,
    );
    Ok(())
}

pub fn clear_cache(cache_path: String, architecture: String) -> AppResult<()> {
    let cache_file = Path::new(&cache_path);
    if cache_file.exists() {
        fs::remove_file(cache_file)?;
    }
    emit(
        "cleared",
        0.0,
        "Cache cleared",
        BepInExTargetType::Cache,
        &architecture,
    );
    Ok(())
}

pub fn cache_size(cache_path: &str) -> Option<u64> {
    fs::metadata(cache_path).ok().map(|m| m.len())
}

#[cfg(test)]
mod tests {
    use super::changed_percent;

    #[test]
    fn progress_is_emitted_once_per_whole_percent() {
        let mut last = Some(0);

        assert_eq!(changed_percent(11, 100, &mut last), Some(11));
        assert_eq!(changed_percent(119, 1000, &mut last), None);
        assert_eq!(changed_percent(120, 1000, &mut last), Some(12));
        assert_eq!(changed_percent(1_500, 1_000, &mut last), Some(100));
        assert_eq!(changed_percent(2_000, 1_000, &mut last), None);
    }

    #[test]
    fn progress_ignores_an_unknown_zero_total() {
        let mut last = None;

        assert_eq!(changed_percent(64, 0, &mut last), None);
        assert_eq!(last, None);
    }
}
