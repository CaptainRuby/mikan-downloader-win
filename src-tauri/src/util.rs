use chrono::Utc;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
};

pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn item_id_for(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)[..16].to_string()
}

pub fn mask_url(value: &str) -> String {
    secret_regex().replace_all(value, "$1***").into_owned()
}

pub fn redact_secrets(value: &str) -> String {
    secret_regex()
        .replace_all(value, "$1[redacted]")
        .into_owned()
}

pub fn sanitize_filename(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_was_space = false;
    for character in value.chars() {
        let invalid = matches!(
            character,
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
        ) || character.is_control();
        let character = if invalid { '_' } else { character };
        if character.is_whitespace() {
            if !last_was_space {
                output.push(' ');
            }
            last_was_space = true;
        } else {
            output.push(character);
            last_was_space = false;
        }
    }
    let cleaned = output.trim();
    let cleaned = if cleaned.is_empty() {
        "torrent"
    } else {
        cleaned
    };
    cleaned.chars().take(150).collect()
}

pub fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = fs::File::create(&temporary).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.write_all(b"\n").map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);

    let result = if path.exists() {
        replace_file(path, &temporary, &backup_path(path))
    } else {
        fs::rename(&temporary, path).map_err(|error| error.to_string())
    };
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn read_json_with_backup<T>(path: &Path) -> Result<T, String>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    match read_json(path) {
        Ok(value) => Ok(value),
        Err(primary_error) => {
            let backup = backup_path(path);
            let value = read_json(&backup).map_err(|backup_error| {
                format!("状态文件和备份都无法读取：{primary_error}；备份错误：{backup_error}")
            })?;
            if path.exists() {
                fs::remove_file(path).map_err(|error| error.to_string())?;
            }
            write_json(path, &value)?;
            Ok(value)
        }
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|error| error.to_string())
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension(
        path.extension()
            .map(|extension| format!("{}.bak", extension.to_string_lossy()))
            .unwrap_or_else(|| "bak".to_string()),
    )
}

#[cfg(windows)]
fn replace_file(path: &Path, replacement: &Path, backup: &Path) -> Result<(), String> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let wide = |value: &Path| {
        value
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let path = wide(path);
    let replacement = wide(replacement);
    let backup = wide(backup);
    let result = unsafe {
        ReplaceFileW(
            path.as_ptr(),
            replacement.as_ptr(),
            backup.as_ptr(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(path: &Path, replacement: &Path, backup: &Path) -> Result<(), String> {
    if backup.exists() {
        fs::remove_file(backup).map_err(|error| error.to_string())?;
    }
    fs::rename(path, backup).map_err(|error| error.to_string())?;
    if let Err(error) = fs::rename(replacement, path) {
        let _ = fs::rename(backup, path);
        return Err(error.to_string());
    }
    Ok(())
}

pub fn existing_folder(candidate: &str) -> Option<PathBuf> {
    if candidate.is_empty() {
        return None;
    }
    let path = PathBuf::from(candidate);
    if path.is_dir() {
        return Some(path);
    }
    if path.is_file() {
        return path.parent().map(Path::to_path_buf);
    }
    path.parent()
        .filter(|parent| parent.is_dir())
        .map(Path::to_path_buf)
}

fn secret_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?i)([?&](?:token|key|auth|pass|password)=)[^&\s]+").unwrap())
}

#[cfg(test)]
mod tests {
    use super::{read_json_with_backup, write_json};
    use serde_json::{json, Value};
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mikan-state-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn restores_state_from_atomic_backup() {
        let directory = temp_directory();
        let path = directory.join("state.json");
        fs::create_dir_all(&directory).unwrap();
        write_json(&path, &json!({ "revision": 1 })).unwrap();
        write_json(&path, &json!({ "revision": 2 })).unwrap();
        write_json(&path, &json!({ "revision": 3 })).unwrap();

        fs::write(&path, b"invalid json").unwrap();
        let recovered: Value = read_json_with_backup(&path).unwrap();
        assert_eq!(recovered["revision"], 2);
        let restored: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(restored["revision"], 2);

        fs::remove_dir_all(directory).unwrap();
    }
}
