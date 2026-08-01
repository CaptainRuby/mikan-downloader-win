use crate::models::BitCometTask;
use aes::Aes256;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use cbc::{
    cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit},
    Encryptor,
};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use rand::{rngs::OsRng, RngCore};
use regex::Regex;
use roxmltree::Document;
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};
#[cfg(windows)]
use std::{
    ffi::c_void,
    os::windows::{ffi::OsStrExt, process::CommandExt},
};

const MINIMUM_VERSION: (u16, u16) = (2, 9);
const WEBUI_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Default)]
pub struct TaskSnapshot {
    pub tasks: Vec<BitCometTask>,
    pub realtime: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct WebUiConfig {
    ports: Vec<u16>,
    username: String,
    password: String,
}

pub fn is_executable(path: &str) -> bool {
    let path = Path::new(path);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(name.as_str(), "bitcomet.exe" | "bitcomet_x64.exe") && path.is_file()
}

pub fn version(path: &str) -> Option<String> {
    executable_version(path).map(|parts| {
        if parts.2 == 0 && parts.3 == 0 {
            format!("{}.{}", parts.0, parts.1)
        } else {
            format!("{}.{}.{}", parts.0, parts.1, parts.2)
        }
    })
}

pub fn is_supported(path: &str) -> bool {
    executable_version(path).is_some_and(version_parts_supported)
}

fn version_parts_supported(value: (u16, u16, u16, u16)) -> bool {
    (value.0, value.1) > MINIMUM_VERSION
}

pub fn version_requirement_error(path: &str) -> Option<String> {
    if !is_executable(path) {
        return None;
    }
    match version(path) {
        Some(current) if !is_supported(path) => {
            Some(format!("BitComet 版本必须高于 2.09，当前版本为 {current}"))
        }
        None => Some("无法读取 BitComet 版本，要求版本高于 2.09".to_string()),
        _ => None,
    }
}

pub fn find_in_directory(directory: &Path) -> Option<PathBuf> {
    ["BitComet_x64.exe", "BitComet.exe"]
        .into_iter()
        .map(|name| directory.join(name))
        .find(|candidate| is_executable(&candidate.to_string_lossy()))
}

pub fn detect(saved_path: &str) -> String {
    let mut candidates = Vec::new();
    if !saved_path.is_empty() {
        candidates.push(PathBuf::from(saved_path));
    }
    candidates.extend(registry_candidates());
    candidates.extend(common_candidates());
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .find(|candidate| {
            let normalized = candidate.to_string_lossy().to_ascii_lowercase();
            seen.insert(normalized)
                && is_executable(&candidate.to_string_lossy())
                && is_supported(&candidate.to_string_lossy())
        })
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn add_torrent(executable: &str, torrent: &Path, download_dir: &Path) -> Result<(), String> {
    if !is_executable(executable) {
        return Err("BitComet executable is not valid".to_string());
    }
    if !torrent.is_file() {
        return Err("Torrent file does not exist".to_string());
    }
    if !download_dir.is_dir() {
        return Err("Download directory does not exist".to_string());
    }
    let mut command = Command::new(executable);
    command
        .arg(torrent)
        .arg(format!("--output={}", download_dir.to_string_lossy()))
        .arg("--silent")
        .arg("--tray")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        command.creation_flags(0x0000_0008 | 0x0800_0000);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Failed to start BitComet: {error}"))
}

pub fn read_tasks(executable: &str) -> Vec<BitCometTask> {
    if !is_executable(executable) {
        return Vec::new();
    }
    downloads_xml_candidates(executable)
        .into_iter()
        .filter_map(|path| read_xml(&path).ok())
        .find_map(|xml| parse_tasks(&xml))
        .unwrap_or_default()
}

pub fn task_snapshot(executable: &str) -> TaskSnapshot {
    let fallback = read_tasks(executable);
    if !is_executable(executable) {
        return TaskSnapshot {
            tasks: fallback,
            error: Some("BitComet 路径无效".to_string()),
            ..TaskSnapshot::default()
        };
    }
    if let Some(error) = version_requirement_error(executable) {
        return TaskSnapshot {
            tasks: fallback,
            version: version(executable),
            error: Some(error),
            ..TaskSnapshot::default()
        };
    }
    match fetch_webui_tasks(executable) {
        Ok((tasks, version)) => TaskSnapshot {
            tasks,
            realtime: true,
            version: Some(version),
            error: None,
        },
        Err((reported_version, error)) => TaskSnapshot {
            tasks: fallback,
            realtime: false,
            version: reported_version.or_else(|| version(executable)),
            error: Some(error),
        },
    }
}

pub fn pause_task(executable: &str, info_hash: &str) -> Result<(), String> {
    control_task(executable, info_hash, "/api_v2/tasks/action", "stop")
}

pub fn resume_task(executable: &str, info_hash: &str) -> Result<(), String> {
    control_task(executable, info_hash, "/api_v2/tasks/action", "start")
}

pub fn delete_task_and_files(executable: &str, info_hash: &str) -> Result<(), String> {
    control_task(executable, info_hash, "/api_v2/tasks/delete", "delete_all")
}

fn control_task(
    executable: &str,
    info_hash: &str,
    endpoint: &str,
    action: &str,
) -> Result<(), String> {
    let snapshot = task_snapshot(executable);
    if !snapshot.realtime {
        return Err(snapshot
            .error
            .unwrap_or_else(|| "BitComet WebUI 未连接".to_string()));
    }
    let task_id = snapshot
        .tasks
        .iter()
        .find(|task| task.info_hash.eq_ignore_ascii_case(info_hash))
        .map(|task| task.task_id.clone())
        .filter(|task_id| !task_id.is_empty())
        .ok_or_else(|| "BitComet 中没有找到对应任务".to_string())?;
    let config = webui_config(executable);
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(WEBUI_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    for port in config.ports.iter().copied() {
        let base = format!("http://127.0.0.1:{port}");
        let Ok(verify) = post_json(
            &client,
            &format!("{base}/api/webui/ip_verify"),
            &json!({}),
            None,
        ) else {
            continue;
        };
        for attempt in 0..2 {
            let token = authenticate(&client, &base, &config, &verify, executable)?;
            let response = post_json(
                &client,
                &format!("{base}{endpoint}"),
                &json!({ "task_ids": [&task_id], "action": action }),
                Some(&token),
            )?;
            if response.get("error_code").and_then(Value::as_str) == Some("INVALID_TOKEN")
                && attempt == 0
            {
                token_cache().lock().unwrap().remove(&base);
                continue;
            }
            return if control_action_succeeded(&response, action) {
                Ok(())
            } else {
                Err(api_error(&response))
            };
        }
    }
    Err("无法连接 BitComet WebUI".to_string())
}

fn control_action_succeeded(response: &Value, action: &str) -> bool {
    if response
        .get("error_code")
        .and_then(Value::as_str)
        .is_some_and(|code| code.eq_ignore_ascii_case("ok"))
    {
        return true;
    }
    response
        .get("tasks")
        .and_then(Value::as_array)
        .is_some_and(|tasks| {
            !tasks.is_empty()
                && tasks.iter().all(|task| {
                    let status = string_value(task, &["status"])
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    match action {
                        "stop" => status == "stopped",
                        "start" => matches!(status.as_str(), "starting" | "running"),
                        _ => false,
                    }
                })
        })
}

fn fetch_webui_tasks(
    executable: &str,
) -> Result<(Vec<BitCometTask>, String), (Option<String>, String)> {
    let config = webui_config(executable);
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(WEBUI_TIMEOUT)
        .build()
        .map_err(|error| (None, error.to_string()))?;
    let mut detected_version = None;
    let checked_ports = config
        .ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join("、");
    for port in config.ports.iter().copied() {
        let base = format!("http://127.0.0.1:{port}");
        let verify = match post_json(
            &client,
            &format!("{base}/api/webui/ip_verify"),
            &json!({}),
            None,
        ) {
            Ok(value) => value,
            Err(_) => continue,
        };
        detected_version = string_value(&verify, &["version"]).or(detected_version);
        if verify.get("error_code").and_then(Value::as_str) == Some("APP_ACCESS_DISABLED") {
            return Err((
                detected_version,
                format!(
                "BitComet WebUI 未开启；请在 BitComet 设置的远程访问中启用 WebUI（端口 {port}）"
            ),
            ));
        }
        let mut token = match authenticate(&client, &base, &config, &verify, executable) {
            Ok(token) => token,
            Err(error) => return Err((detected_version, error)),
        };
        let mut list = post_json(
            &client,
            &format!("{base}/api_v2/task_list/get"),
            &json!({
                "state_group": "ALL", "task_type": "ALL", "tag_filter": "ALL",
                "sort_key": "", "sort_order": "unsorted", "keyword": ""
            }),
            Some(&token),
        )
        .map_err(|error| (detected_version.clone(), error))?;
        if list.get("error_code").and_then(Value::as_str) == Some("INVALID_TOKEN") {
            token_cache().lock().unwrap().remove(&base);
            token = authenticate(&client, &base, &config, &verify, executable)
                .map_err(|error| (detected_version.clone(), error))?;
            list = post_json(
                &client,
                &format!("{base}/api_v2/task_list/get"),
                &json!({
                    "state_group": "ALL", "task_type": "ALL", "tag_filter": "ALL",
                    "sort_key": "", "sort_order": "unsorted", "keyword": ""
                }),
                Some(&token),
            )
            .map_err(|error| (detected_version.clone(), error))?;
        }
        if list.get("error_code").and_then(Value::as_str) != Some("OK") {
            return Err((detected_version, api_error(&list)));
        }
        let mut tasks = Vec::new();
        for item in list
            .get("tasks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(task_id) = item.get("task_id") else {
                continue;
            };
            let summary = post_json(
                &client,
                &format!("{base}/api/tasks/info/get"),
                &json!({ "task_id": task_id }),
                Some(&token),
            )
            .map_err(|error| (detected_version.clone(), error))?;
            if let Some(task) = parse_webui_task(item, &summary) {
                tasks.push(task);
            }
        }
        let found_version = detected_version.unwrap_or_else(|| "未知".to_string());
        return Ok((tasks, found_version));
    }
    Err((
        detected_version,
        format!(
            "无法连接 BitComet WebUI；请在 BitComet 设置 > 远程访问中启用 Web UI（已检查端口 {checked_ports}）"
        ),
    ))
}

fn authenticate(
    client: &reqwest::blocking::Client,
    base: &str,
    config: &WebUiConfig,
    verify: &Value,
    executable: &str,
) -> Result<String, String> {
    if let Some(token) = token_cache().lock().unwrap().get(base).cloned() {
        return Ok(token);
    }
    let client_id = client_device_id(executable);
    let login_body = if verify.get("bypass_eligible").and_then(Value::as_bool) == Some(true) {
        json!({ "client_id": client_id, "bypass": true })
    } else {
        if config.username.is_empty() {
            return Err(
                "BitComet WebUI 需要登录，但未在 BitComet.xml 中找到用户名；可开启“允许本机免登录”"
                    .to_string(),
            );
        }
        let credentials =
            json!({ "username": config.username, "password": config.password }).to_string();
        json!({ "client_id": client_id, "authentication": encrypt_authentication(&credentials, &client_id)? })
    };
    let login = post_json(
        client,
        &format!("{base}/api/webui/login"),
        &login_body,
        None,
    )?;
    let invite = login
        .get("invite_token")
        .and_then(Value::as_str)
        .filter(|_| login.get("error_code").and_then(Value::as_str) == Some("OK"))
        .ok_or_else(|| api_error(&login))?;
    let token = post_json(
        client,
        &format!("{base}/api/device_token/get"),
        &json!({
            "invite_token": invite, "device_id": client_id,
            "device_name": "Mikan下载助手 @ Windows", "platform": "webui"
        }),
        Some(invite),
    )?;
    let token = token
        .get("device_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| api_error(&token))?;
    token_cache()
        .lock()
        .unwrap()
        .insert(base.to_string(), token.clone());
    Ok(token)
}

fn client_device_id(executable: &str) -> String {
    let digest = Sha256::digest(executable.to_ascii_lowercase().as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&bytes[0..4]),
        hex::encode(&bytes[4..6]),
        hex::encode(&bytes[6..8]),
        hex::encode(&bytes[8..10]),
        hex::encode(&bytes[10..16])
    )
}

fn token_cache() -> &'static std::sync::Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<std::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn post_json(
    client: &reqwest::blocking::Client,
    url: &str,
    body: &Value,
    token: Option<&str>,
) -> Result<Value, String> {
    let mut request = client
        .post(url)
        .header("Client-Type", "BitComet WebUI")
        .json(body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request
        .send()
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .map_err(|error| error.to_string())
}

fn parse_webui_task(list: &Value, summary: &Value) -> Option<BitCometTask> {
    let task = summary.get("task").unwrap_or(&Value::Null);
    let status = summary.get("task_status").unwrap_or(&Value::Null);
    let info_hash = normalize_info_hash(
        &string_value(task, &["infohash", "info_hash"])
            .or_else(|| string_value(list, &["infohash", "info_hash", "task_guid"]))?,
    );
    let selected_size = u64_value(list, &["selected_size", "total_size"]);
    let downloaded = u64_value(list, &["selected_downloaded_size", "dl_size"]);
    let left = u64_value(status, &["size_left"]).or_else(|| {
        selected_size
            .zip(downloaded)
            .map(|(total, done)| total.saturating_sub(done))
    });
    let progress_percent = f64_value(list, &["permillage"])
        .map(|value| value / 10.0)
        .or_else(|| f64_value(status, &["download_permillage"]).map(|value| value / 10.0));
    Some(BitCometTask {
        task_id: scalar_string(list.get("task_id")).unwrap_or_default(),
        info_hash,
        finish_date: string_value(task, &["finish_time", "finish_date"]).unwrap_or_default(),
        left,
        progress_percent,
        completed: selected_size
            .zip(downloaded)
            .map(|(total, done)| total > 0 && done >= total),
        status: string_value(list, &["status"]).unwrap_or_default(),
        save_name: string_value(task, &["save_name", "task_name"])
            .unwrap_or_else(|| string_value(list, &["task_name"]).unwrap_or_default()),
        show_name: string_value(list, &["task_name"]).unwrap_or_default(),
        save_location: string_value(task, &["save_folder", "save_location"]).unwrap_or_default(),
    })
}

fn normalize_info_hash(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    value.strip_prefix("bt_").unwrap_or(&value).to_string()
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| match value {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn string_value(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(name)?.as_str().map(str::to_string))
        .filter(|value| !value.is_empty())
}

fn u64_value(value: &Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        value
            .get(name)
            .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
    })
}

fn f64_value(value: &Value, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        value
            .get(name)
            .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
    })
}

