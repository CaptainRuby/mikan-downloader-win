use crate::{
    bitcomet,
    models::{AppConfig, AppState, BitCometTask, ConfigPatch, FeedItem, ItemStatus, ParsedRssItem},
    network::{HttpClient, TORRENT_MAX_BYTES},
    rss, startup, torrent,
    util::{
        item_id_for, mask_url, now_iso, read_json_with_backup, redact_secrets, sanitize_filename,
        write_json,
    },
};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::Duration as StdDuration,
};
use url::Url;

pub struct Service {
    app_dir: PathBuf,
    data_dir: PathBuf,
    state_file: PathBuf,
    log_file: PathBuf,
    executable: PathBuf,
    inner: Mutex<Inner>,
    operation: Mutex<()>,
    log_lock: Mutex<()>,
}

struct Inner {
    state: AppState,
    started_at: String,
    next_poll_at: Option<String>,
    polling: bool,
    last_poll_at: Option<String>,
    last_poll_error: Option<String>,
    detected_bitcomet: String,
    bitcomet_version: Option<String>,
    bitcomet_realtime: bool,
    bitcomet_realtime_error: Option<String>,
}

pub fn resolve_data_dir(app_dir: &Path, local_data_dir: PathBuf) -> PathBuf {
    if app_dir.join(".portable").is_file() {
        app_dir.join("data")
    } else {
        local_data_dir.join("data")
    }
}

