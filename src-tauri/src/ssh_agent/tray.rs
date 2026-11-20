pub(crate) use crate::provider_routing::provider_needs_routing;

pub(crate) fn tray_language() -> String {
    crate::settings::get_settings()
        .language
        .unwrap_or_else(|| "en".into())
}

pub(crate) fn refresh_tray_menu(_: &tauri::AppHandle) {}
pub(crate) fn schedule_tray_refresh(_: &tauri::AppHandle) {}
pub(crate) fn schedule_tray_status_check(_: &tauri::AppHandle) {}