fn api_error(value: &Value) -> String {
    match string_value(value, &["error_message", "error_code"])
        .unwrap_or_else(|| "BitComet WebUI 返回了无效响应".to_string())
        .as_str()
    {
        "action completed." => "操作已完成。".to_string(),
        "action skipped." => "BitComet 未执行该操作。".to_string(),
        message => message.to_string(),
    }
}

fn encrypt_authentication(plaintext: &str, password: &str) -> Result<String, String> {
    let mut salt_key = [0u8; 8];
    let mut salt_hmac = [0u8; 8];
    let mut iv = [0u8; 16];
    OsRng.fill_bytes(&mut salt_key);
    OsRng.fill_bytes(&mut salt_hmac);
    OsRng.fill_bytes(&mut iv);
    let mut key = [0u8; 32];
    let mut hmac_key = [0u8; 32];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), &salt_key, 10_000, &mut key);
    pbkdf2_hmac::<Sha1>(password.as_bytes(), &salt_hmac, 10_000, &mut hmac_key);
    let encrypted = Encryptor::<Aes256>::new(&key.into(), &iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(plaintext.as_bytes());
    let mut payload = Vec::with_capacity(34 + encrypted.len() + 32);
    payload.extend_from_slice(&[3, 1]);
    payload.extend_from_slice(&salt_key);
    payload.extend_from_slice(&salt_hmac);
    payload.extend_from_slice(&iv);
    payload.extend_from_slice(&encrypted);
    let mut mac = Hmac::<Sha256>::new_from_slice(&hmac_key).map_err(|error| error.to_string())?;
    mac.update(&payload);
    payload.extend_from_slice(&mac.finalize().into_bytes());
    Ok(BASE64.encode(payload))
}