fn migrate_legacy_data(source: &Path, destination: &Path) -> Result<(), String> {
    if source == destination || !source.is_dir() {
        return Ok(());
    }
    copy_directory(source, destination)
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else if !destination_path.exists() {
            fs::copy(&source_path, &destination_path).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn verify_data_dir_writable(data_dir: &Path) -> Result<(), String> {
    let probe = data_dir.join(format!(".write-test-{}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map_err(|error| error.to_string())?;
        file.write_all(b"Mikan portable data write test")
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())
    })();
    let _ = fs::remove_file(&probe);
    result.map_err(|error| format!("数据目录不可写（{}）：{error}", data_dir.to_string_lossy()))
}

impl Service {
    pub fn new(
        app_dir: PathBuf,
        data_dir: PathBuf,
        executable: PathBuf,
    ) -> Result<Arc<Self>, String> {
        migrate_legacy_data(&app_dir.join("data"), &data_dir)?;
        fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
        verify_data_dir_writable(&data_dir)?;
        let state_file = data_dir.join("state.json");
        let state_backup = state_file.with_extension("json.bak");
        let state = if state_file.exists() || state_backup.exists() {
            read_json_with_backup(&state_file)?
        } else {
            let state = AppState::default();
            write_json(&state_file, &state)?;
            state
        };
        let service = Arc::new(Self {
            app_dir,
            data_dir: data_dir.clone(),
            state_file,
            log_file: data_dir.join("logs").join("service.log"),
            executable,
            inner: Mutex::new(Inner {
                state,
                started_at: now_iso(),
                next_poll_at: None,
                polling: false,
                last_poll_at: None,
                last_poll_error: None,
                detected_bitcomet: String::new(),
                bitcomet_version: None,
                bitcomet_realtime: false,
                bitcomet_realtime_error: None,
            }),
            operation: Mutex::new(()),
            log_lock: Mutex::new(()),
        });
        service.initialize()?;
        Ok(service)
    }

    pub fn start_background(service: Arc<Self>) {
        thread::spawn(move || {
            thread::sleep(StdDuration::from_millis(1500));
            let config = service.config();
            if !config.auto_download_enabled || config.rss_url.is_empty() {
                // The regular polling loop remains active for later configuration.
            } else if let Err(error) = service.refresh_rss(true) {
                service.log("ERROR", &format!("Initial refresh failed: {error}"));
            }
            loop {
                thread::sleep(StdDuration::from_secs(1));
                let due = service
                    .inner
                    .lock()
                    .ok()
                    .and_then(|inner| inner.next_poll_at.clone())
                    .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
                    .map(|value| value <= Utc::now())
                    .unwrap_or(false);
                if due {
                    let _ = service.refresh_rss(true);
                }
            }
        });
    }

    pub fn config_value(&self) -> Value {
        let config = self.config();
        let mut value = serde_json::to_value(&config).unwrap_or_else(|_| json!({}));
        value["rssUrlMasked"] = json!(mask_url(&config.rss_url));
        value["dataFilePath"] = json!(self.state_file.to_string_lossy());
        value["portableMode"] = json!(self.app_dir.join(".portable").is_file());
        value
    }

    pub fn items_value(&self) -> Value {
        let config = self.config();
        let snapshot = bitcomet::task_snapshot(&config.bitcomet_exe);
        let _ = self.refresh_completion_status_with_tasks(&snapshot.tasks);
        self.update_bitcomet_status(&snapshot);
        let mut items = self
            .inner
            .lock()
            .map(|inner| inner.state.items.clone())
            .unwrap_or_default();
        items.sort_by(|left, right| right.first_seen_at.cmp(&left.first_seen_at));
        let items = items
            .into_iter()
            .map(|item| item_with_progress(item, &snapshot.tasks))
            .collect::<Vec<_>>();
        json!({ "items": items })
    }

    pub fn status_value(&self) -> Value {
        let (runtime, config) = {
            let inner = self.inner.lock().unwrap();
            (
                (
                    inner.started_at.clone(),
                    inner.next_poll_at.clone(),
                    inner.polling,
                    inner.last_poll_at.clone(),
                    inner.last_poll_error.clone(),
                    inner.detected_bitcomet.clone(),
                    inner.bitcomet_version.clone(),
                    inner.bitcomet_realtime,
                    inner.bitcomet_realtime_error.clone(),
                ),
                inner.state.config.clone(),
            )
        };
        let bitcomet_path = if config.bitcomet_exe.is_empty() {
            runtime.5.clone()
        } else {
            config.bitcomet_exe.clone()
        };
        json!({
            "startedAt": runtime.0,
            "listenHost": "tauri",
            "listenPort": 0,
            "preferredPort": config.port,
            "nextPollAt": runtime.1,
            "polling": runtime.2,
            "lastPollAt": runtime.3,
            "lastPollError": runtime.4,
            "autoDownloadEnabled": config.auto_download_enabled,
            "dataDir": self.data_dir.to_string_lossy(),
            "appDir": self.app_dir.to_string_lossy(),
            "bitcometDetectedPath": runtime.5,
            "bitcometConfigured": bitcomet::is_executable(&bitcomet_path),
            "bitcometVersion": runtime.6.or_else(|| bitcomet::version(&bitcomet_path)),
            "bitcometRealtime": runtime.7,
            "bitcometRealtimeError": runtime.8,
            "downloadDirReady": Path::new(&config.download_dir).is_dir(),
            "startupEnabled": startup::enabled(),
            "logs": self.recent_logs(80)
        })
    }

    pub fn update_config(&self, patch: ConfigPatch) -> Result<Value, String> {
        let _operation = self.operation.lock().unwrap();
        let mut candidate = self.config();
        if let Some(value) = patch.rss_url {
            candidate.rss_url = value.trim().to_string();
        }
        if let Some(value) = patch.download_dir {
            candidate.download_dir = value.trim().to_string();
        }
        if let Some(value) = patch.bitcomet_exe {
            candidate.bitcomet_exe = value.trim().to_string();
        }
        if let Some(value) = patch.poll_interval_minutes {
            candidate.poll_interval_minutes = value.max(0) as u64;
        }
        if let Some(value) = patch.port {
            candidate.port = value.clamp(1024, 65535) as u16;
        }
        if let Some(value) = patch.proxy_mode {
            candidate.proxy_mode = value;
        }
        candidate.bind_host = "127.0.0.1".to_string();
        validate_config_fields(&candidate)?;

        let previous_config;
        {
            let mut inner = self.inner.lock().unwrap();
            previous_config = inner.state.config.clone();
            inner.state.config = candidate.clone();
            if let Err(error) = self.save_locked(&inner) {
                inner.state.config = previous_config;
                return Err(format!("配置写入失败：{error}"));
            }
            update_schedule(&mut inner);
        }
        let persisted: AppState = read_json_with_backup(&self.state_file)
            .map_err(|error| format!("配置写入后无法重新读取：{error}"))?;
        if persisted.config != candidate {
            let mut inner = self.inner.lock().unwrap();
            inner.state.config = previous_config;
            return Err("配置写入校验失败：磁盘内容与保存内容不一致".to_string());
        }
        self.refresh_bitcomet_detection()?;
        self.log("INFO", "Configuration updated");
        let mut value = serde_json::to_value(&candidate).map_err(|error| error.to_string())?;
        value["rssUrlMasked"] = json!(mask_url(&candidate.rss_url));
        value["restartRequired"] = json!(false);
        value["dataFilePath"] = json!(self.state_file.to_string_lossy());
        value["portableMode"] = json!(self.app_dir.join(".portable").is_file());
        Ok(value)
    }

    pub fn refresh_rss(&self, auto_download: bool) -> Result<Value, String> {
        let _operation = self.operation.lock().unwrap();
        self.refresh_rss_locked(auto_download)
    }

    pub fn refresh_subscription(&self) -> Result<Value, String> {
        self.refresh_rss(self.config().auto_download_enabled)
    }

    pub fn set_auto_download(&self, enabled: bool) -> Result<Value, String> {
        let _operation = self.operation.lock().unwrap();
        if enabled {
            validate_config(&self.config())?;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.state.config.auto_download_enabled = enabled;
        if enabled {
            schedule_soon(&mut inner);
        } else {
            inner.next_poll_at = None;
        }
        self.save_locked(&inner)?;
        drop(inner);
        self.log(
            "INFO",
            if enabled {
                "Automatic downloads enabled"
            } else {
                "Automatic downloads paused"
            },
        );
        Ok(json!({ "enabled": enabled }))
    }

    pub fn submit_item(&self, id: &str) -> Result<Value, String> {
        let _operation = self.operation.lock().unwrap();
        let item = self.submit_one(id)?;
        Ok(json!({ "item": item }))
    }

    pub fn ignore_item(&self, id: &str) -> Result<Value, String> {
        let _operation = self.operation.lock().unwrap();
        let item = {
            let mut inner = self.inner.lock().unwrap();
            let item = inner
                .state
                .items
                .iter_mut()
                .find(|item| item.id == id)
                .ok_or_else(|| "Item not found".to_string())?;
            item.status = ItemStatus::Ignored;
            item.ignored_at = Some(now_iso());
            item.updated_at = now_iso();
            item.last_error = None;
            let item = item.clone();
            self.save_locked(&inner)?;
            item
        };
        self.log("INFO", &format!("Ignored item {}", item.title));
        Ok(json!({ "item": item }))
    }

    pub fn unignore_item(&self, id: &str) -> Result<Value, String> {
        let _operation = self.operation.lock().unwrap();
        let item = {
            let mut inner = self.inner.lock().unwrap();
            let item = inner
                .state
                .items
                .iter_mut()
                .find(|item| item.id == id)
                .ok_or_else(|| "Item not found".to_string())?;
            if !matches!(item.status, ItemStatus::Ignored) {
                return Err("Only ignored items can be restored".to_string());
            }
            item.status = restored_item_status(&item.enclosure_url);
            item.ignored_at = None;
            item.updated_at = now_iso();
            item.last_error = None;
            let item = item.clone();
            if inner.state.config.auto_download_enabled {
                schedule_soon(&mut inner);
            }
            self.save_locked(&inner)?;
            item
        };
        self.log("INFO", &format!("Restored ignored item {}", item.title));
        Ok(json!({ "item": item }))
    }

    pub fn retry_item(&self, id: &str) -> Result<Value, String> {
        let _operation = self.operation.lock().unwrap();
        {
            let mut inner = self.inner.lock().unwrap();
            let item = inner
                .state
                .items
                .iter_mut()
                .find(|item| item.id == id)
                .ok_or_else(|| "Item not found".to_string())?;
            if !matches!(item.status, ItemStatus::Failed) {
                return Err("Only failed items can be retried".to_string());
            }
            item.status = ItemStatus::Queued;
            item.last_error = None;
            item.updated_at = now_iso();
            self.save_locked(&inner)?;
        }
        let item = self.submit_one(id)?;
        Ok(json!({ "item": item }))
    }

    pub fn pause_item(&self, id: &str) -> Result<Value, String> {
        self.control_item(id, true)
    }

    pub fn resume_item(&self, id: &str) -> Result<Value, String> {
        self.control_item(id, false)
    }

    fn control_item(&self, id: &str, pause: bool) -> Result<Value, String> {
        let _operation = self.operation.lock().unwrap();
        let (mut item, executable) = {
            let inner = self.inner.lock().unwrap();
            let item = inner
                .state
                .items
                .iter()
                .find(|item| item.id == id)
                .cloned()
                .ok_or_else(|| "Item not found".to_string())?;
            (item, inner.state.config.bitcomet_exe.clone())
        };
        let info_hash = item
            .info_hash
            .as_deref()
            .ok_or_else(|| "任务尚无 InfoHash".to_string())?;
        if pause {
            bitcomet::pause_task(&executable, info_hash)?;
            item.status = ItemStatus::Paused;
        } else {
            bitcomet::resume_task(&executable, info_hash)?;
            item.status = ItemStatus::Submitted;
        }
        item.updated_at = now_iso();
        item.last_error = None;
        self.replace_item(&item)?;
        self.log(
            "INFO",
            &format!(
                "{} BitComet task {}",
                if pause { "Paused" } else { "Resumed" },
                item.title
            ),
        );
        Ok(json!({ "item": item }))
    }

    pub fn delete_item_task(&self, id: &str) -> Result<Value, String> {
        let _operation = self.operation.lock().unwrap();
        let (mut item, executable) = {
            let inner = self.inner.lock().unwrap();
            let item = inner
                .state
                .items
                .iter()
                .find(|item| item.id == id)
                .cloned()
                .ok_or_else(|| "Item not found".to_string())?;
            (item, inner.state.config.bitcomet_exe.clone())
        };
        let info_hash = item
            .info_hash
            .as_deref()
            .ok_or_else(|| "任务尚无 InfoHash".to_string())?;
        bitcomet::delete_task_and_files(&executable, info_hash)?;
        delete_download_target(item.save_location.as_deref(), item.save_name.as_deref())?;
        if let Some(torrent_path) = item.torrent_path.as_deref() {
            let path = PathBuf::from(torrent_path);
            if path.starts_with(self.data_dir.join("torrent-cache")) && path.is_file() {
                fs::remove_file(&path).map_err(|error| format!("种子缓存删除失败：{error}"))?;
            }
        }
        item.status = ItemStatus::Deleted;
        item.torrent_path = None;
        item.updated_at = now_iso();
        item.last_error = None;
        self.replace_item(&item)?;
        self.log(
            "INFO",
            &format!("Deleted BitComet task and files {}", item.title),
        );
        Ok(json!({ "item": item }))
    }

    pub fn open_item_directory(&self, id: &str) -> Result<Value, String> {
        let (item, download_dir) = {
            let inner = self.inner.lock().unwrap();
            let item = inner
                .state
                .items
                .iter()
                .find(|item| item.id == id)
                .cloned()
                .ok_or_else(|| "Item not found".to_string())?;
            (item, inner.state.config.download_dir.clone())
        };
        let directory = item_directory(&item, &download_dir)
            .ok_or_else(|| "Download directory does not exist".to_string())?;
        Command::new("explorer.exe")
            .arg(&directory)
            .spawn()
            .map_err(|error| format!("Failed to open download directory: {error}"))?;
        Ok(json!({ "path": directory.to_string_lossy() }))
    }

    pub fn detect_bitcomet(&self) -> Result<Value, String> {
        let _operation = self.operation.lock().unwrap();
        let path = self.refresh_bitcomet_detection()?;
        Ok(json!({ "path": path }))
    }

    pub fn inspect_bitcomet(&self, path: &str) -> Value {
        let path = path.trim();
        let valid = bitcomet::is_executable(path);
        let version = valid.then(|| bitcomet::version(path)).flatten();
        let error = if path.is_empty() {
            Some("BitComet 路径不能为空".to_string())
        } else if !valid {
            Some("路径必须指向 BitComet.exe 或 BitComet_x64.exe".to_string())
        } else {
            bitcomet::version_requirement_error(path)
        };
        json!({
            "path": path,
            "valid": valid,
            "version": version,
            "supported": valid && error.is_none(),
            "error": error
        })
    }

    pub fn select_download_dir(&self, initial_path: &str) -> Value {
        let path = select_folder("选择下载目录", initial_path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        json!({ "path": path })
    }

    pub fn select_bitcomet_dir(&self, initial_path: &str) -> Value {
        let selected = select_folder("选择 BitComet 安装目录", initial_path);
        let folder = selected
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path = selected
            .as_deref()
            .and_then(bitcomet::find_in_directory)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        json!({ "folder": folder, "path": path })
    }

    pub fn startup_value(&self) -> Value {
        json!({ "enabled": startup::enabled() })
    }

    pub fn set_startup(&self, enabled: bool) -> Result<Value, String> {
        Ok(json!({ "enabled": startup::set_enabled(&self.executable, enabled)? }))
    }

    pub fn download_dir(&self) -> String {
        self.config().download_dir
    }

    fn initialize(&self) -> Result<(), String> {
        startup::migrate_legacy(&self.executable);
        self.refresh_bitcomet_detection()?;
        self.refresh_completion_status()?;
        let mut inner = self.inner.lock().unwrap();
        update_schedule(&mut inner);
        Ok(())
    }

    fn config(&self) -> AppConfig {
        self.inner.lock().unwrap().state.config.clone()
    }

    fn refresh_rss_locked(&self, auto_download: bool) -> Result<Value, String> {
        let config = self.config();
        if config.rss_url.is_empty() {
            return Err("尚未配置订阅，请先在配置页填写并保存 RSS 地址。".to_string());
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.polling {
                return Ok(json!({ "items": inner.state.items }));
            }
            inner.polling = true;
            inner.last_poll_at = Some(now_iso());
            inner.last_poll_error = None;
        }
        self.log(
            "INFO",
            &format!("Refreshing RSS {}", mask_url(&config.rss_url)),
        );

        let http = HttpClient::new(&config.proxy_mode)?;
        let result = rss::fetch_and_parse(&http, &config.rss_url);
        match result {
            Ok(parsed) => {
                let save_result = {
                    let mut inner = self.inner.lock().unwrap();
                    merge_items(&mut inner.state.items, parsed, &config.download_dir);
                    let result = self.save_locked(&inner);
                    finish_polling(&mut inner, &result);
                    result
                };
                if let Err(error) = save_result {
                    self.log("ERROR", &format!("RSS state could not be saved: {error}"));
                    return Err(error);
                }
                self.log("INFO", "RSS refresh completed");
                self.refresh_completion_status()?;
                if auto_download {
                    let ids = self
                        .inner
                        .lock()
                        .unwrap()
                        .state
                        .items
                        .iter()
                        .filter(|item| {
                            matches!(
                                item.status,
                                ItemStatus::New | ItemStatus::Queued | ItemStatus::Failed
                            )
                        })
                        .map(|item| item.id.clone())
                        .collect::<Vec<_>>();
                    for id in ids {
                        let _ = self.submit_one(&id);
                    }
                }
                Ok(self.items_value())
            }
            Err(error) => {
                let mut inner = self.inner.lock().unwrap();
                inner.polling = false;
                inner.last_poll_error = Some(error.clone());
                update_schedule(&mut inner);
                self.log("ERROR", &format!("RSS refresh failed: {error}"));
                Err(error)
            }
        }
    }

    fn submit_one(&self, id: &str) -> Result<FeedItem, String> {
        let (mut item, config) = {
            let inner = self.inner.lock().unwrap();
            let item = inner
                .state
                .items
                .iter()
                .find(|item| item.id == id)
                .ok_or_else(|| "Item not found".to_string())?
                .clone();
            (item, inner.state.config.clone())
        };
        if matches!(item.status, ItemStatus::Ignored) {
            return Err("Ignored item cannot be submitted".to_string());
        }
        if matches!(
            item.status,
            ItemStatus::Completed | ItemStatus::Submitted | ItemStatus::Paused
        ) {
            return Ok(item);
        }

        let result = (|| {
            if item.enclosure_url.is_empty() {
                return Err("RSS item does not include a torrent enclosure URL".to_string());
            }
            if !Path::new(&config.download_dir).is_dir() {
                return Err("Download directory does not exist".to_string());
            }
            if !bitcomet::is_executable(&config.bitcomet_exe) {
                return Err("BitComet executable is not configured".to_string());
            }
            item.status = ItemStatus::DownloadingTorrent;
            item.updated_at = now_iso();
            self.replace_item(&item)?;

            let torrent_path = self.download_torrent(&item)?;
            let bytes = fs::read(&torrent_path).map_err(|error| error.to_string())?;
            let metadata = torrent::parse_metadata(&bytes)?;
            item.info_hash = Some(metadata.info_hash.clone());
            item.total_bytes = Some(metadata.total_bytes);
            item.save_name = Some(if metadata.name.is_empty() {
                item.title.clone()
            } else {
                metadata.name
            });
            let duplicate_local = self.inner.lock().unwrap().state.items.iter().any(|other| {
                other.id != item.id
                    && other.info_hash.as_deref() == Some(metadata.info_hash.as_str())
                    && !matches!(other.status, ItemStatus::Failed | ItemStatus::New)
            });
            let duplicate_external =
                bitcomet::existing_hashes(&config.bitcomet_exe).contains(&metadata.info_hash);
            if duplicate_local || duplicate_external {
                item.status = ItemStatus::Submitted;
                item.last_error = Some(
                    "Duplicate torrent infohash already exists; skipped new submission".to_string(),
                );
                item.updated_at = now_iso();
                self.log("INFO", &format!("Skipped duplicate torrent {}", item.title));
                return Ok(());
            }

            bitcomet::add_torrent(
                &config.bitcomet_exe,
                &torrent_path,
                Path::new(&config.download_dir),
            )?;
            if !bitcomet::wait_for_task(
                &config.bitcomet_exe,
                &metadata.info_hash,
                StdDuration::from_secs(10),
            ) {
                return Err(
                    "BitComet did not confirm the submitted torrent within 10 seconds".to_string(),
                );
            }
            item.status = ItemStatus::Submitted;
            item.download_dir = config.download_dir;
            item.torrent_path = Some(torrent_path.to_string_lossy().into_owned());
            item.submitted_at = Some(now_iso());
            item.updated_at = now_iso();
            item.last_error = None;
            self.log(
                "INFO",
                &format!("Submitted torrent to BitComet: {}", item.title),
            );
            Ok(())
        })();
        if let Err(error) = result {
            item.status = ItemStatus::Failed;
            item.last_error = Some(error.clone());
            item.updated_at = now_iso();
            self.log(
                "ERROR",
                &format!("Submit failed for {}: {error}", item.title),
            );
        }
        self.replace_item(&item)?;
        Ok(item)
    }

    fn download_torrent(&self, item: &FeedItem) -> Result<PathBuf, String> {
        let cache_dir = self.data_dir.join("torrent-cache");
        fs::create_dir_all(&cache_dir).map_err(|error| error.to_string())?;
        let path = cache_dir.join(format!(
            "{}-{}.torrent",
            item.id,
            sanitize_filename(&item.title)
        ));
        let config = self.config();
        let http = HttpClient::new(&config.proxy_mode)?;
        let bytes = http.get_bytes(&item.enclosure_url, "Torrent", TORRENT_MAX_BYTES)?;
        if bytes.is_empty() {
            return Err("Torrent response was empty".to_string());
        }
        fs::write(&path, bytes).map_err(|error| error.to_string())?;
        Ok(path)
    }

    fn refresh_completion_status(&self) -> Result<(), String> {
        let config = self.config();
        let snapshot = bitcomet::task_snapshot(&config.bitcomet_exe);
        let result = self.refresh_completion_status_with_tasks(&snapshot.tasks);
        self.update_bitcomet_status(&snapshot);
        result
    }

    fn refresh_completion_status_with_tasks(&self, tasks: &[BitCometTask]) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let mut changed = false;
        for item in &mut inner.state.items {
            if item.status == ItemStatus::Completed && download_target_exists(item) == Some(false) {
                item.status = ItemStatus::Deleted;
                item.updated_at = now_iso();
                changed = true;
            }
            if item.status == ItemStatus::Deleted {
                continue;
            }
            let Some(info_hash) = &item.info_hash else {
                continue;
            };
            if matches!(item.status, ItemStatus::Ignored) {
                continue;
            }
            let Some(task) = tasks
                .iter()
                .find(|task| task.info_hash.eq_ignore_ascii_case(info_hash))
            else {
                continue;
            };
            let save_name = if task.save_name.is_empty() {
                task.show_name.clone()
            } else {
                task.save_name.clone()
            };
            if !save_name.is_empty() && item.save_name.as_deref() != Some(save_name.as_str()) {
                item.save_name = Some(save_name);
                changed = true;
            }
            if !task.save_location.is_empty()
                && item.save_location.as_deref() != Some(task.save_location.as_str())
            {
                item.save_location = Some(task.save_location.clone());
                changed = true;
            }
            if bitcomet_task_completed(task) {
                item.status = if download_target_exists(item) == Some(false) {
                    ItemStatus::Deleted
                } else {
                    ItemStatus::Completed
                };
                if item.completed_at.is_none() {
                    item.completed_at = Some(now_iso());
                }
                item.updated_at = now_iso();
                item.last_error = None;
                changed = true;
            } else if task.status.to_ascii_lowercase().contains("stopped") {
                if item.status != ItemStatus::Paused {
                    item.status = ItemStatus::Paused;
                    item.updated_at = now_iso();
                    changed = true;
                }
            }
        }
        if changed {
            self.save_locked(&inner)?;
        }
        Ok(())
    }

    fn update_bitcomet_status(&self, snapshot: &bitcomet::TaskSnapshot) {
        let mut inner = self.inner.lock().unwrap();
        inner.bitcomet_version = snapshot.version.clone();
        inner.bitcomet_realtime = snapshot.realtime;
        inner.bitcomet_realtime_error = snapshot.error.clone();
    }

    fn refresh_bitcomet_detection(&self) -> Result<String, String> {
        let saved = self.config().bitcomet_exe;
        let detected = bitcomet::detect(&saved);
        let mut inner = self.inner.lock().unwrap();
        inner.detected_bitcomet = detected.clone();
        if inner.state.config.bitcomet_exe.is_empty() && !detected.is_empty() {
            inner.state.config.bitcomet_exe = detected.clone();
            self.save_locked(&inner)?;
            self.log("INFO", &format!("Detected BitComet at {detected}"));
        }
        Ok(detected)
    }

    fn replace_item(&self, item: &FeedItem) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let target = inner
            .state
            .items
            .iter_mut()
            .find(|target| target.id == item.id)
            .ok_or_else(|| "Item not found".to_string())?;
        *target = item.clone();
        self.save_locked(&inner)
    }

    fn save_locked(&self, inner: &Inner) -> Result<(), String> {
        write_json(&self.state_file, &inner.state)
    }

    fn log(&self, level: &str, message: &str) {
        let Ok(_guard) = self.log_lock.lock() else {
            return;
        };
        if let Some(parent) = self.log_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let message = redact_secrets(message)
            .chars()
            .take(4000)
            .collect::<String>();
        let line = format!("{} {} {}\n", now_iso(), level, message);
        let _ = rotate_logs_if_needed(&self.log_file, line.len() as u64, 2 * 1024 * 1024, 3);
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_file)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn recent_logs(&self, limit: usize) -> Vec<String> {
        let Ok(_guard) = self.log_lock.lock() else {
            return Vec::new();
        };
        read_recent_lines(&self.log_file, limit, 512 * 1024).unwrap_or_default()
    }
}

fn rotate_logs_if_needed(
    path: &Path,
    incoming_bytes: u64,
    max_bytes: u64,
    backups: usize,
) -> Result<(), String> {
    let current_bytes = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if current_bytes + incoming_bytes <= max_bytes {
        return Ok(());
    }
    for index in (1..=backups).rev() {
        let destination = numbered_log_path(path, index);
        if destination.exists() {
            fs::remove_file(&destination).map_err(|error| error.to_string())?;
        }
        let source = if index == 1 {
            path.to_path_buf()
        } else {
            numbered_log_path(path, index - 1)
        };
        if source.exists() {
            fs::rename(source, destination).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn numbered_log_path(path: &Path, index: usize) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "service.log".to_string());
    path.with_file_name(format!("{name}.{index}"))
}

fn read_recent_lines(path: &Path, limit: usize, max_bytes: u64) -> Result<Vec<String>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    let read_bytes = length.min(max_bytes);
    file.seek(SeekFrom::End(-(read_bytes as i64)))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(read_bytes as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if read_bytes < length {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text
        .lines()
        .rev()
        .take(limit)
        .map(str::to_string)
        .collect::<Vec<_>>();
    lines.reverse();
    Ok(lines)
}

fn bitcomet_task_completed(task: &BitCometTask) -> bool {
    let status = task.status.trim().to_ascii_lowercase();
    !task.finish_date.is_empty()
        || task.left == Some(0)
        || task.completed == Some(true)
        || task
            .progress_percent
            .is_some_and(|progress| progress >= 99.999)
        || matches!(
            status.as_str(),
            "completed" | "complete" | "finished" | "seeding"
        )
}

fn download_target_exists(item: &FeedItem) -> Option<bool> {
    download_path_exists(item.save_location.as_deref(), item.save_name.as_deref())
}

fn item_directory(item: &FeedItem, configured_download_dir: &str) -> Option<PathBuf> {
    resolve_item_directory(
        item.save_location.as_deref(),
        item.save_name.as_deref(),
        configured_download_dir,
    )
}

fn resolve_item_directory(
    save_location: Option<&str>,
    save_name: Option<&str>,
    configured_download_dir: &str,
) -> Option<PathBuf> {
    if let Some(location) = save_location
        .map(str::trim)
        .filter(|location| !location.is_empty())
    {
        let path = PathBuf::from(location);
        if path.is_dir() {
            return Some(path);
        }
        if path.is_file() {
            return path.parent().map(Path::to_path_buf);
        }
        let is_target = save_name.is_some_and(|save_name| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case(save_name))
        });
        if is_target {
            if let Some(parent) = path.parent().filter(|parent| parent.is_dir()) {
                return Some(parent.to_path_buf());
            }
        }
    }
    let configured = PathBuf::from(configured_download_dir.trim());
    configured.is_dir().then_some(configured)
}

fn download_path_exists(location: Option<&str>, save_name: Option<&str>) -> Option<bool> {
    let location = location?.trim();
    if location.is_empty() {
        return None;
    }
    let location = PathBuf::from(location);
    if location.is_file() {
        return Some(true);
    }
    let Some(save_name) = save_name.map(str::trim).filter(|name| !name.is_empty()) else {
        return Some(location.exists());
    };
    let location_is_target = location
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(save_name));
    Some(if location_is_target {
        location.exists()
    } else {
        location.join(save_name).exists()
    })
}

