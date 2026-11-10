use std::sync::{LazyLock, atomic::{AtomicUsize, Ordering}};
pub const LANGUAGES: &[(&str, &str)] = &[("en", "English"), ("de", "Deutsch"), ("fr", "Français"), ("es", "Español"), ("it", "Italiano"), ("pt", "Português"), ("nl", "Nederlands"), ("pl", "Polski")];
pub fn supported(code: &str) -> bool { LANGUAGES.iter().any(|(c, _)| *c == code) }
pub fn system_language(values: &[&str]) -> String {
    let value = values.iter().find(|s| !s.is_empty()).copied().unwrap_or("en");
    let code = value.split(['_', '.', '@', '-']).next().unwrap_or("en").to_lowercase();
    if supported(&code) { code } else { "en".into() }
}
pub fn system_code() -> String {
    let values = ["LC_ALL", "LC_MESSAGES", "LANG"].map(|k| std::env::var(k).unwrap_or_default());
    system_language(&values.iter().map(String::as_str).collect::<Vec<_>>())
}
static LANGUAGE: LazyLock<AtomicUsize> = LazyLock::new(|| AtomicUsize::new(index(&system_code())));
fn index(code: &str) -> usize { LANGUAGES.iter().position(|(c, _)| *c == code).unwrap_or(0) }
pub fn set_language(setting: &str) { let code = if setting == "auto" { system_code() } else { setting.to_string() }; LANGUAGE.store(index(&code), Ordering::Relaxed); }
static TABLES: LazyLock<Vec<std::collections::HashMap<String, String>>> = LazyLock::new(|| {
    let assets = [
        (include_str!("../translations/en.json"), include_str!("../translations/extra/en.json")),
        (include_str!("../translations/de.json"), include_str!("../translations/extra/de.json")),
        (include_str!("../translations/fr.json"), include_str!("../translations/extra/fr.json")),
        (include_str!("../translations/es.json"), include_str!("../translations/extra/es.json")),
        (include_str!("../translations/it.json"), include_str!("../translations/extra/it.json")),
        (include_str!("../translations/pt.json"), include_str!("../translations/extra/pt.json")),
        (include_str!("../translations/nl.json"), include_str!("../translations/extra/nl.json")),
        (include_str!("../translations/pl.json"), include_str!("../translations/extra/pl.json")),
    ];
    assets.iter().map(|(base, extra)| {
        let mut table: std::collections::HashMap<String, String> = serde_json::from_str(base).unwrap();
        table.extend(serde_json::from_str::<std::collections::HashMap<String, String>>(extra).unwrap()); table
    }).collect()
});
pub fn tr(key: &str) -> &str {
    TABLES[LANGUAGE.load(Ordering::Relaxed)].get(key).map(String::as_str).unwrap_or(key)
}
pub fn trf(key: &str, args: &[String]) -> String {
    // One pass: values containing placeholders are data and are never substituted again.
    let text = tr(key); let mut out = String::new(); let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]); rest = &rest[start..];
        if let Some(end) = rest.find('}') {
            if let Ok(n) = rest[1..end].parse::<usize>() {
                if let Some(value) = args.get(n) { out.push_str(value); rest = &rest[end+1..]; continue; }
            }
        }
        out.push('{'); rest = &rest[1..];
    }
    out.push_str(rest); out
}
pub fn language_choices() -> Vec<(String, String)> {
    let code = system_code();
    let mut choices = vec![("auto".into(), trf("System ({0})", &[LANGUAGES[index(&code)].1.into()]))];
    choices.extend(LANGUAGES.iter().map(|(c, n)| (c.to_string(), n.to_string()))); choices
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn fallback_and_values() {
        assert_eq!(tr("Uncatalogued fallback"), "Uncatalogued fallback");
        assert_eq!(trf("{0} / {1}", &["Mouse {1}".into(), "42".into()]), "Mouse {1} / 42");
        assert_eq!(trf("{0} / {1}", &["42".into()]), "42 / {1}");
        for (index, table) in TABLES.iter().enumerate() {
            assert!(!table["Language"].is_empty(), "{}", LANGUAGES[index].0);
        }
    }
    #[test] fn locale_precedence() {
        for (values, expected) in [(["de_DE.UTF-8", "fr", "pl"], "de"), (["", "fr_FR", "de"], "fr"), (["", "", "pt-BR"], "pt"), (["xx", "de", "fr"], "en"), (["C", "de", "fr"], "en"), (["POSIX", "", "pl"], "en"), (["", "", ""], "en")] { assert_eq!(system_language(&values), expected); }
    }
}