fn parse_tasks(xml: &str) -> Option<Vec<BitCometTask>> {
    let Ok(document) = Document::parse(xml.trim_start_matches('\u{feff}')) else {
        return None;
    };
    Some(
        document
            .descendants()
            .filter(|node| {
                node.is_element() && node.tag_name().name().eq_ignore_ascii_case("Torrent")
            })
            .filter_map(|node| {
                let info_hash = field(node, &["InfoHashHex", "InfoHash"]).to_ascii_lowercase();
                if info_hash.is_empty() {
                    return None;
                }
                Some(BitCometTask {
                    task_id: String::new(),
                    info_hash,
                    finish_date: field(node, &["FinishDate", "FinishTime", "CompletedTime"]),
                    left: parse_u64_field(node, &["Left", "LeftSize", "TotalLeft", "DownloadLeft"]),
                    progress_percent: parse_percent_field(
                        node,
                        &["Progress", "ProgressPercent", "Percent"],
                    ),
                    completed: parse_bool_field(node, &["Completed", "IsCompleted", "Finished"]),
                    status: field(node, &["Status", "TaskStatus"]),
                    save_name: field(node, &["SaveName"]),
                    show_name: field(node, &["ShowName", "Name"]),
                    save_location: field(node, &["SaveLocation", "SavePath"]),
                })
            })
            .collect(),
    )
}

