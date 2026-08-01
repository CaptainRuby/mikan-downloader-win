use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default)]
    pub rss_url: String,
    #[serde(default)]
    pub download_dir: String,
    #[serde(default)]
    pub bitcomet_exe: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_minutes: u64,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    #[serde(default)]
    pub auto_download_enabled: bool,
    #[serde(default)]
    pub proxy_mode: ProxyMode,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    NoProxy,
    #[default]
    System,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            rss_url: String::new(),
            download_dir: String::new(),
            bitcomet_exe: String::new(),
            poll_interval_minutes: default_poll_interval(),
            port: default_port(),
            bind_host: default_bind_host(),
            auto_download_enabled: false,
            proxy_mode: ProxyMode::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigPatch {
    pub rss_url: Option<String>,
    pub download_dir: Option<String>,
    pub bitcomet_exe: Option<String>,
    pub poll_interval_minutes: Option<i64>,
    pub port: Option<i64>,
    pub proxy_mode: Option<ProxyMode>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    New,
    Queued,
    DownloadingTorrent,
    Submitted,
    Paused,
    Completed,
    Deleted,
    Ignored,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedItem {
    pub id: String,
    pub unique_key: String,
    pub title: String,
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub guid: String,
    #[serde(default)]
    pub pub_date: String,
    #[serde(default)]
    pub enclosure_url: String,
    pub status: ItemStatus,
    #[serde(default)]
    pub download_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub torrent_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    pub first_seen_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub config: AppConfig,
    #[serde(default)]
    pub items: Vec<FeedItem>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            version: default_version(),
            config: AppConfig::default(),
            items: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ParsedRssItem {
    pub title: String,
    pub link: String,
    pub guid: String,
    pub pub_date: String,
    pub total_bytes: Option<u64>,
    pub enclosure_url: String,
    pub unique_key: String,
}

#[derive(Clone, Debug)]
pub struct BitCometTask {
    pub task_id: String,
    pub info_hash: String,
    pub finish_date: String,
    pub left: Option<u64>,
    pub progress_percent: Option<f64>,
    pub completed: Option<bool>,
    pub status: String,
    pub save_name: String,
    pub show_name: String,
    pub save_location: String,
}

fn default_version() -> u32 {
    1
}

fn default_poll_interval() -> u64 {
    15
}

fn default_port() -> u16 {
    3199
}

fn default_bind_host() -> String {
    "127.0.0.1".to_string()
}

#[cfg(test)]
mod tests {
    use super::AppState;

    #[test]
    fn loads_legacy_state_with_missing_optional_fields() {
        let state: AppState = serde_json::from_str(
            r#"{
              "version": 1,
              "config": {
                "rssUrl": "",
                "downloadDir": "",
                "bitcometExe": "",
                "pollIntervalMinutes": 15,
                "port": 3199,
                "bindHost": "127.0.0.1"
              },
              "items": []
            }"#,
        )
        .unwrap();
        assert_eq!(state.config.port, 3199);
        assert!(!state.config.auto_download_enabled);
        assert_eq!(state.config.proxy_mode, super::ProxyMode::System);
        assert!(state.items.is_empty());
    }
}