fn delete_download_target(
    save_location: Option<&str>,
    save_name: Option<&str>,
) -> Result<(), String> {
    let Some(location) = save_location
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let Some(save_name) = save_name.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let mut components = Path::new(save_name).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err("本地下载文件删除失败：下载名称不是安全的单层路径".to_string());
    }
    let location = PathBuf::from(location);
    let location_is_target = location
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(save_name));
    let target = if location_is_target {
        location
    } else {
        location.join(save_name)
    };
    if !target.exists() {
        return Ok(());
    }
    if target.is_dir() {
        fs::remove_dir_all(&target)
    } else {
        fs::remove_file(&target)
    }
    .map_err(|error| format!("本地下载文件删除失败（{}）：{error}", target.display()))
}

fn item_with_progress(mut item: FeedItem, tasks: &[BitCometTask]) -> Value {
    if item.total_bytes.is_none() {
        item.total_bytes = item
            .torrent_path
            .as_deref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| torrent::parse_metadata(&bytes).ok())
            .map(|metadata| metadata.total_bytes);
    }
    let total = item.total_bytes;
    let task = item.info_hash.as_ref().and_then(|info_hash| {
        tasks
            .iter()
            .find(|task| task.info_hash.eq_ignore_ascii_case(info_hash))
    });
    let remaining = task
        .and_then(|task| task.left)
        .map(|left| total.map(|total| left.min(total)).unwrap_or(left));
    let progress_values = total
        .zip(remaining)
        .and_then(|(total, remaining)| download_progress(total, remaining));
    let downloaded = progress_values.map(|(downloaded, _)| downloaded);
    let progress = if matches!(item.status, ItemStatus::Completed) {
        Some(100.0)
    } else {
        progress_values.map(|(_, progress)| progress)
    };
    let mut value = serde_json::to_value(item).unwrap_or_else(|_| json!({}));
    if let Some(remaining) = remaining {
        value["remainingBytes"] = json!(remaining);
    }
    if let Some(downloaded) = downloaded.or_else(|| total.filter(|_| progress == Some(100.0))) {
        value["downloadedBytes"] = json!(downloaded);
    }
    if let Some(progress) = progress {
        value["progressPercent"] = json!(progress);
    }
    value
}

