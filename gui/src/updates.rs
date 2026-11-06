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
///
/// Сначала используется GitHub API; если он недоступен (частая ситуация в
/// корпоративных сетях), читается Atom-фид релизов с github.com.
pub fn check(current: &str) -> Result<Option<UpdateInfo>, String> {
    match check_api(current) {
        Ok(result) => Ok(result),
        Err(api_err) => match check_atom(current) {
            Ok(result) => Ok(result),
            Err(atom_err) => Err(format!("{api_err}; {atom_err}")),
        },
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(6))
        .timeout(Duration::from_secs(12))
        .user_agent(concat!("RemontExplorer/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn check_api(current: &str) -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = match agent().get(&url).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
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

/// Запасной канал: Atom-фид релизов на github.com (доступен там, где API закрыт).
fn check_atom(current: &str) -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://github.com/{REPO}/releases.atom");
    let body = agent()
        .get(&url)
        .call()
        .map_err(|e| format!("фид релизов: {e}"))?
        .into_string()
        .map_err(|e| format!("фид релизов: чтение ответа: {e}"))?;

    match parse_atom(&body) {
        Some((version, page)) => {
            let version = version.trim_start_matches(['v', 'V']).to_string();
            if is_newer(&version, current) {
                Ok(Some(UpdateInfo { version, url: page }))
            } else {
                Ok(None)
            }
        }
        None => Ok(None),
    }
}

/// Достать (тег, ссылка) первого релиза из Atom-фида.
pub fn parse_atom(xml: &str) -> Option<(String, String)> {
    let entry_start = xml.find("<entry")?;
    let entry = &xml[entry_start..];
    let title_start = entry.find("<title>")? + "<title>".len();
    let title_end = title_start + entry[title_start..].find("</title>")?;
    let tag = entry[title_start..title_end].trim().to_string();
    if tag.is_empty() {
        return None;
    }
    // ссылка на страницу релиза — первый href .../releases/tag/...
    let href = entry
        .find("/releases/tag/")
        .map(|i| {
            let before = &entry[..i];
            let quote = before.rfind('"').map(|q| q + 1).unwrap_or(0);
            let after = &entry[i..];
            let end = after.find('"').unwrap_or(after.len());
            format!("https://github.com{}{}", &entry[quote..i], &after[..end])
        })
        .filter(|u| u.contains("/releases/tag/"))
        .unwrap_or_else(|| format!("https://github.com/{REPO}/releases"));
    Some((tag, href))
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
    fn parses_atom_feed() {
        let xml = r#"<?xml version="1.0"?><feed><entry><id>tag:github.com,2008:Repository/1/v2.1.0</id>
        <link rel="alternate" type="text/html" href="https://github.com/IntruderHere/RemontExplorer/releases/tag/v2.1.0"/>
        <title>v2.1.0</title></entry></feed>"#;
        let (tag, url) = parse_atom(xml).expect("фид разобран");
        assert_eq!(tag, "v2.1.0");
        assert!(url.ends_with("/releases/tag/v2.1.0"), "ссылка: {url}");
        assert!(parse_atom("<feed></feed>").is_none());
    }

    #[test]
    fn parses_version_parts() {
        assert_eq!(parse("v2.1.3"), (2, 1, 3));
        assert_eq!(parse("2.1"), (2, 1, 0));
        assert_eq!(parse("abc"), (0, 0, 0));
        assert_eq!(parse("2.1.3-beta.1"), (2, 1, 3));
    }
}