fn read_xml(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    if bytes.starts_with(&[0xff, 0xfe]) {
        return decode_utf16(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return decode_utf16(&bytes[2..], false);
    }
    if bytes
        .iter()
        .take(64)
        .skip(1)
        .step_by(2)
        .any(|byte| *byte == 0)
    {
        return decode_utf16(&bytes, true);
    }
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String, String> {
    if bytes.len() % 2 != 0 {
        return Err("Invalid UTF-16 XML length".to_string());
    }
    let units = bytes.chunks_exact(2).map(|pair| {
        if little_endian {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        }
    });
    String::from_utf16(&units.collect::<Vec<_>>()).map_err(|error| error.to_string())
}

fn downloads_xml_candidates(executable: &str) -> Vec<PathBuf> {
    data_file_candidates(executable, "Downloads.xml")
}

fn bitcomet_xml_candidates(executable: &str) -> Vec<PathBuf> {
    data_file_candidates(executable, "BitComet.xml")
}

fn data_file_candidates(executable: &str, file_name: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(directory) = Path::new(executable).parent() {
        candidates.push(directory.join(file_name));
        if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
            let relative = directory
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(value) => Some(value),
                    _ => None,
                })
                .collect::<PathBuf>();
            candidates.push(
                PathBuf::from(local_app_data)
                    .join("VirtualStore")
                    .join(relative)
                    .join(file_name),
            );
        }
    }
    if let Ok(app_data) = env::var("APPDATA") {
        candidates.push(PathBuf::from(app_data).join("BitComet").join(file_name));
    }
    candidates.retain(|path| path.is_file());
    candidates.sort_by_key(|path| {
        std::cmp::Reverse(
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok(),
        )
    });
    candidates
}

