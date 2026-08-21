use crate::backend::error::AppResult;
use log::debug;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;
use zip::ZipArchive;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const READ_CHUNK: usize = 64 * 1024;

/// Blocking HTTP client with the given timeouts, shared by every backend
/// fetcher so connection behavior stays consistent.
pub fn http_client(
    connect_timeout: Duration,
    request_timeout: Duration,
) -> AppResult<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()?)
}

/// Stream `url` into `dest_path`, creating parent dirs, reporting
/// `(downloaded, total)` per chunk. Optionally sends one extra header and
/// hashes the body into `hasher` as it goes.
pub fn download_file<F>(
    url: &str,
    dest_path: &Path,
    extra_header: Option<(&str, String)>,
    mut hasher: Option<&mut Sha256>,
    mut on_progress: F,
) -> AppResult<()>
where
    F: FnMut(u64, Option<u64>),
{
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let client = http_client(CONNECT_TIMEOUT, REQUEST_TIMEOUT)?;

    let mut request = client.get(url);
    if let Some((name, value)) = extra_header {
        request = request.header(name, value);
    }
    let mut response = request.send()?.error_for_status()?;
    let total: Option<u64> = response.content_length();

    let mut file = File::create(dest_path)?;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut downloaded = 0u64;

    loop {
        let n = response.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if let Some(hasher) = hasher.as_deref_mut() {
            hasher.update(&buf[..n]);
        }
        file.write_all(&buf[..n])?;
        downloaded += n as u64;
        on_progress(downloaded, total);
    }

    Ok(())
}

/// Hex digest of everything hashed so far. Call after `download_file` when
/// verifying a checksum.
pub fn finish_digest(hasher: &mut Sha256) -> String {
    crate::backend::services::hex_digest(std::mem::take(hasher).finalize())
}

pub fn extract_zip<F>(zip_path: &Path, dest_path: &Path, mut on_progress: F) -> AppResult<()>
where
    F: FnMut(usize, usize),
{
    let file = File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)?;

    let total_entries = archive.len();
    if total_entries == 0 {
        return Ok(());
    }

    for i in 0..total_entries {
        let mut entry = archive.by_index(i)?;
        let Some(entry_path) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
            continue;
        };

        let output_path = dest_path.join(entry_path);
        if entry.is_dir() {
            fs::create_dir_all(&output_path)?;
        } else {
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = File::create(&output_path)?;
            std::io::copy(&mut entry, &mut output)?;
        }

        on_progress(i + 1, total_entries);
    }

    debug!(
        "Extracted zip archive with {} entries from {}",
        total_entries,
        zip_path.display()
    );

    Ok(())
}