fn download_progress(total: u64, remaining: u64) -> Option<(u64, f64)> {
    if total == 0 {
        return None;
    }
    let downloaded = total.saturating_sub(remaining.min(total));
    let percent = ((downloaded as f64 / total as f64) * 1000.0).round() / 10.0;
    Some((downloaded, percent))
}

fn restored_item_status(enclosure_url: &str) -> ItemStatus {
    if enclosure_url.is_empty() {
        ItemStatus::New
    } else {
        ItemStatus::Queued
    }
}

fn finish_polling(inner: &mut Inner, save_result: &Result<(), String>) {
    inner.polling = false;
    if let Err(error) = save_result {
        inner.last_poll_error = Some(error.clone());
    }
    update_schedule(inner);
}

fn validate_config(config: &AppConfig) -> Result<(), String> {
    let mut errors: Vec<String> = Vec::new();
    if config.rss_url.is_empty() {
        errors.push("RSS 地址不能为空".to_string());
    } else if Url::parse(&config.rss_url)
        .map(|url| !matches!(url.scheme(), "http" | "https"))
        .unwrap_or(true)
    {
        errors.push("RSS 地址必须是 http:// 或 https:// URL".to_string());
    }
    if config.download_dir.is_empty() {
        errors.push("下载目录不能为空".to_string());
    } else if !Path::new(&config.download_dir).is_dir() {
        errors.push("下载目录不存在或不是文件夹".to_string());
    }
    if config.bitcomet_exe.is_empty() {
        errors.push("BitComet 路径不能为空".to_string());
    } else if !bitcomet::is_executable(&config.bitcomet_exe) {
        errors.push("BitComet 路径必须指向 BitComet.exe 或 BitComet_x64.exe".to_string());
    } else if let Some(error) = bitcomet::version_requirement_error(&config.bitcomet_exe) {
        errors.push(error);
    }
    if !(1..=1440).contains(&config.poll_interval_minutes) {
        errors.push("轮询间隔必须是 1 到 1440 之间的整数分钟".to_string());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("配置不可用：{}", errors.join("；")))
    }
}

