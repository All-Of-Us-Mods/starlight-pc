use crate::backend::error::{AppError, AppResult};
use log::{info, warn};
use serde_json::{Map, Value};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub struct ZipProfileInfo {
    pub root_prefix: Option<String>,
    pub metadata_name: Option<String>,
    pub metadata_bytes: Option<Vec<u8>>,
}

pub fn export_profile_zip(
    profile_path: String,
    destination: String,
    mut on_progress: impl FnMut(f64),
) -> AppResult<()> {
    let profile_dir = Path::new(&profile_path);
    if !profile_dir.exists() || !profile_dir.is_dir() {
        return Err(AppError::validation(format!(
            "Profile directory does not exist: {}",
            profile_path
        )));
    }

    let destination_path = Path::new(&destination);
    if let Some(parent) = destination_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let sanitized_metadata = build_sanitized_metadata(profile_dir)?;

    let output = File::create(destination_path)?;
    let mut zip = ZipWriter::new(output);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut metadata_written = false;

    let ctx = ZipExportContext {
        root_dir: profile_dir,
        destination_path,
        options,
        sanitized_metadata: &sanitized_metadata,
    };

    let mut progress = ExportProgress {
        written: 0,
        total: count_export_files(profile_dir).max(1),
        emit: &mut on_progress,
    };

    add_directory_to_zip(
        &mut zip,
        profile_dir,
        &ctx,
        &mut metadata_written,
        &mut progress,
    )?;

    if !metadata_written {
        zip.start_file("profile.json", options)?;
        zip.write_all(sanitized_metadata.as_bytes())?;
    }

    zip.finish()?;
    on_progress(100.0);
    info!("Exported profile zip: {} -> {}", profile_path, destination);
    Ok(())
}

pub fn analyze_profile_zip(zip_path: &str) -> AppResult<Vec<ZipProfileInfo>> {
    let zip_file = File::open(zip_path)?;
    let mut archive = ZipArchive::new(zip_file)?;

    let mut top_level_files = false;
    let mut top_level_dirs = std::collections::HashSet::new();
    let mut metadata_files = std::collections::HashMap::new();

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(path) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
            continue;
        };

        let components: Vec<_> = path.components().collect();
        if components.is_empty() {
            continue;
        }

        if components.len() == 1 {
            if entry.is_dir() {
                top_level_dirs.insert(components[0].as_os_str().to_string_lossy().to_string());
            } else {
                top_level_files = true;
            }
        } else {
            top_level_dirs.insert(components[0].as_os_str().to_string_lossy().to_string());
        }

        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        if file_name.eq_ignore_ascii_case("metadata.json")
            || file_name.eq_ignore_ascii_case("profile.json")
        {
            if components.len() == 1 {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                metadata_files.insert(None, bytes);
            } else if components.len() == 2 {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                let prefix = components[0].as_os_str().to_string_lossy().to_string();
                metadata_files.insert(Some(prefix), bytes);
            }
        }
    }

    let mut infos = Vec::new();

    if top_level_files || top_level_dirs.is_empty() {
        let bytes = metadata_files.remove(&None);
        let name = bytes.as_ref().and_then(|b| extract_name_from_metadata(b));
        infos.push(ZipProfileInfo {
            root_prefix: None,
            metadata_name: name,
            metadata_bytes: bytes,
        });
    } else if top_level_dirs.len() == 1 {
        let prefix = top_level_dirs.into_iter().next().unwrap();
        let bytes = metadata_files.remove(&Some(prefix.clone()));
        let name = bytes.as_ref().and_then(|b| extract_name_from_metadata(b));
        infos.push(ZipProfileInfo {
            root_prefix: Some(prefix),
            metadata_name: name,
            metadata_bytes: bytes,
        });
    } else {
        for prefix in top_level_dirs {
            let bytes = metadata_files.remove(&Some(prefix.clone()));
            let name = bytes.as_ref().and_then(|b| extract_name_from_metadata(b));
            infos.push(ZipProfileInfo {
                root_prefix: Some(prefix),
                metadata_name: name,
                metadata_bytes: bytes,
            });
        }
    }

    Ok(infos)
}

