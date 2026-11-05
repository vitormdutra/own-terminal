use std::sync::{Arc, Mutex};

use eframe::egui;

/// The release workflow sets OWN_TERMINAL_VERSION to the tag it publishes (Cargo.toml's version is not bumped).
pub const VERSION: &str = match option_env!("OWN_TERMINAL_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
pub const RELEASES: &str = "https://github.com/vitormdutra/own-terminal/releases/latest";
const API: &str = "https://api.github.com/repos/vitormdutra/own-terminal/releases/latest";

fn parse(v: &str) -> Option<(u32, u32, u32)> {
    let mut p = v.trim_start_matches('v').split('.').map(|n| n.parse().ok());
    Some((p.next()??, p.next()??, p.next()??))
}

fn newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(a), Some(b)) if a > b)
}

/// Pulls `"tag_name": "v1.2.3"` out of the GitHub API response without a JSON parser.
fn tag_of(body: &str) -> Option<&str> {
    body.split("\"tag_name\"").nth(1)?.split('"').nth(1)
}

/// Asks GitHub for the latest release in the background (through `curl`, so no HTTP dependency).
/// The returned slot fills with the new tag when a newer version exists; it stays empty otherwise,
/// including when offline.
pub fn check(ctx: &egui::Context) -> Arc<Mutex<Option<String>>> {
    let slot = Arc::new(Mutex::new(None));
    let (out, ctx) = (slot.clone(), ctx.clone());
    std::thread::spawn(move || {
        let Ok(r) = std::process::Command::new("curl").args(["-fsS", "--max-time", "10", API]).output() else { return };
        let body = String::from_utf8_lossy(&r.stdout);
        if let Some(tag) = tag_of(&body).filter(|t| newer(t, VERSION)) {
            *out.lock().unwrap() = Some(tag.to_string());
            ctx.request_repaint();
        }
    });
    slot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically() {
        assert!(newer("v1.5.10", "1.5.9"));
        assert!(newer("v1.6.0", "v1.5.99"));
        assert!(!newer("v1.5.0", "1.5.0"));
        assert!(!newer("v1.4.9", "1.5.0"));
        assert!(!newer("garbage", "1.5.0"));
    }

    #[test]
    fn reads_the_tag() {
        assert_eq!(tag_of(r#"{"url":"x","tag_name": "v1.5.2","name":"y"}"#), Some("v1.5.2"));
        assert_eq!(tag_of("{}"), None);
    }
}
