use crate::i18n::{self, tr, trf};
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Language(usize),
    Activate,
    Open,
    Quit,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
    pub children: Vec<Item>,
    pub action: Option<Action>,
}
impl Item {
    fn new(id: i32, label: impl Into<String>, enabled: bool, action: Option<Action>) -> Self {
        Self {
            id,
            label: label.into(),
            enabled,
            action,
            separator: false,
            children: vec![],
        }
    }
    pub fn find(&self, id: i32) -> Option<&Self> {
        if self.id == id {
            Some(self)
        } else {
            self.children.iter().find_map(|c| c.find(id))
        }
    }
    pub fn activate(&self, id: i32) -> Option<Action> {
        self.find(id).filter(|i| i.enabled).and_then(|i| i.action)
    }
}
pub fn menu() -> Item {
    let mut root = Item::new(0, "", true, None);
    root.children = vec![
        Item::new(1, trf("Open {0}", &[tr("Maprisma Bluetooth Monitor").into()]), true, Some(Action::Open)),
        Item::new(2, tr("Quit tray"), true, Some(Action::Quit)),
    ];
    let mut language = Item::new(3, tr("Language"), true, None);
    language.children = i18n::language_choices().into_iter().enumerate().map(|(i, (_, label))| Item::new(10 + i as i32, label, true, Some(Action::Language(i)))).collect();
    root.children.push(language);
    root
}
pub fn settings() -> serde_json::Value {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    base.and_then(|p| std::fs::read(p.join("maprisma-bluetooth-monitor/settings.json")).ok())
        .and_then(|s| serde_json::from_slice(&s).ok())
        .unwrap_or_default()
}
pub fn tooltip(snapshot: &serde_json::Value, settings: &serde_json::Value) -> String {
    let devices = snapshot["devices"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let visible: Vec<_> = devices
        .iter()
        .filter(|d| {
            let key = d["key"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or(d["id"].as_str());
            d["online"].as_bool() == Some(true)
                && !settings["hidden"]
                    .as_array()
                    .is_some_and(|h| h.iter().any(|v| v.as_str() == key))
        })
        .collect();
    if visible.is_empty() {
        return tr("No devices connected").into();
    }
    let lowest = visible
        .iter()
        .filter_map(|d| d["battery"]["percent"].as_i64())
        .min();
    match lowest {
        Some(p) => trf("{0} connected · lowest battery {1}%", &[visible.len().to_string(), p.to_string()]),
        None => trf("{0} connected · no battery reading", &[visible.len().to_string()]),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn menu_actions() {
        let m = menu();
        assert_eq!(m.children.len(), 3);
        assert_eq!(m.find(1).unwrap().label, trf("Open {0}", &[tr("Maprisma Bluetooth Monitor").into()]));
        assert_eq!(m.activate(1), Some(Action::Open));
        assert_eq!(m.activate(2), Some(Action::Quit));
        assert_eq!(m.activate(0), None);
        assert_eq!(m.activate(99), None);
    }
    #[test]
    fn tooltip_states() {
        assert_eq!(tooltip(&json!({}), &json!({})), tr("No devices connected"));
        let s = json!({"devices":[{"id":"a","online":true,"battery":{"percent":12}}, {"key":"b","online":true}, {"online":false,"battery":{"percent":1}}]});
        assert_eq!(tooltip(&s, &json!({})), trf("{0} connected · lowest battery {1}%", &["2".into(), "12".into()]));
        assert_eq!(
            tooltip(&s, &json!({"hidden":["a"]})),
            trf("{0} connected · no battery reading", &["1".into()])
        );
        assert_eq!(
            tooltip(&s, &json!({"hidden":["a","b"]})),
            tr("No devices connected")
        );
    }
}

pub fn save_language(code: &str) {
    let Some(base) = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))) else { return };
    let dir = base.join("maprisma-bluetooth-monitor");
    let mut data = settings(); if !data.is_object() { data = serde_json::json!({}); }
    data["language"] = code.into();
    if std::fs::create_dir_all(&dir).is_ok() {
        let tmp = dir.join(format!(".language.{}", std::process::id()));
        if std::fs::write(&tmp, format!("{data:#}\n")).is_ok() { let _ = std::fs::rename(tmp, dir.join("settings.json")); }
    }
}