pub fn extract_profile_from_zip(
    zip_path: &str,
    destination: &str,
    root_prefix: Option<&str>,
    mut on_progress: impl FnMut(f64),
) -> AppResult<()> {
    let zip_file = File::open(zip_path)?;
    let mut archive = ZipArchive::new(zip_file)?;

    let destination_path = Path::new(&destination);
    fs::create_dir_all(destination_path)?;

    let total = archive.len().max(1);
    for i in 0..archive.len() {
        on_progress((i as f64 / total as f64) * 100.0);
        let mut entry = archive.by_index(i)?;
        let Some(raw_entry_path) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
            warn!("Skipping entry {} with unsafe path", i);
            continue;
        };

        if let Some(prefix) = root_prefix {
            let mut components = raw_entry_path.components();
            if let Some(Component::Normal(first)) = components.next() {
                if first.to_string_lossy() != prefix {
                    continue;
                }
            } else {
                continue;
            }
        }

        let relative_path = strip_root_prefix(&raw_entry_path, root_prefix);
        if relative_path.as_os_str().is_empty() {
            continue;
        }

        let out_path = destination_path.join(&relative_path);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }

        if is_metadata_file(&relative_path) {
            continue;
        } else {
            let mut output = File::create(&out_path)?;
            std::io::copy(&mut entry, &mut output)?;
        }

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&out_path, fs::Permissions::from_mode(mode)).ok();
        }
    }

    on_progress(100.0);
    info!("Imported profile from zip: {} -> {}", zip_path, destination);
    Ok(())
}

struct ZipExportContext<'a> {
    root_dir: &'a Path,
    destination_path: &'a Path,
    options: SimpleFileOptions,
    sanitized_metadata: &'a str,
}

/// Tracks files written vs. total so we can report 0–100% progress.
struct ExportProgress<'a> {
    written: usize,
    total: usize,
    emit: &'a mut dyn FnMut(f64),
}

impl ExportProgress<'_> {
    fn tick(&mut self) {
        self.written += 1;
        (self.emit)((self.written as f64 / self.total as f64) * 100.0);
    }
}

/// Rough file count for the progress denominator. Over-counts skipped files,
/// so `tick()` may land under 100% — the forced `on_progress(100.0)` at
/// `finish()` covers the gap. Keeps the skip logic in one place.
fn count_export_files(current_dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(current_dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                count_export_files(&path)
            } else {
                1
            }
        })
        .sum()
}

fn add_directory_to_zip(
    zip: &mut ZipWriter<File>,
    current_dir: &Path,
    ctx: &ZipExportContext<'_>,
    metadata_written: &mut bool,
    progress: &mut ExportProgress<'_>,
) -> AppResult<()> {
    let entries = fs::read_dir(current_dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path == ctx.destination_path {
            continue;
        }

        let relative = path
            .strip_prefix(ctx.root_dir)
            .map_err(|e| AppError::other(e.to_string()))?;
        if should_skip_export_file(relative) {
            continue;
        }

        let mut zip_path = to_zip_path(relative)?;
        if zip_path.is_empty() {
            continue;
        }

        if path.is_dir() {
            zip.add_directory(format!("{zip_path}/"), ctx.options)?;
            add_directory_to_zip(zip, &path, ctx, metadata_written, progress)?;
            continue;
        }

        let is_root_metadata = is_metadata_file(relative) && relative.components().count() == 1;

        if is_root_metadata {
            zip_path = "profile.json".to_string();
        }

        zip.start_file(zip_path, ctx.options)?;
        if is_root_metadata {
            *metadata_written = true;
            zip.write_all(ctx.sanitized_metadata.as_bytes())?;
        } else {
            let mut file = File::open(&path)?;
            std::io::copy(&mut file, zip)?;
        }
        progress.tick();
    }

    Ok(())
}

