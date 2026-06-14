//! Reading the host desktop's appearance — the accent color and preferred
//! color scheme — so the frontend can theme itself like a native app.
//!
//! - **Linux**: the XDG desktop portal `org.freedesktop.portal.Settings`
//!   namespace `org.freedesktop.appearance` (`accent-color`, `color-scheme`),
//!   via `ashpd`.
//! - **macOS**: `NSUserDefaults` global domain — `AppleInterfaceStyle` for dark
//!   mode and `AppleAccentColor` for the accent (mapped to Apple's palette).
//!   `NSColor.controlAccentColor` would be exact but needs AppKit on the main
//!   thread; the defaults read is thread-safe and good enough to start.
//! - **Anything else**: no preference, no accent (the CSS fallback applies).

use mnemis_types::AppearanceDto;

/// One-shot read of the current system appearance. Best-effort: any failure
/// (no portal, key unset) yields `AppearanceDto::default()` so the frontend
/// keeps its CSS fallback rather than erroring.
#[tauri::command]
pub async fn get_appearance() -> Result<AppearanceDto, String> {
    Ok(read_appearance().await)
}

#[cfg(target_os = "linux")]
async fn read_appearance() -> AppearanceDto {
    use ashpd::desktop::settings::{ColorScheme as Portal, Settings};
    use mnemis_types::ColorScheme;

    let mut out = AppearanceDto::default();
    let Ok(settings) = Settings::new().await else {
        return out;
    };
    if let Ok(scheme) = settings.color_scheme().await {
        out.color_scheme = match scheme {
            Portal::PreferDark => ColorScheme::Dark,
            Portal::PreferLight => ColorScheme::Light,
            Portal::NoPreference => ColorScheme::NoPreference,
        };
    }
    if let Ok(color) = settings.accent_color().await {
        out.accent = Some(rgb_to_hex(color.red(), color.green(), color.blue()));
    }
    out
}

/// Pack three 0..1 channels into `"#rrggbb"`.
#[cfg(target_os = "linux")]
fn rgb_to_hex(r: f64, g: f64, b: f64) -> String {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", c(r), c(g), c(b))
}

#[cfg(target_os = "macos")]
async fn read_appearance() -> AppearanceDto {
    use mnemis_types::ColorScheme;
    use objc2_foundation::{NSString, NSUserDefaults};

    // NSUserDefaults is thread-safe, so reading it off the Tauri async thread
    // (rather than the main thread) is fine — unlike most of AppKit.
    let defaults = NSUserDefaults::standardUserDefaults();

    // Dark mode: the global "AppleInterfaceStyle" default is the string "Dark"
    // in dark mode and absent (light) otherwise.
    let style_key = NSString::from_str("AppleInterfaceStyle");
    let is_dark = defaults
        .stringForKey(&style_key)
        .map(|s| s.to_string() == "Dark")
        .unwrap_or(false);
    let color_scheme = if is_dark {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    };

    // Accent: "AppleAccentColor" is an integer picking one of the standard
    // accent colors; the key is absent when "Multicolor" (blue) is selected.
    let accent_key = NSString::from_str("AppleAccentColor");
    let accent_idx = if defaults.objectForKey(&accent_key).is_some() {
        Some(defaults.integerForKey(&accent_key))
    } else {
        None
    };

    AppearanceDto {
        accent: Some(macos_accent_hex(accent_idx).to_string()),
        color_scheme,
    }
}

/// Map the `AppleAccentColor` index to Apple's standard accent palette.
/// `None` (key absent) is Multicolor, which presents as system blue.
#[cfg(target_os = "macos")]
fn macos_accent_hex(idx: Option<isize>) -> &'static str {
    match idx {
        Some(-1) => "#8c8c8c", // Graphite
        Some(0) => "#ff5257",  // Red
        Some(1) => "#f7821b",  // Orange
        Some(2) => "#ffc600",  // Yellow
        Some(3) => "#62ba46",  // Green
        Some(5) => "#953d96",  // Purple
        Some(6) => "#f74f9e",  // Pink
        _ => "#007aff",        // Blue (index 4) / Multicolor / unknown
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn read_appearance() -> AppearanceDto {
    AppearanceDto::default()
}
