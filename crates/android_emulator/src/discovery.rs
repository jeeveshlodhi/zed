#![allow(dead_code)]

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use collections::HashMap;
use smol::process::Command;

/// A discovered Android Virtual Device from `avdmanager list avd`.
#[derive(Clone, Debug)]
pub struct AvdInfo {
    pub name: String,
    pub target: String,
    pub abi: String,
    pub path: PathBuf,
}

/// Runtime status of an AVD as reported by `adb devices`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmulatorStatus {
    Running { serial: String },
    Stopped,
}

/// Derives the gRPC port for a running emulator from its ADB serial.
///
/// The emulator always allocates a console port (the number in `emulator-5554`)
/// and a gRPC bridge port at `console_port + 3000`. This is the same formula
/// Android Studio uses when no explicit `-grpc` flag was passed.
pub fn grpc_port_from_serial(serial: &str) -> Option<u16> {
    let console_port: u16 = serial.strip_prefix("emulator-")?.parse().ok()?;
    Some(console_port + 3000)
}

/// Runs `avdmanager list avd` and parses the output into a list of `AvdInfo`.
pub async fn list_avds(avdmanager_path: &std::path::Path) -> Result<Vec<AvdInfo>> {
    let output = Command::new(avdmanager_path)
        .args(["list", "avd"])
        .output()
        .await
        .context("failed to run avdmanager")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut avds = Vec::new();
    let mut current: HashMap<String, String> = HashMap::default();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush_avd_block(&mut current, &mut avds);
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            current.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    flush_avd_block(&mut current, &mut avds);

    Ok(avds)
}

fn flush_avd_block(current: &mut HashMap<String, String>, avds: &mut Vec<AvdInfo>) {
    if let Some(name) = current.get("Name").cloned() {
        avds.push(AvdInfo {
            name,
            target: current
                .get("Based on")
                .cloned()
                .unwrap_or_else(|| "Unknown".to_string()),
            abi: current
                .get("Tag/ABI")
                .cloned()
                .unwrap_or_else(|| "Unknown".to_string()),
            path: current
                .get("Path")
                .map(PathBuf::from)
                .unwrap_or_default(),
        });
    }
    current.clear();
}

/// Runs `adb devices` and for each emulator serial queries its AVD name via
/// `adb -s <serial> emu avd name`, returning a map of AVD name → status.
pub async fn list_running_emulators(
    adb_path: &std::path::Path,
) -> Result<HashMap<String, EmulatorStatus>> {
    let output = Command::new(adb_path)
        .arg("devices")
        .output()
        .await
        .context("failed to run adb devices")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut result: HashMap<String, EmulatorStatus> = HashMap::default();

    for line in stdout.lines().skip(1) {
        let mut parts = line.split_whitespace();
        let serial = match parts.next() {
            Some(s) => s,
            None => continue,
        };
        let state = match parts.next() {
            Some(s) => s,
            None => continue,
        };

        if state != "device" || !serial.starts_with("emulator-") {
            continue;
        }

        if let Ok(name_output) = Command::new(adb_path)
            .args(["-s", serial, "emu", "avd", "name"])
            .output()
            .await
        {
            let name_stdout = String::from_utf8_lossy(&name_output.stdout);
            if let Some(avd_name) = name_stdout.lines().next().map(str::trim) {
                if !avd_name.is_empty() {
                    result.insert(
                        avd_name.to_string(),
                        EmulatorStatus::Running {
                            serial: serial.to_string(),
                        },
                    );
                }
            }
        }
    }

    Ok(result)
}