fn build_sanitized_metadata(profile_dir: &Path) -> AppResult<String> {
    let metadata_path = profile_dir.join("metadata.json");
    let mut metadata = match fs::read_to_string(&metadata_path) {
        Ok(content) => parse_metadata_object(&content),
        Err(_) => Map::new(),
    };

    metadata.remove("id");
    metadata.remove("path");
    metadata.remove("created_at");
    metadata.remove("total_play_time");
    metadata.remove("last_launched_at");
    // `bepinex_installed` is passed through as-is (arch string, or a legacy
    // bool the import-side deserializer converts).

    if !metadata.contains_key("name") {
        metadata.insert(
            "name".to_string(),
            Value::String(default_profile_name(profile_dir)),
        );
    }

    if let Some(Value::Array(mods_array)) = metadata.get("mods") {
        let mut new_mods = Map::new();
        // Which plugin file each mod owns. `mods` stays a bare id -> version map
        // for compatibility with older readers; this side map lets our own
        // importer re-attach the shipped DLLs to their catalog entries instead
        // of surfacing them as loose "custom" mods.
        let mut mod_files = Map::new();
        for mod_entry in mods_array {
            if let Some(obj) = mod_entry.as_object()
                && let (Some(Value::String(mod_id)), Some(Value::String(version))) =
                    (obj.get("mod_id"), obj.get("version"))
            {
                new_mods.insert(mod_id.clone(), Value::String(version.clone()));
                if let Some(Value::String(file_name)) = obj.get("file") {
                    mod_files.insert(mod_id.clone(), Value::String(file_name.clone()));
                }
            }
        }
        metadata.insert("mods".to_string(), Value::Object(new_mods));
        metadata.insert("mod_files".to_string(), Value::Object(mod_files));
    } else if !metadata.contains_key("mods") {
        metadata.insert("mods".to_string(), Value::Object(Map::new()));
    }

    Ok(serde_json::to_string_pretty(&Value::Object(metadata))?)
}

fn parse_metadata_object(content: &str) -> Map<String, Value> {
    serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn default_profile_name(profile_dir: &Path) -> String {
    profile_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| "Imported Profile".to_string())
}

fn strip_root_prefix(path: &Path, root_prefix: Option<&str>) -> PathBuf {
    let mut components = path.components();
    if let Some(prefix) = root_prefix
        && let Some(Component::Normal(first)) = components.next()
        && first == prefix
    {
        return components.as_path().to_path_buf();
    }
    path.to_path_buf()
}

fn extract_name_from_metadata(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|value| {
            value
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn is_metadata_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.eq_ignore_ascii_case("metadata.json") || name.eq_ignore_ascii_case("profile.json")
        })
        .unwrap_or(false)
}

/// Only BepInEx's log files are left out — every plugin ships in the zip,
/// including mods from the Starlight catalog, so an imported profile is
/// playable without re-downloading anything.
fn should_skip_export_file(path: &Path) -> bool {
    let is_log_file = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.eq_ignore_ascii_case("errorlog.log") || name.eq_ignore_ascii_case("logoutput.log")
        })
        .unwrap_or(false);
    if !is_log_file {
        return false;
    }

    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name) if name.to_string_lossy().eq_ignore_ascii_case("bepinex")
        )
    })
}

fn to_zip_path(path: &Path) -> AppResult<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => parts.push(segment.to_string_lossy().to_string()),
            Component::CurDir => {}
            _ => {
                return Err(AppError::validation(format!(
                    "Unsupported path in zip entry: {:?}",
                    path
                )));
            }
        }
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_profile(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("starlight-zip-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn plugins_are_exported_and_only_bepinex_logs_are_skipped() {
        assert!(!should_skip_export_file(Path::new(
            "BepInEx/plugins/Reactor.dll"
        )));
        assert!(should_skip_export_file(Path::new("BepInEx/LogOutput.log")));
        assert!(!should_skip_export_file(Path::new("LogOutput.log")));
    }

    #[test]
    fn metadata_keeps_the_id_version_map_and_adds_plugin_file_names() {
        let dir = temp_profile("metadata");
        fs::write(
            dir.join("metadata.json"),
            br#"{
                "id": "reactor-1",
                "path": "C:/somewhere",
                "name": "Reactor",
                "mods": [
                    {"mod_id": "reactor", "version": "2.0.0", "file": "Reactor.dll", "enabled": true},
                    {"mod_id": "custom:Loose.dll", "version": "", "enabled": true}
                ]
            }"#,
        )
        .unwrap();

        let metadata = build_sanitized_metadata(&dir).unwrap();
        let value: Value = serde_json::from_str(&metadata).unwrap();

        assert!(value.get("id").is_none());
        assert!(value.get("path").is_none());
        assert_eq!(value["mods"]["reactor"], Value::String("2.0.0".into()));
        assert_eq!(
            value["mod_files"]["reactor"],
            Value::String("Reactor.dll".into())
        );
        // A mod with no file recorded contributes nothing to the side map.
        assert!(value["mod_files"].get("custom:Loose.dll").is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}
