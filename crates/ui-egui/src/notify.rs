//! A system notification with a sound, for work that finishes while the person is in another
//! window: macOS's notification centre (through `osascript`), `notify-send` and the desktop's
//! sound on Linux and the BSDs. The window also asks for attention (the Dock icon bounces, the
//! taskbar button flashes). Where none of this exists (the web, Windows without these tools)
//! only the attention request is made and [`notify`] says so.

/// Post `title` and `body` with a sound. Returns whether a system notification was posted; the
/// window asks for attention either way. The tools run on their own thread, so a slow
/// notification service never holds up a frame.
pub(crate) fn notify(ctx: &egui::Context, title: &str, body: &str) -> bool {
    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
    post(title, body)
}

#[cfg(target_os = "macos")]
fn post(title: &str, body: &str) -> bool {
    // The texts go in as arguments, never into the script, so nothing in them is run.
    let script = ["on run argv", "display notification (item 2 of argv) with title (item 1 of argv) sound name \"Glass\"", "end run"];
    let mut cmd = std::process::Command::new("osascript");
    for line in script {
        cmd.arg("-e").arg(line);
    }
    cmd.arg(title).arg(body);
    spawn(cmd)
}

#[cfg(all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))]
fn post(title: &str, body: &str) -> bool {
    let mut cmd = std::process::Command::new("notify-send");
    cmd.arg("--app-name=WordCraft").arg("--").arg(title).arg(body);
    let posted = spawn(cmd);
    let mut sound = std::process::Command::new("canberra-gtk-play");
    sound.arg("--id=complete");
    let _ = spawn(sound);
    posted
}

#[cfg(any(windows, target_arch = "wasm32"))]
fn post(_title: &str, _body: &str) -> bool {
    false
}

/// Start `cmd` and wait for it on a thread of its own (so it is reaped); `false` when it could
/// not be started (the tool is not installed).
#[cfg(not(any(windows, target_arch = "wasm32")))]
fn spawn(mut cmd: std::process::Command) -> bool {
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(e) => {
            log::debug!("couldn't post a notification: {e}");
            false
        }
    }
}
