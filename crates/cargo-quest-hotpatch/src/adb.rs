//! adb helpers for the wireless-device flow.

use std::process::Command;

pub fn device(env_device: Option<&str>) -> anyhow::Result<String> {
    if let Some(d) = env_device {
        if !d.trim().is_empty() {
            return Ok(d.trim().to_string());
        }
    }
    let out = Command::new("adb").arg("devices").output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines().skip(1) {
        let mut it = line.split_whitespace();
        if let (Some(serial), Some(state)) = (it.next(), it.next()) {
            if state == "device" {
                return Ok(serial.to_string());
            }
        }
    }
    anyhow::bail!("no connected device found (set --device / DEVICE)")
}

pub fn reverse(device: &str, port: u16) -> anyhow::Result<()> {
    let _ = Command::new("adb").args(["-s", device, "reverse", &format!("tcp:{port}"), &format!("tcp:{port}")]).status()?;
    Ok(())
}

pub fn install(device: &str, apk: &std::path::Path) -> anyhow::Result<()> {
    let status = Command::new("adb").args(["-s", device, "install", "-r", "-t"]).arg(apk).status()?;
    anyhow::ensure!(status.success(), "adb install failed");
    Ok(())
}

pub fn launch(device: &str, pkg: &str, activity: &str) -> anyhow::Result<()> {
    let status = Command::new("adb")
        .args(["-s", device, "shell", "am", "start", "-n", &format!("{pkg}/{activity}")])
        .status()?;
    anyhow::ensure!(status.success(), "am start failed");
    Ok(())
}
