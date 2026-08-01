mod bitcomet;
mod models;
mod network;
mod rss;
mod service;
mod startup;
mod torrent;
mod util;

use serde_json::{json, Value};
use service::Service;
use std::{path::Path, process::Command, sync::Arc};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

#[tauri::command]
async fn api_request(
    state: tauri::State<'_, Arc<Service>>,
    path: String,
    method: String,
    body: Option<Value>,
) -> Result<Value, String> {
    let service = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || route_request(&service, &path, &method, body))
        .await
        .map_err(|error| error.to_string())?
}

fn route_request(
    service: &Service,
    path: &str,
    method: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let method = method.to_ascii_uppercase();
    match (method.as_str(), path) {
        ("GET", "/api/config") => Ok(service.config_value()),
        ("PUT", "/api/config") => service.update_config(parse_body(body)?),
        ("POST", "/api/rss/refresh") => service.refresh_subscription(),
        ("PUT", "/api/automation") => {
            let enabled = body
                .as_ref()
                .and_then(|value| value.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            service.set_auto_download(enabled)
        }
        ("GET", "/api/items") => Ok(service.items_value()),
        ("GET", "/api/status") => Ok(service.status_value()),
        ("GET", "/api/startup") => Ok(service.startup_value()),
        ("PUT", "/api/startup") => {
            let enabled = body
                .as_ref()
                .and_then(|value| value.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            service.set_startup(enabled)
        }
        ("GET", "/api/bitcomet/detect") => service.detect_bitcomet(),
        ("POST", "/api/bitcomet/inspect") => {
            Ok(service.inspect_bitcomet(&body_string(&body, "path")))
        }
        ("POST", "/api/dialog/download-dir") => {
            let initial = body_string(&body, "initialPath");
            Ok(service.select_download_dir(&initial))
        }
        ("POST", "/api/dialog/bitcomet-dir") => {
            let initial = body_string(&body, "initialPath");
            Ok(service.select_bitcomet_dir(&initial))
        }
        ("POST", path) if path.ends_with("/download") => {
            service.submit_item(item_id(path, "/download")?)
        }
        ("POST", path) if path.ends_with("/ignore") => {
            service.ignore_item(item_id(path, "/ignore")?)
        }
        ("POST", path) if path.ends_with("/unignore") => {
            service.unignore_item(item_id(path, "/unignore")?)
        }
        ("POST", path) if path.ends_with("/retry") => service.retry_item(item_id(path, "/retry")?),
        ("POST", path) if path.ends_with("/pause") => service.pause_item(item_id(path, "/pause")?),
        ("POST", path) if path.ends_with("/resume") => {
            service.resume_item(item_id(path, "/resume")?)
        }
        ("POST", path) if path.ends_with("/delete") => {
            service.delete_item_task(item_id(path, "/delete")?)
        }
        ("POST", path) if path.ends_with("/open-directory") => {
            service.open_item_directory(item_id(path, "/open-directory")?)
        }
        _ => Err(format!("Unsupported local API route: {method} {path}")),
    }
}

fn parse_body<T: serde::de::DeserializeOwned>(body: Option<Value>) -> Result<T, String> {
    serde_json::from_value(body.unwrap_or_else(|| json!({}))).map_err(|error| error.to_string())
}

fn body_string(body: &Option<Value>, key: &str) -> String {
    body.as_ref()
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn item_id<'a>(path: &'a str, suffix: &str) -> Result<&'a str, String> {
    path.strip_prefix("/api/items/")
        .and_then(|path| path.strip_suffix(suffix))
        .filter(|id| !id.is_empty() && !id.contains('/'))
        .ok_or_else(|| "Invalid item route".to_string())
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn open_download_directory(app: &tauri::AppHandle) {
    let Some(service) = app.try_state::<Arc<Service>>() else {
        return;
    };
    let directory = service.download_dir();
    if !Path::new(&directory).is_dir() {
        return;
    }
    let _ = Command::new("explorer.exe").arg(directory).spawn();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
        .setup(|app| {
            let executable = std::env::current_exe()?;
            let app_dir = executable
                .parent()
                .map(Path::to_path_buf)
                .ok_or("Executable directory is unavailable")?;
            let local_data_dir = app.path().app_local_data_dir()?;
            let data_dir = service::resolve_data_dir(&app_dir, local_data_dir);
            let service = Service::new(app_dir, data_dir, executable)?;
            Service::start_background(service.clone());
            app.manage(service);

            let open_item = MenuItem::with_id(app, "open", "打开助手", true, None::<&str>)?;
            let startup_item = CheckMenuItem::with_id(
                app,
                "startup",
                "开机自启动",
                true,
                startup::enabled(),
                None::<&str>,
            )?;
            let download_item =
                MenuItem::with_id(app, "download-dir", "打开下载目录", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &open_item,
                    &startup_item,
                    &download_item,
                    &separator,
                    &quit_item,
                ],
            )?;
            let startup_for_menu = startup_item.clone();
            let mut tray = TrayIconBuilder::new()
                .tooltip("Mikan下载助手")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open" => show_main(app),
                    "startup" => {
                        if let Some(service) = app.try_state::<Arc<Service>>() {
                            let next = !startup::enabled();
                            if service.set_startup(next).is_ok() {
                                let _ = startup_for_menu.set_checked(next);
                            }
                        }
                    }
                    "download-dir" => open_download_directory(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        show_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon().cloned() {
                tray = tray.icon(icon);
            }
            tray.build(app)?;

            if !std::env::args().any(|argument| argument == "--hidden") {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            } else if let Some(window) = app.get_webview_window("main") {
                let _ = window.hide();
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![api_request])
        .run(tauri::generate_context!())
        .expect("error while running Mikan下载助手");
}

#[cfg(test)]
mod tests {
    use super::item_id;

    #[test]
    fn parses_item_routes() {
        assert_eq!(
            item_id("/api/items/abc/download", "/download").unwrap(),
            "abc"
        );
        assert!(item_id("/api/items/a/b/download", "/download").is_err());
        assert_eq!(
            item_id("/api/items/abc/open-directory", "/open-directory").unwrap(),
            "abc"
        );
    }
}