fn webui_config(executable: &str) -> WebUiConfig {
    let mut config = WebUiConfig {
        ports: vec![8080, 2333],
        ..WebUiConfig::default()
    };
    if let Some(xml) = bitcomet_xml_candidates(executable)
        .into_iter()
        .find_map(|path| read_xml(&path).ok())
    {
        if let Ok(document) = Document::parse(xml.trim_start_matches('\u{feff}')) {
            let settings = document.descendants().find(|node| {
                node.is_element() && node.tag_name().name().eq_ignore_ascii_case("Settings")
            });
            if let Some(settings) = settings {
                if let Ok(port) = field(settings, &["WebInterfacePort", "WebUIPort"]).parse::<u16>()
                {
                    config.ports.insert(0, port);
                }
                config.username = field(settings, &["WebInterfaceUsername", "WebUIUsername"]);
                config.password = field(settings, &["WebInterfacePassword", "WebUIPassword"]);
            }
        }
    }
    if let Ok(port) = env::var("BITCOMET_WEBUI_PORT")
        .unwrap_or_default()
        .parse::<u16>()
    {
        config.ports.insert(0, port);
    }
    config.ports.sort_unstable();
    config.ports.dedup();
    config
}

#[cfg(windows)]
fn executable_version(path: &str) -> Option<(u16, u16, u16, u16)> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };

    if !is_executable(path) {
        return None;
    }
    let wide = Path::new(path)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut handle = 0u32;
    let size = unsafe { GetFileVersionInfoSizeW(wide.as_ptr(), &mut handle) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    if unsafe { GetFileVersionInfoW(wide.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let root = [b'\\' as u16, 0];
    let mut info: *mut c_void = std::ptr::null_mut();
    let mut info_len = 0u32;
    if unsafe {
        VerQueryValueW(
            data.as_ptr().cast(),
            root.as_ptr(),
            &mut info,
            &mut info_len,
        )
    } == 0
        || info.is_null()
        || info_len < std::mem::size_of::<VS_FIXEDFILEINFO>() as u32
    {
        return None;
    }
    let info = unsafe { &*(info.cast::<VS_FIXEDFILEINFO>()) };
    Some((
        (info.dwFileVersionMS >> 16) as u16,
        info.dwFileVersionMS as u16,
        (info.dwFileVersionLS >> 16) as u16,
        info.dwFileVersionLS as u16,
    ))
}

#[cfg(not(windows))]
fn executable_version(_path: &str) -> Option<(u16, u16, u16, u16)> {
    None
}

pub fn existing_hashes(executable: &str) -> HashSet<String> {
    read_tasks(executable)
        .into_iter()
        .map(|task| task.info_hash)
        .collect()
}

pub fn wait_for_task(executable: &str, info_hash: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if existing_hashes(executable).contains(info_hash) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn field(node: roxmltree::Node<'_, '_>, names: &[&str]) -> String {
    for attribute in node.attributes() {
        if names
            .iter()
            .any(|name| attribute.name().eq_ignore_ascii_case(name))
        {
            return attribute.value().trim().to_string();
        }
    }
    for child in node.children().filter(|child| child.is_element()) {
        if names
            .iter()
            .any(|name| child.tag_name().name().eq_ignore_ascii_case(name))
        {
            return child
                .attribute("Value")
                .or_else(|| child.text())
                .unwrap_or_default()
                .trim()
                .to_string();
        }
    }
    String::new()
}

fn parse_u64_field(node: roxmltree::Node<'_, '_>, names: &[&str]) -> Option<u64> {
    field(node, names).replace(',', "").parse().ok()
}

fn parse_percent_field(node: roxmltree::Node<'_, '_>, names: &[&str]) -> Option<f64> {
    let value = field(node, names)
        .trim_end_matches('%')
        .parse::<f64>()
        .ok()?;
    Some(if value <= 1.0 { value * 100.0 } else { value })
}

fn parse_bool_field(node: roxmltree::Node<'_, '_>, names: &[&str]) -> Option<bool> {
    match field(node, names).to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

fn common_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for variable in [
        "ProgramFiles",
        "ProgramFiles(x86)",
        "LOCALAPPDATA",
        "APPDATA",
    ] {
        let Ok(root) = env::var(variable) else {
            continue;
        };
        let roots = if variable == "LOCALAPPDATA" {
            vec![PathBuf::from(root).join("Programs")]
        } else {
            vec![PathBuf::from(root)]
        };
        for root in roots {
            for folder in ["BitComet", "BitComet_x64", ""] {
                for name in ["BitComet_x64.exe", "BitComet.exe"] {
                    candidates.push(root.join(folder).join(name));
                }
            }
        }
    }
    candidates
}

#[cfg(windows)]
fn registry_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let keys = [
        r"HKCR\magnet\shell\open\command",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall",
        r"HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall",
        r"HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    ];
    for (index, key) in keys.into_iter().enumerate() {
        let mut command = Command::new("reg.exe");
        command.args(["query", key]);
        if index > 0 {
            command.arg("/s");
        } else {
            command.arg("/ve");
        }
        let Ok(output) = command.creation_flags(0x0800_0000).output() else {
            continue;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        for matched in executable_regex().find_iter(&text) {
            candidates.push(PathBuf::from(matched.as_str().trim_matches('"')));
        }
        for line in text.lines().filter(|line| line.contains("InstallLocation")) {
            if let Some((_, value)) = line.split_once("REG_SZ") {
                let directory = PathBuf::from(value.trim());
                candidates.push(directory.join("BitComet_x64.exe"));
                candidates.push(directory.join("BitComet.exe"));
            }
        }
    }
    candidates
}

#[cfg(not(windows))]
fn registry_candidates() -> Vec<PathBuf> {
    Vec::new()
}

fn executable_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r#"(?i)[A-Z]:\\[^\r\n"]*?BitComet(?:_x64)?\.exe"#).unwrap())
}

#[cfg(test)]
mod tests {
    use super::{
        client_device_id, control_action_succeeded, parse_tasks, parse_webui_task, read_xml,
        version_parts_supported, wait_for_task,
    };
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    fn temp_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mikan-bitcomet-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn creates_stable_uuid_device_id() {
        let first = client_device_id(r"C:\Program Files\BitComet\BitComet_x64.exe");
        let second = client_device_id(r"c:\program files\bitcomet\bitcomet_x64.exe");
        assert_eq!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(&first[8..9], "-");
        assert_eq!(&first[13..14], "-");
        assert_eq!(&first[18..19], "-");
        assert_eq!(&first[23..24], "-");
    }

    #[test]
    fn waits_until_bitcomet_reports_info_hash() {
        let directory = temp_directory();
        let executable = directory.join("BitComet.exe");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&executable, b"").unwrap();
        let xml = directory.join("Downloads.xml");
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            fs::write(
                xml,
                r#"<Downloads><Torrent InfoHashHex="abc123"/></Downloads>"#,
            )
            .unwrap();
        });

        assert!(wait_for_task(
            &executable.to_string_lossy(),
            "abc123",
            Duration::from_secs(2)
        ));
        writer.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parses_child_fields_and_utf16_xml() {
        let directory = temp_directory();
        fs::create_dir_all(&directory).unwrap();
        let xml = r#"<Downloads><Torrent><InfoHashHex>ABC123</InfoHashHex><Progress>100%</Progress><Completed>true</Completed><SaveLocation>C:\Downloads\episode.mkv</SaveLocation><SaveName>episode.mkv</SaveName></Torrent></Downloads>"#;
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        let path = directory.join("Downloads.xml");
        fs::write(&path, bytes).unwrap();

        let decoded = read_xml(&path).unwrap();
        let tasks = parse_tasks(&decoded).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].info_hash, "abc123");
        assert_eq!(tasks[0].progress_percent, Some(100.0));
        assert_eq!(tasks[0].completed, Some(true));
        assert_eq!(tasks[0].save_name, "episode.mkv");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn requires_a_version_strictly_higher_than_2_09() {
        assert!(!version_parts_supported((2, 9, 9, 0)));
        assert!(version_parts_supported((2, 10, 0, 0)));
        assert!(version_parts_supported((3, 0, 0, 0)));
    }

    #[test]
    fn parses_realtime_task_list_and_summary() {
        let list = json!({
            "task_id": 7,
            "task_name": "episode.mkv",
            "status": "running",
            "selected_size": 1_000,
            "selected_downloaded_size": 125,
            "permillage": 125
        });
        let summary = json!({
            "task": {
                "infohash": "ABC123",
                "save_folder": "D:\\Downloads",
                "finish_time": ""
            },
            "task_status": { "size_left": 875 }
        });
        let task = parse_webui_task(&list, &summary).unwrap();
        assert_eq!(task.task_id, "7");
        assert_eq!(task.info_hash, "abc123");
        assert_eq!(task.left, Some(875));
        assert_eq!(task.progress_percent, Some(12.5));
        assert_eq!(task.save_location, "D:\\Downloads");
    }

    #[test]
    fn parses_bitcomet_task_guid_as_info_hash() {
        let list = json!({
            "task_id": 1007,
            "task_guid": "bt_57D1B1E7F3B47453C1D36BBB491E98DD6CE249A0",
            "task_name": "episode.mkv",
            "status": "running",
            "selected_size": 1_000,
            "selected_downloaded_size": 415,
            "permillage": 415
        });
        let task = parse_webui_task(
            &list,
            &json!({
                "error_code": "FATALL_ERROR",
                "error_message": "task_ids invalid"
            }),
        )
        .unwrap();
        assert_eq!(task.task_id, "1007");
        assert_eq!(task.info_hash, "57d1b1e7f3b47453c1d36bbb491e98dd6ce249a0");
        assert_eq!(task.left, Some(585));
        assert_eq!(task.progress_percent, Some(41.5));
    }

    #[test]
    fn accepts_bitcomet_control_success_responses() {
        assert!(control_action_succeeded(
            &json!({ "error_code": "ok", "error_message": "action completed." }),
            "start"
        ));
        assert!(control_action_succeeded(
            &json!({
                "error_code": "skipped",
                "tasks": [{ "status": "stopped" }]
            }),
            "stop"
        ));
        assert!(!control_action_succeeded(
            &json!({ "error_code": "skipped", "tasks": [{ "status": "running" }] }),
            "stop"
        ));
    }
}