fn validate_config_fields(config: &AppConfig) -> Result<(), String> {
    let mut errors = Vec::new();
    if !config.rss_url.is_empty()
        && Url::parse(&config.rss_url)
            .map(|url| !matches!(url.scheme(), "http" | "https"))
            .unwrap_or(true)
    {
        errors.push("RSS 地址必须是 http:// 或 https:// URL".to_string());
    }
    if !config.download_dir.is_empty() && !Path::new(&config.download_dir).is_dir() {
        errors.push("下载目录不存在或不是文件夹".to_string());
    }
    if !config.bitcomet_exe.is_empty() {
        if !bitcomet::is_executable(&config.bitcomet_exe) {
            errors.push("BitComet 路径必须指向 BitComet.exe 或 BitComet_x64.exe".to_string());
        } else if let Some(error) = bitcomet::version_requirement_error(&config.bitcomet_exe) {
            errors.push(error);
        }
    }
    if !(1..=1440).contains(&config.poll_interval_minutes) {
        errors.push("轮询间隔必须是 1 到 1440 之间的整数分钟".to_string());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("配置不可用：{}", errors.join("；")))
    }
}

fn merge_items(items: &mut Vec<FeedItem>, parsed: Vec<ParsedRssItem>, download_dir: &str) {
    let now = now_iso();
    for parsed_item in parsed {
        if let Some(current) = items
            .iter_mut()
            .find(|item| item.unique_key == parsed_item.unique_key)
        {
            if !parsed_item.title.is_empty() {
                current.title = parsed_item.title;
            }
            if !parsed_item.link.is_empty() {
                current.link = parsed_item.link;
            }
            if !parsed_item.guid.is_empty() {
                current.guid = parsed_item.guid;
            }
            if !parsed_item.pub_date.is_empty() {
                current.pub_date = parsed_item.pub_date;
            }
            if parsed_item.total_bytes.is_some() {
                current.total_bytes = parsed_item.total_bytes;
            }
            if !parsed_item.enclosure_url.is_empty() {
                current.enclosure_url = parsed_item.enclosure_url;
            }
            current.updated_at = now.clone();
            continue;
        }
        items.push(FeedItem {
            id: item_id_for(&parsed_item.unique_key),
            unique_key: parsed_item.unique_key,
            title: parsed_item.title,
            link: parsed_item.link,
            guid: parsed_item.guid,
            pub_date: parsed_item.pub_date,
            status: if parsed_item.enclosure_url.is_empty() {
                ItemStatus::New
            } else {
                ItemStatus::Queued
            },
            enclosure_url: parsed_item.enclosure_url,
            download_dir: download_dir.to_string(),
            torrent_path: None,
            info_hash: None,
            save_name: None,
            save_location: None,
            total_bytes: parsed_item.total_bytes,
            first_seen_at: now.clone(),
            updated_at: now.clone(),
            submitted_at: None,
            completed_at: None,
            ignored_at: None,
            last_error: None,
        });
    }
    items.sort_by_key(|item| std::cmp::Reverse(rss_timestamp(item)));
}

