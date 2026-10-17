// OWNER: dialogs
//! `/models`: the backend's models grouped by provider, with favorites and recent on top, and
//! the variant (reasoning effort) dialog that can follow a pick.

use agent_core::{Event, ModelOption, Request};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::SelectDialog;
use tuikit::select::{SelectItem, SelectState};
use tuikit::Theme;

use super::connect::{self, PROVIDERS};
use super::list::{Act, ListDialog, ListEvent};
use super::{Ctx, Dialog, Effect, Nav, Outcome};

#[derive(Clone, Debug, PartialEq)]
enum Pick {
    Model(String),
    Provider(usize),
}

pub struct ModelDialog {
    list: ListDialog<Pick>,
    models: Vec<ModelOption>,
    current: String,
    favorites: Vec<String>,
    recent: Vec<String>,
    /// Config knowledge needed to decide whether a variant has to be chosen after a pick.
    effort: String,
    efforts: Vec<String>,
    sections: bool,
}

/// Provider order: `opencode` first as in opencode, then alphabetical; models keep the order the
/// backend gave them.
fn provider_order(models: &[ModelOption]) -> Vec<String> {
    let mut p: Vec<String> = Vec::new();
    for m in models {
        if !p.contains(&m.provider) {
            p.push(m.provider.clone());
        }
    }
    p.sort_by_key(|n| (n != "opencode", n.to_lowercase()));
    p
}

fn build(
    models: &[ModelOption],
    current: &str,
    favorites: &[String],
    recent: &[String],
    sections: bool,
) -> Vec<SelectItem<Pick>> {
    let mut v: Vec<SelectItem<Pick>> = Vec::new();
    let find = |id: &str| models.iter().find(|m| m.id == id);
    let fav = |id: &str| favorites.iter().any(|f| f == id);
    let rec = |id: &str| recent.iter().any(|r| r == id);
    let row = |m: &ModelOption, group: Option<&str>, desc: Option<String>| {
        let mut it =
            SelectItem::new(Pick::Model(m.id.clone()), m.name.clone()).current(m.id == current);
        if let Some(g) = group {
            it = it.group(g);
        }
        if let Some(d) = desc {
            it = it.description(d);
        }
        it
    };
    if sections {
        for id in favorites {
            if let Some(m) = find(id) {
                v.push(row(m, Some("Favorites"), Some(m.provider.clone())));
            }
        }
        for id in recent.iter().filter(|r| !fav(r)) {
            if let Some(m) = find(id) {
                v.push(row(m, Some("Recent"), Some(m.provider.clone())));
            }
        }
    }
    for prov in provider_order(models) {
        for m in models.iter().filter(|m| m.provider == prov) {
            if sections && (fav(&m.id) || rec(&m.id)) {
                continue;
            }
            let group = (!m.provider.is_empty()).then_some(m.provider.as_str());
            v.push(row(m, group, fav(&m.id).then(|| "(Favorite)".to_string())));
        }
    }
    if models.is_empty() {
        for (i, p) in PROVIDERS.iter().enumerate().take(6) {
            v.push(
                SelectItem::new(Pick::Provider(i), p.title)
                    .description(p.description)
                    .group("Popular providers"),
            );
        }
    }
    v
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    let c = &ctx.config;
    let mut dlg = SelectDialog::new("Select model", Vec::new());
    dlg.state = SelectState::new(Vec::new()).flat(true);
    let mut d = ModelDialog {
        list: ListDialog::new(dlg).centered(),
        models: c.models.clone(),
        current: c.model.clone(),
        favorites: ctx.prefs.favorites.clone(),
        recent: ctx.prefs.recent.clone(),
        effort: c.effort.clone(),
        efforts: c.efforts.clone(),
        sections: true,
    };
    d.rebuild(true);
    Box::new(d)
}

impl ModelDialog {
    fn connected(&self) -> bool {
        !self.models.is_empty()
    }

