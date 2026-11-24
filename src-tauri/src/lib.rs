#![cfg_attr(not(feature = "desktop"), allow(dead_code, unused_imports))]

const SSH_AGENT_PROTOCOL: u16 = 2;

mod agy_config;
#[cfg(all(test, unix))]
mod agy_smoke_tests;
mod app_config;
mod app_store;
#[cfg(feature = "desktop")]
mod app_update;
#[cfg(feature = "desktop")]
mod auto_launch;
mod claude_desktop_config;
mod claude_mcp;
mod claude_plugin;
mod codex_config;
mod codex_history_migration;
mod codex_rollout_file;
mod codex_state_db;
mod commands;
mod config;
mod copilot_config;
mod database;
mod deeplink;
mod dsh_config;
mod error;
#[cfg(test)]
mod frontend_parity_tests;
mod gemini_config;
mod gemini_mcp;
mod grok_config;
pub mod hermes_config;
mod init_status;
mod jsonc_document;
mod kimi_config;
mod kimi_session;
#[cfg(feature = "desktop")]
mod lightweight;
#[cfg(all(feature = "desktop", target_os = "linux"))]
mod linux_fix;
pub mod live;
mod log_redaction;
mod mcode_config;
mod mcp;
pub mod mode;
mod model_capabilities;
mod native_config;
mod omp_config;
mod openclaw_config;
mod opencode_config;
mod panic_hook;
mod pi_config;
mod prompt;
mod prompt_files;
mod provider;
mod provider_kind;
mod provider_routing;
mod proxy;
mod services;
mod session_manager;
mod settings;
#[cfg(feature = "desktop")]
mod ssh;
mod ssh_usage;
mod store;
mod sync_utils;
#[cfg(test)]
mod test_support;

#[cfg(feature = "desktop")]
mod tray;
#[cfg(not(feature = "desktop"))]
#[path = "ssh_agent/tray.rs"]
mod tray;
#[cfg(all(feature = "desktop", target_os = "windows"))]
mod tray_compat;
#[cfg(feature = "desktop")]
mod tray_panel;
mod usage_events;
mod usage_script;
#[cfg(feature = "desktop")]
mod window_fit;
mod workbuddy_config;

pub use app_config::{AppType, InstalledSkill, McpApps, McpServer, MultiAppConfig, SkillApps};
pub use codex_config::{
    extract_codex_experimental_bearer_token, get_codex_auth_path, get_codex_config_path,
    read_codex_live_settings, write_codex_live_atomic,
};
#[cfg(feature = "desktop")]
pub use commands::open_provider_terminal;
pub use commands::*;
pub use config::{get_claude_mcp_path, get_claude_settings_path, read_json_file};
pub use database::{Database, Profile};
pub use deeplink::{import_provider_from_deeplink, parse_deeplink_url, DeepLinkImportRequest};
pub use error::AppError;
pub use grok_config::get_grok_config_path;
pub(crate) use log_redaction::*;
pub use mcp::{
    import_from_claude, import_from_codex, import_from_gemini, import_from_grokbuild,
    remove_server_from_claude, remove_server_from_codex, remove_server_from_gemini,
    remove_server_from_grokbuild, sync_single_server_to_claude, sync_single_server_to_codex,
    sync_single_server_to_gemini, sync_single_server_to_grokbuild,
};
pub use prompt::Prompt;
pub use provider::{Provider, ProviderMeta};
#[doc(hidden)]
pub use services::provider::claude_direct::settle_claude_tombstones;
pub use services::{
    profile::{ProfilePayload, ProfileScope, ProfileService, NO_ACTIVE_PROMPT},
    provider::{
        reapply_current_codex_official_live, reapply_current_codex_third_party_live, EditorSave,
        EditorView,
    },
    skill::{migrate_skills_to_ssot, ImportSkillSelection},
    ConfigService, EndpointLatency, McpService, PromptService, ProviderService, ProxyService,
    SkillService, SpeedtestService,
};
pub use settings::{update_settings, AppSettings};
pub use store::AppState;
#[cfg(feature = "desktop")]
include!("desktop.rs");

#[cfg(not(feature = "desktop"))]
extern crate self as tauri;
#[cfg(not(feature = "desktop"))]
pub use cc_switch_ssh_macros::command;
#[cfg(not(feature = "desktop"))]
pub use ssh_agent::runtime::{async_runtime, AppHandle, Emitter, Manager, State};
#[cfg(not(feature = "desktop"))]
mod ssh_agent;
#[cfg(not(feature = "desktop"))]
pub use ssh_agent::run as run_ssh_agent;
