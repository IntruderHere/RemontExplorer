//! Проверка обновлений на GitHub.
//!
//! Запрашивает последний релиз публичного репозитория через GitHub API и
//! сравнивает его версию с текущей. Сеть может быть недоступна (корпоративный
//! прокси, блокировка) — тогда возвращается ошибка, но приложение продолжает
//! работать.

use std::time::Duration;

/// Репозиторий с релизами приложения.
pub const REPO: &str = "IntruderHere/RemontExplorer";

/// Информация о найденном обновлении.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    /// Версия из тега релиза, без префикса `v` (например, `2.1.0`).
    pub version: String,
    /// Страница релиза для скачивания.
    pub url: String,
}

/// Сравнить версии вида `2.0.0` / `v2.1` / `2.1.0-beta`.
/// Возвращает `Some(true)`, если `candidate` новее `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    parse(candidate) > parse(current)
}

/// Числовая часть версии: (major, minor, patch). Нераспознанное — нули.
fn parse(v: &str) -> (u64, u64, u64) {
    let v = v.trim().trim_start_matches(['v', 'V']);
    // отбрасываем суффикс предрелиза: "2.1.0-beta" -> "2.1.0"
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Проверить последний релиз. `Ok(None)` — обновлений нет.
pub fn check(current: &str) -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(6))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("RemontExplorer/", env!("CARGO_PKG_VERSION")))
        .build();

    let resp = match agent.get(&url).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(404, _)) => {
            // Релизов ещё нет — обновляться не на что.
            return Ok(None);
        }
        Err(e) => return Err(format!("GitHub API: {e}")),
    };

    let body = resp
        .into_string()
        .map_err(|e| format!("GitHub API: чтение ответа: {e}"))?;
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("GitHub API: некорректный ответ: {e}"))?;

    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if tag.is_empty() {
        return Ok(None);
    }
    let page = json
        .get("html_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("https://github.com/{REPO}/releases"));

    let version = tag.trim_start_matches(['v', 'V']).to_string();
    if is_newer(&version, current) {
        Ok(Some(UpdateInfo {
            version,
            url: page,
        }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("2.1.0", "2.0.0"));
        assert!(is_newer("v2.1.0", "2.0.9"));
        assert!(is_newer("3.0", "2.9.9"));
        assert!(is_newer("2.0.1", "2.0"));
        assert!(!is_newer("2.0.0", "2.0.0"));
        assert!(!is_newer("1.9.9", "2.0.0"));
        assert!(!is_newer("2.0.0-beta", "2.0.0"));
    }

    #[test]
    fn parses_version_parts() {
        assert_eq!(parse("v2.1.3"), (2, 1, 3));
        assert_eq!(parse("2.1"), (2, 1, 0));
        assert_eq!(parse("abc"), (0, 0, 0));
        assert_eq!(parse("2.1.3-beta.1"), (2, 1, 3));
    }
}