    fn rebuild(&mut self, select_current: bool) {
        self.sections = self.list.query().trim().is_empty();
        let items = build(
            &self.models,
            &self.current,
            &self.favorites,
            &self.recent,
            self.sections,
        );
        self.list.dlg.state.set_items(items);
        if select_current && self.sections {
            self.list.dlg.state.select_current();
        }
        let mut fav = Act::new("favorite", "Favorite", "ctrl+f");
        fav.hidden = !self.connected();
        let provider = if self.connected() {
            "Connect provider"
        } else {
            "View all providers"
        };
        self.list
            .set_actions(vec![Act::new("provider", provider, "ctrl+a"), fav]);
    }

    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Filtered => {
                // Sections only show with an empty filter.
                if self.list.query().trim().is_empty() != self.sections {
                    self.rebuild(false);
                }
                Outcome::stay()
            }
            ListEvent::Select(i) => match self.list.items()[i].value.clone() {
                Pick::Model(id) => {
                    let effects = vec![
                        Effect::Request(Request::SetModel(id.clone())),
                        Effect::RecordModel(id),
                    ];
                    let valid = self.effort == "default" || self.efforts.contains(&self.effort);
                    let needs_variant = !self.efforts.is_empty() && !valid;
                    if needs_variant {
                        return Outcome {
                            nav: Nav::Replace(variant(&self.efforts, &self.effort)),
                            effects,
                        };
                    }
                    Outcome {
                        nav: Nav::Close,
                        effects,
                    }
                }
                Pick::Provider(p) => Outcome {
                    nav: Nav::Replace(connect::instructions(&PROVIDERS[p])),
                    effects: Vec::new(),
                },
            },
            ListEvent::Action("provider", _) => Outcome {
                nav: Nav::Replace(connect::open()),
                effects: Vec::new(),
            },
            ListEvent::Action("favorite", i) => {
                if let Pick::Model(id) = self.list.items()[i].value.clone() {
                    match self.favorites.iter().position(|f| *f == id) {
                        Some(p) => {
                            self.favorites.remove(p);
                        }
                        None => self.favorites.insert(0, id.clone()),
                    }
                    self.rebuild(false);
                    self.list
                        .dlg
                        .state
                        .select_where(|p| matches!(p, Pick::Model(m) if *m == id));
                    return Outcome::stay().with(Effect::ToggleFavorite(id));
                }
                Outcome::stay()
            }
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for ModelDialog {
    fn title(&self) -> &str {
        "Select model"
    }
    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ev = self.list.handle_key(key);
        self.pick(ev)
    }
    fn handle_paste(&mut self, text: &str) -> Outcome {
        let ev = self.list.handle_paste(text);
        self.pick(ev)
    }
    fn handle_mouse(&mut self, ev: MouseEvent) -> Outcome {
        let ev = self.list.handle_mouse(ev);
        self.pick(ev)
    }
    fn on_event(&mut self, ev: &Event) {
        if let Event::ConfigChanged(c) = ev {
            self.models = c.models.clone();
            self.current = c.model.clone();
            self.effort = c.effort.clone();
            self.efforts = c.efforts.clone();
            self.rebuild(false);
        }
    }
    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.list.draw(buf, screen, theme)
    }
}

// ---- variant (reasoning effort) ----------------------------------------------------------

pub struct VariantDialog {
    list: ListDialog<String>,
}

/// `Default` plus the backend's efforts; picking one sends `SetEffort`.
pub fn variant(efforts: &[String], current: &str) -> Box<dyn Dialog> {
    let mut items = vec![SelectItem::new("default".to_string(), "Default")
        .current(current.is_empty() || current == "default")];
    for e in efforts
        .iter()
        .filter(|e| !e.eq_ignore_ascii_case("default"))
    {
        items.push(SelectItem::new(e.clone(), e.clone()).current(e == current));
    }
    let st = SelectState::new(items).flat(true);
    let mut dlg = SelectDialog::new("Select variant", Vec::new());
    dlg.state = st;
    dlg.state.select_current();
    Box::new(VariantDialog {
        list: ListDialog::new(dlg).centered(),
    })
}

impl Dialog for VariantDialog {
    fn title(&self) -> &str {
        "Select variant"
    }
    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        match self.list.handle_key(key) {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Select(i) => {
                let v = self.list.items()[i].value.clone();
                Outcome::close_with(vec![Effect::Request(Request::SetEffort(v))])
            }
            _ => Outcome::stay(),
        }
    }
    fn handle_paste(&mut self, text: &str) -> Outcome {
        self.list.handle_paste(text);
        Outcome::stay()
    }
    fn handle_mouse(&mut self, ev: MouseEvent) -> Outcome {
        match self.list.handle_mouse(ev) {
            ListEvent::Select(i) => {
                let v = self.list.items()[i].value.clone();
                Outcome::close_with(vec![Effect::Request(Request::SetEffort(v))])
            }
            _ => Outcome::stay(),
        }
    }
    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.list.draw(buf, screen, theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, n: &str) -> ModelOption {
        ModelOption {
            id: format!("{p}/{n}"),
            name: n.into(),
            provider: p.into(),
        }
    }

    #[test]
    fn sections_come_first_and_do_not_repeat() {
        let models = vec![m("xai", "grok"), m("xai", "fast"), m("chatgpt", "gpt")];
        let v = build(
            &models,
            "xai/grok",
            &["chatgpt/gpt".to_string()],
            &["xai/fast".to_string(), "chatgpt/gpt".to_string()],
            true,
        );
        let g: Vec<_> = v
            .iter()
            .map(|i| (i.group.clone().unwrap(), i.title.clone()))
            .collect();
        assert_eq!(
            g,
            vec![
                ("Favorites".into(), "gpt".into()),
                ("Recent".into(), "fast".into()),
                ("xai".into(), "grok".into()),
            ]
        );
    }

    #[test]
    fn filtering_drops_sections_and_marks_favorites() {
        let models = vec![m("xai", "grok"), m("chatgpt", "gpt")];
        let v = build(&models, "", &["xai/grok".to_string()], &[], false);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].group.as_deref(), Some("xai"));
        assert_eq!(v[1].description.as_deref(), Some("(Favorite)"));
    }

    #[test]
    fn no_models_lists_how_to_connect() {
        let v = build(&[], "", &[], &[], true);
        assert!(v
            .iter()
            .all(|i| i.group.as_deref() == Some("Popular providers")));
        assert!(!v.is_empty());
    }
}
