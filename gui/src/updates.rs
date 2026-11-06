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

/// Достать (версия, ссылка) последнего релиза из Atom-фида.
///
/// Важно: в `<title>` GitHub кладёт НАЗВАНИЕ релиза («RemontExplorer 2.0.0»),
/// а не тег, поэтому версия берётся из ссылки `/releases/tag/v2.0.0`
/// (при её отсутствии — из `<id>` вида `tag:github.com,2008:Repository/1/v2.0.0`).
pub fn parse_atom(xml: &str) -> Option<(String, String)> {
    let entry_start = xml.find("<entry")?;
    let entry = &xml[entry_start..];
    let entry_end = entry.find("</entry>").unwrap_or(entry.len());
    let entry = &entry[..entry_end];

    // 1) ссылка на страницу релиза -> версия из хвоста
    if let Some(pos) = entry.find("/releases/tag/") {
        let after = &entry[pos + "/releases/tag/".len()..];
        let end = after
            .find(['"', '<', '?'])
            .unwrap_or(after.len());
        let tag = after[..end].trim();
        if !tag.is_empty() {
            // начало href — от последней кавычки перед "/releases"
            let before = &entry[..pos];
            let quote = before.rfind('"').map(|q| q + 1).unwrap_or(0);
            let url = format!("https://github.com{}{}", &entry[quote..pos], &entry[pos..pos + "/releases/tag/".len() + end]);
            if is_version_like(tag) {
                return Some((tag.to_string(), url));
            }
        }
    }

    // 2) запасной путь — <id>tag:github.com,2008:Repository/1/v2.0.0</id>
    if let Some(id_start) = entry.find("<id>") {
        let rest = &entry[id_start + 4..];
        if let Some(id_end) = rest.find("</id>") {
            let id = &rest[..id_end];
            if let Some(slash) = id.rfind('/') {
                let tag = id[slash + 1..].trim();
                if is_version_like(tag) {
                    return Some((
                        tag.to_string(),
                        format!("https://github.com/{REPO}/releases/tag/{tag}"),
                    ));
                }
            }
        }
    }
    None
}

/// Похоже ли на версию: начинается с цифры или `v` + цифра.
fn is_version_like(s: &str) -> bool {
    let s = s.trim_start_matches(['v', 'V']);
    s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
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
        // Реальный формат GitHub: в <title> — название релиза, тег — в ссылке.
        let xml = r#"<?xml version="1.0"?><feed><entry>
        <id>tag:github.com,2008:Repository/123456789/v2.1.0</id>
        <updated>2026-11-06T12:00:00Z</updated>
        <link rel="alternate" type="text/html" href="https://github.com/IntruderHere/RemontExplorer/releases/tag/v2.1.0"/>
        <title>RemontExplorer 2.1.0</title></entry></feed>"#;
        let (tag, url) = parse_atom(xml).expect("фид разобран");
        assert_eq!(tag, "v2.1.0", "версия должна браться из ссылки, а не из названия");
        assert!(url.ends_with("/releases/tag/v2.1.0"), "ссылка: {url}");

        // Без ссылки — версия из <id>.
        let xml2 = r#"<feed><entry><id>tag:github.com,2008:Repository/1/v3.0.0</id>
        <title>Release</title></entry></feed>"#;
        assert_eq!(parse_atom(xml2).unwrap().0, "v3.0.0");

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