fn rss_timestamp(item: &FeedItem) -> i64 {
    DateTime::parse_from_rfc2822(&item.pub_date)
        .map(|date| date.timestamp())
        .or_else(|_| DateTime::parse_from_rfc3339(&item.pub_date).map(|date| date.timestamp()))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(&item.pub_date, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|date| date.and_utc().timestamp())
        })
        .unwrap_or_else(|_| {
            DateTime::parse_from_rfc3339(&item.first_seen_at)
                .map(|date| date.timestamp())
                .unwrap_or_default()
        })
}

fn update_schedule(inner: &mut Inner) {
    if !inner.state.config.auto_download_enabled {
        inner.next_poll_at = None;
        return;
    }
    let minutes = inner.state.config.poll_interval_minutes.clamp(1, 1440) as i64;
    inner.next_poll_at = Some(
        (Utc::now() + Duration::minutes(minutes))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    );
}

fn schedule_soon(inner: &mut Inner) {
    inner.next_poll_at = Some(
        (Utc::now() + Duration::seconds(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    );
}

fn select_folder(title: &str, initial_path: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    if let Some(folder) = crate::util::existing_folder(initial_path) {
        dialog = dialog.set_directory(folder);
    }
    dialog.pick_folder()
}

#[cfg(test)]
mod tests {
    use super::{
        bitcomet_task_completed, delete_download_target, download_path_exists, download_progress,
        finish_polling, migrate_legacy_data, read_recent_lines, resolve_data_dir,
        resolve_item_directory, restored_item_status, rotate_logs_if_needed, validate_config,
    };
    use crate::models::{AppConfig, BitCometTask, ItemStatus};
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_directory(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mikan-{name}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn existing_download_directory_does_not_mark_task_complete() {
        let task = BitCometTask {
            task_id: "1".to_string(),
            info_hash: "abc".to_string(),
            finish_date: String::new(),
            left: Some(1024),
            progress_percent: Some(50.0),
            completed: Some(false),
            status: "downloading".to_string(),
            save_name: String::new(),
            show_name: String::new(),
            save_location: std::env::temp_dir().to_string_lossy().into_owned(),
        };
        assert!(!bitcomet_task_completed(&task));
    }

    #[test]
    fn unignored_downloadable_item_returns_to_queue() {
        assert_eq!(
            restored_item_status("https://example.test/item.torrent"),
            ItemStatus::Queued
        );
        assert_eq!(restored_item_status(""), ItemStatus::New);
    }

    #[test]
    fn reports_a_missing_completed_download_as_deleted() {
        let directory = temp_directory("deleted-download");
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("episode.mkv");
        fs::write(&target, b"video").unwrap();

        assert_eq!(
            download_path_exists(Some(&target.to_string_lossy()), Some("episode.mkv")),
            Some(true)
        );
        fs::remove_file(&target).unwrap();
        assert_eq!(
            download_path_exists(Some(&target.to_string_lossy()), Some("episode.mkv")),
            Some(false)
        );

        let nested = directory.join("nested.mkv");
        fs::write(&nested, b"video").unwrap();
        assert_eq!(
            download_path_exists(Some(&directory.to_string_lossy()), Some("nested.mkv")),
            Some(true)
        );
        fs::remove_file(nested).unwrap();
        assert_eq!(
            download_path_exists(Some(&directory.to_string_lossy()), Some("nested.mkv")),
            Some(false)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn deletes_local_download_file_as_a_fallback() {
        let directory = temp_directory("delete-local-file");
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("episode.mkv");
        fs::write(&target, b"video").unwrap();

        delete_download_target(Some(&directory.to_string_lossy()), Some("episode.mkv")).unwrap();

        assert!(!target.exists());
        assert!(directory.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn deletes_local_multi_file_download_directory_as_a_fallback() {
        let directory = temp_directory("delete-local-directory");
        let target = directory.join("series");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("episode.mkv"), b"video").unwrap();

        delete_download_target(Some(&directory.to_string_lossy()), Some("series")).unwrap();

        assert!(!target.exists());
        assert!(directory.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolves_the_folder_for_a_downloaded_file() {
        let directory = temp_directory("open-download");
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("episode.mkv");
        fs::write(&target, b"video").unwrap();

        assert_eq!(
            resolve_item_directory(Some(&target.to_string_lossy()), Some("episode.mkv"), ""),
            Some(directory.clone())
        );
        fs::remove_file(target).unwrap();
        assert_eq!(
            resolve_item_directory(
                Some(&directory.join("episode.mkv").to_string_lossy()),
                Some("episode.mkv"),
                ""
            ),
            Some(directory.clone())
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn calculates_download_progress_from_remaining_bytes() {
        assert_eq!(download_progress(1_000, 250), Some((750, 75.0)));
        assert_eq!(download_progress(0, 0), None);
    }

    #[test]
    fn save_failure_always_resets_polling_state() {
        let mut inner = super::Inner {
            state: crate::models::AppState::default(),
            started_at: crate::util::now_iso(),
            next_poll_at: None,
            polling: true,
            last_poll_at: None,
            last_poll_error: None,
            detected_bitcomet: String::new(),
            bitcomet_version: None,
            bitcomet_realtime: false,
            bitcomet_realtime_error: None,
        };
        finish_polling(&mut inner, &Err("disk full".to_string()));
        assert!(!inner.polling);
        assert_eq!(inner.last_poll_error.as_deref(), Some("disk full"));
    }

    #[test]
    fn rotates_logs_and_reads_only_recent_lines() {
        let directory = temp_directory("logs");
        let path = directory.join("service.log");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, b"one\ntwo\nthree\n").unwrap();
        assert_eq!(
            read_recent_lines(&path, 2, 1024).unwrap(),
            vec!["two".to_string(), "three".to_string()]
        );

        rotate_logs_if_needed(&path, 5, 10, 3).unwrap();
        assert!(!path.exists());
        assert!(directory.join("service.log.1").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn portable_installation_uses_adjacent_data_directory() {
        let app_dir = temp_directory("portable-path");
        let local_data_dir = temp_directory("local-path");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join(".portable"), b"portable").unwrap();

        assert_eq!(
            resolve_data_dir(&app_dir, local_data_dir),
            app_dir.join("data")
        );

        fs::remove_dir_all(app_dir).unwrap();
    }

    #[test]
    fn installed_application_uses_local_app_data() {
        let app_dir = temp_directory("installed-path");
        let local_data_dir = temp_directory("local-path");

        assert_eq!(
            resolve_data_dir(&app_dir, local_data_dir.clone()),
            local_data_dir.join("data")
        );
    }

    #[test]
    fn migrates_legacy_data_without_overwriting_existing_destination() {
        let root = temp_directory("migration");
        let source = root.join("legacy");
        let destination = root.join("current");
        fs::create_dir_all(source.join("logs")).unwrap();
        fs::write(source.join("state.json"), b"legacy").unwrap();
        fs::write(source.join("logs").join("service.log"), b"log").unwrap();

        migrate_legacy_data(&source, &destination).unwrap();
        assert_eq!(fs::read(destination.join("state.json")).unwrap(), b"legacy");
        assert_eq!(
            fs::read(destination.join("logs").join("service.log")).unwrap(),
            b"log"
        );

        fs::write(destination.join("state.json"), b"current").unwrap();
        fs::write(source.join("state.json"), b"changed").unwrap();
        migrate_legacy_data(&source, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("state.json")).unwrap(),
            b"current"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_incomplete_runtime_configuration() {
        let error = validate_config(&AppConfig::default()).unwrap_err();
        assert!(error.contains("RSS 地址不能为空"));
        assert!(error.contains("下载目录不能为空"));
        assert!(error.contains("BitComet 路径不能为空"));
    }

    #[test]
    fn rejects_out_of_range_poll_interval() {
        let config = AppConfig {
            poll_interval_minutes: 0,
            ..AppConfig::default()
        };
        assert!(validate_config(&config)
            .unwrap_err()
            .contains("轮询间隔必须是 1 到 1440 之间的整数分钟"));
    }

    #[test]
    fn scheduling_is_disabled_by_default() {
        let mut inner = super::Inner {
            state: crate::models::AppState::default(),
            started_at: crate::util::now_iso(),
            next_poll_at: Some(crate::util::now_iso()),
            polling: false,
            last_poll_at: None,
            last_poll_error: None,
            detected_bitcomet: String::new(),
            bitcomet_version: None,
            bitcomet_realtime: false,
            bitcomet_realtime_error: None,
        };
        super::update_schedule(&mut inner);
        assert!(inner.next_poll_at.is_none());
        inner.state.config.auto_download_enabled = true;
        super::update_schedule(&mut inner);
        assert!(inner.next_poll_at.is_some());
    }
}
