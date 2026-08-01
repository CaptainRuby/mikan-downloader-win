use std::{path::Path, process::Command};

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "MikanRssDownloader";
const LEGACY_TASK: &str = "MikanRssDownloader";

pub fn enabled() -> bool {
    registry_enabled()
}

pub fn set_enabled(executable: &Path, should_enable: bool) -> Result<bool, String> {
    if should_enable {
        let value = format!("\"{}\" --hidden", executable.to_string_lossy());
        run_hidden(
            "reg.exe",
            &[
                "add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &value, "/f",
            ],
        )?;
        let _ = delete_legacy_task();
    } else {
        let _ = run_hidden("reg.exe", &["delete", RUN_KEY, "/v", VALUE_NAME, "/f"]);
        let _ = delete_legacy_task();
    }
    Ok(enabled())
}

pub fn migrate_legacy(executable: &Path) {
    if !registry_enabled() && legacy_task_enabled() {
        let _ = set_enabled(executable, true);
    }
}

fn registry_enabled() -> bool {
    command_success("reg.exe", &["query", RUN_KEY, "/v", VALUE_NAME])
}

fn legacy_task_enabled() -> bool {
    command_success("schtasks.exe", &["/query", "/tn", LEGACY_TASK])
}

fn delete_legacy_task() -> Result<(), String> {
    run_hidden("schtasks.exe", &["/delete", "/tn", LEGACY_TASK, "/f"])
}

fn command_success(program: &str, args: &[&str]) -> bool {
    hidden_command(program)
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run_hidden(program: &str, args: &[&str]) -> Result<(), String> {
    let status = hidden_command(program)
        .args(args)
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

fn hidden_command(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}
