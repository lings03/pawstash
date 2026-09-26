use crate::config::settings::AppSettings;
use crate::webview_proxy;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Applied {
    proxied: bool,
    browser_user_agent: String,
    transparent: bool,
}

impl Applied {
    fn from_settings(settings: &AppSettings) -> Self {
        Self {
            proxied: webview_proxy::SUPPORTED && webview_proxy::needed(settings),
            browser_user_agent: settings.network_browser_user_agent.trim().to_string(),
            transparent: cfg!(target_os = "linux") && settings.linux_transparent_window,
        }
    }
}

static APPLIED: OnceLock<Applied> = OnceLock::new();

/// Must run before the window is created.
pub fn apply(
    tauri_config: &mut tauri::Config,
    settings: &AppSettings,
) -> Option<std::net::TcpListener> {
    let applied = Applied::from_settings(settings);

    #[cfg(target_os = "linux")]
    if !applied.transparent {
        // WebKitGTK only scrolls by blitting when the view is opaque; opacity < 1 over a
        // transparent window leaves after-images (issue #14).
        let background = crate::config::settings::parse_window_background_color(
            &settings.window_background_color,
        );
        for window in &mut tauri_config.app.windows {
            window.transparent = false;
            window.background_color = background;
        }
    }

    if !applied.browser_user_agent.is_empty() {
        for window in &mut tauri_config.app.windows {
            window.user_agent = Some(applied.browser_user_agent.clone());
        }
    }

    let listener = webview_proxy::install(tauri_config, settings);
    let applied = Applied {
        proxied: listener.is_some(),
        ..applied
    };
    let _ = APPLIED.set(applied);
    listener
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingRestart {
    pub settings: Vec<&'static str>,
    pub can_restart: bool,
}

pub fn pending_restart(settings: &AppSettings) -> PendingRestart {
    let mut pending = Vec::new();
    if let Some(applied) = APPLIED.get() {
        let wanted = Applied::from_settings(settings);
        if wanted.proxied != applied.proxied {
            pending.push("webview_proxy");
        }
        if wanted.browser_user_agent != applied.browser_user_agent {
            pending.push("browser_user_agent");
        }
        if wanted.transparent != applied.transparent {
            pending.push("transparent_window");
        }
    }
    PendingRestart {
        settings: pending,
        can_restart: cfg!(desktop),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::ProxyMode;

    #[test]
    fn only_window_level_changes_ask_for_restart() {
        let settings = AppSettings {
            proxy_mode: ProxyMode::System,
            ..Default::default()
        };
        let mut config = tauri::Config::default();
        assert!(apply(&mut config, &settings).is_none());
        assert!(pending_restart(&settings).settings.is_empty());

        let unrelated = AppSettings {
            network_timeout_secs: settings.network_timeout_secs + 5,
            proxy_url: "http://proxy.test:3128".into(),
            ..settings.clone()
        };
        assert!(pending_restart(&unrelated).settings.is_empty());

        let agent = AppSettings {
            network_browser_user_agent: "Custom/1.0".into(),
            ..settings.clone()
        };
        assert_eq!(pending_restart(&agent).settings, ["browser_user_agent"]);

        let direct = AppSettings {
            proxy_mode: ProxyMode::None,
            ..settings.clone()
        };
        let expected: &[&str] = if webview_proxy::SUPPORTED {
            &["webview_proxy"]
        } else {
            &[]
        };
        assert_eq!(pending_restart(&direct).settings, expected);
    }
}
