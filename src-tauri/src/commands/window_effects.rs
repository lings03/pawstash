#[cfg(any(target_os = "windows", target_os = "macos"))]
use tauri::utils::config::{Color, WindowEffectsConfig};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tauri::window::Effect;
use tauri::AppHandle;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tauri::Manager;

#[tauri::command]
pub async fn set_window_effect(
    app: AppHandle,
    effect_type: String,
    tint: Option<Vec<u8>>,
) -> Result<(), String> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if let Some(window) = app.get_webview_window("main") {
        window
            .set_effects(None::<WindowEffectsConfig>)
            .map_err(|error| error.to_string())?;

        if effect_type == "none" {
            return Ok(());
        }

        #[cfg(target_os = "windows")]
        {
            let effect = match effect_type.as_str() {
                "blur" => Effect::Blur,
                "mica" => Effect::Mica,
                "mica-dark" => Effect::MicaDark,
                "mica-light" => Effect::MicaLight,
                "tabbed" => Effect::Tabbed,
                "tabbed-dark" => Effect::TabbedDark,
                "tabbed-light" => Effect::TabbedLight,
                "acrylic" => Effect::Acrylic,
                other => return Err(format!("Window effect {other} is not available on Windows")),
            };

            let color = match effect_type.as_str() {
                "mica" | "mica-dark" | "mica-light" | "tabbed" | "tabbed-dark" | "tabbed-light" => {
                    None
                }
                _ => match tint.as_deref() {
                    Some([r, g, b, a]) => Some(Color(*r, *g, *b, *a)),
                    _ => Some(Color(19, 19, 19, 163)),
                },
            };

            let effects_config = WindowEffectsConfig {
                effects: vec![effect],
                state: None,
                radius: None,
                color,
            };

            window
                .set_effects(Some(effects_config))
                .map_err(|error| error.to_string())?;
        }

        #[cfg(target_os = "macos")]
        {
            let effect = match effect_type.as_str() {
                "sidebar" => Effect::Sidebar,
                "under-window" | "vibrancy" => Effect::UnderWindowBackground,
                other => return Err(format!("Window effect {other} is not available on macOS")),
            };

            let effects_config = WindowEffectsConfig {
                effects: vec![effect],
                state: None,
                radius: Some(14.0),
                color: None,
            };

            window
                .set_effects(Some(effects_config))
                .map_err(|error| error.to_string())?;
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = (app, effect_type);

    Ok(())
}

/// Applied live only on opaque Linux windows; elsewhere it would paint over the window effect.
#[tauri::command]
pub async fn set_window_background_color(
    app: AppHandle,
    color: String,
    state: tauri::State<'_, crate::commands::AppState>,
) -> Result<(), String> {
    let parsed = crate::config::settings::parse_window_background_color(&color)
        .ok_or_else(|| format!("{color} is not a valid window background colour"))?;

    let settings = state.config_manager.load()?;
    if settings.window_background_color != color {
        state.config_manager.save_window_background_color(&color)?;
    }

    #[cfg(target_os = "linux")]
    if !settings.linux_transparent_window {
        use tauri::Manager;
        if let Some(window) = app.get_webview_window("main") {
            window
                .set_background_color(Some(parsed))
                .map_err(|error| error.to_string())?;
        }
    }

    #[cfg(not(target_os = "linux"))]
    let _ = (app, parsed);

    Ok(())
}
