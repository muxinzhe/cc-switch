use crate::{app_config::AppType, provider::Provider, provider_kind::ProviderKind};

pub(crate) fn is_official_account(app: &AppType, provider: &Provider) -> bool {
    provider.has_official_category()
        || (*app == AppType::Codex && crate::proxy::providers::is_codex_official_provider(provider))
}

/// Codex / Grok Build 配置里当前供应商的 `wire_api`；TOML 读不懂时退回唯一的一条赋值。
fn codex_wire_api(config_text: &str) -> Option<String> {
    if let Ok(doc) = config_text.parse::<toml::Value>() {
        return crate::codex_config::active_provider_str(&doc, "wire_api").map(str::to_string);
    }
    let mut found = config_text.lines().filter_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "wire_api").then(|| {
            value
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string()
        })
    });
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

/// 这家在这个应用下必须经过路由服务才能用（直连写进去也用不了）。和前端
/// `providerNeedsRouting`（`src/utils/providerCapabilities.ts`）是同一条规则，改一边要改另一边。
pub(crate) fn provider_needs_routing(app: &AppType, provider: &Provider) -> bool {
    if is_official_account(app, provider) {
        return false;
    }
    let meta = provider.meta.as_ref();
    let managed_oauth = ProviderKind::declared(provider).is_some();
    let full_url = meta.and_then(|meta| meta.is_full_url) == Some(true);
    let api_format = meta
        .and_then(|meta| meta.api_format.as_deref())
        .filter(|fmt| !fmt.is_empty());
    match app {
        AppType::ClaudeDesktop => {
            managed_oauth
                || meta
                    .and_then(|meta| meta.claude_desktop_mode.as_ref())
                    .is_some_and(|mode| *mode == crate::provider::ClaudeDesktopMode::Proxy)
        }
        AppType::Claude => {
            managed_oauth || full_url || api_format.is_some_and(|f| f != "anthropic")
        }
        AppType::Codex | AppType::GrokBuild => {
            managed_oauth
                || full_url
                || matches!(api_format, Some("openai_chat" | "anthropic"))
                || provider
                    .settings_config
                    .get("config")
                    .and_then(|config| config.as_str())
                    .and_then(codex_wire_api)
                    .is_some_and(|wire_api| {
                        crate::codex_config::is_chat_wire_api(&wire_api)
                            || crate::codex_config::is_anthropic_wire_api(&wire_api)
                    })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::codex_wire_api;

    #[test]
    fn codex_wire_api_falls_back_to_a_single_assignment_when_toml_is_broken() {
        assert_eq!(
            codex_wire_api("wire_api = \"chat\"\nbroken = [").as_deref(),
            Some("chat")
        );
        assert_eq!(
            codex_wire_api("wire_api = \"chat\"\nwire_api = \"responses\"\nx = ["),
            None
        );
    }
}
