//! Windows integration: Mica backdrop, system accent colour, elevation.

use serde::Serialize;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Accent {
    /// accent for light theme surfaces (AccentDark1, as WinUI uses it)
    pub light: String,
    /// accent for dark theme surfaces (AccentLight2)
    pub dark: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub elevated: bool,
    /// "mica" when the window got the Mica backdrop, "none" otherwise
    pub backdrop: &'static str,
    pub accent: Option<Accent>,
    pub version: String,
    /// the scan core serves the commands; false (non-Windows dev builds) means
    /// the WebView uses its mock tree
    pub core: bool,
    /// UI language (a dictionary id: "en" or "zh-CN"), see [`crate::i18n`]
    pub lang: &'static str,
    /// BCP 47 tag for formatting dates and numbers, e.g. "en-GB"
    pub locale: String,
    #[cfg(target_os = "macos")]
    pub benchmark_root: Option<String>,
}

impl Info {
    pub fn detect(backdrop: &'static str, version: String) -> Self {
        Info {
            elevated: imp::is_elevated(),
            backdrop,
            accent: imp::accent(),
            version,
            core: cfg!(any(windows, target_os = "macos")),
            lang: crate::i18n::lang().id(),
            locale: crate::i18n::choice().locale.clone(),
            #[cfg(target_os = "macos")]
            benchmark_root: std::env::var("TESSERA_BENCH_ROOT").ok(),
        }
    }
}

/// Mica needs Windows 11 (build 22000+); elsewhere the WebView paints its own base.
pub fn apply_backdrop(window: &tauri::WebviewWindow) -> &'static str {
    use tauri::window::{Effect, EffectsBuilder};
    if !imp::supports_mica() {
        return "none";
    }
    match window.set_effects(EffectsBuilder::new().effect(Effect::Mica).build()) {
        Ok(()) => "mica",
        Err(_) => "none",
    }
}

pub fn relaunch_elevated() -> Result<(), String> {
    imp::relaunch_elevated()
}

#[cfg(windows)]
mod imp {
    use super::Accent;
    use windows::UI::ViewManagement::{UIColorType, UISettings};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    pub fn supports_mica() -> bool {
        windows_version::OsVersion::current().build >= 22000
    }

    pub fn is_elevated() -> bool {
        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return false;
            }
            let mut elev = TOKEN_ELEVATION::default();
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elev as *mut _ as *mut _),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(token);
            ok && elev.TokenIsElevated != 0
        }
    }

    pub fn accent() -> Option<Accent> {
        let ui = UISettings::new().ok()?;
        let hex = |t: UIColorType| -> Option<String> {
            let c = ui.GetColorValue(t).ok()?;
            Some(format!("#{:02x}{:02x}{:02x}", c.R, c.G, c.B))
        };
        Some(Accent {
            light: hex(UIColorType::AccentDark1)?,
            dark: hex(UIColorType::AccentLight2)?,
        })
    }

    pub fn relaunch_elevated() -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe = HSTRING::from(exe.as_os_str());
        let r = unsafe {
            ShellExecuteW(
                None,
                w!("runas"),
                &exe,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        // values above 32 mean success; the user may also have declined the UAC prompt
        if r.0 as isize > 32 {
            Ok(())
        } else {
            Err(crate::i18n::text::NOT_ELEVATED.get().into())
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Accent;
    pub fn supports_mica() -> bool {
        false
    }
    pub fn is_elevated() -> bool {
        false
    }
    pub fn accent() -> Option<Accent> {
        None
    }
    pub fn relaunch_elevated() -> Result<(), String> {
        Err("only supported on Windows".into())
    }
}
