//! Settings → About: this copy's version and where it lives, the update check
//! (with its last run), the same Update / Restart controls as the sidebar
//! banner, and "Install updates automatically".

use gpui::{Context, Entity, SharedString, Subscription, Window, div, prelude::*, px};

use crate::icons;
use crate::popover;
use crate::settings::widgets;
use crate::theme::Theme;
use crate::updates::{Flow, Updates};

pub struct AboutPage {
    updates: Entity<Updates>,
    scroll: widgets::PageScroll,
    _updates: Subscription,
}

impl AboutPage {
    pub fn new(updates: Entity<Updates>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&updates, |_, _, cx| cx.notify());
        Self {
            updates,
            scroll: widgets::PageScroll::default(),
            _updates: subscription,
        }
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }
}

impl popover::ScrollRailHost for AboutPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

fn text(copy: impl Into<SharedString>) -> gpui::AnyElement {
    div().child(copy.into()).into_any_element()
}

/// The one-line update verdict under "Updates".
pub(crate) fn status_line(
    status: Option<&zeron_update::UpdateStatus>,
    available: Option<&str>,
    now_ms: i64,
) -> String {
    let Some(status) = status else {
        return "Update checks start once the engine is up.".into();
    };
    let checked = crate::updates::format_checked(status.checked_at, now_ms);
    if status.checking {
        return "Checking…".into();
    }
    if let Some(latest) = available {
        return format!("Wizard GUI {latest} is available · checked {checked}");
    }
    match (&status.error, status.checked_at) {
        (Some(error), _) => format!("Couldn't check for updates: {error}"),
        (None, Some(_)) => format!("Up to date · checked {checked}"),
        (None, None) => "Not checked yet".into(),
    }
}

impl Render for AboutPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let updates = self.updates.read(cx);
        let status = updates.status(cx);
        let available = updates.available(cx);
        let manual = updates.install_kind().manual_reason().map(str::to_string);
        let location = updates
            .install_kind()
            .location()
            .map(|path| path.display().to_string());
        let flow = updates.flow().clone();
        let progress = updates.progress();
        let idle = updates.idle(cx);
        let notes = updates.notes_url(cx);
        let auto = Updates::auto_install(cx);
        let now = chrono::Utc::now().timestamp_millis();
        let checking = status.as_ref().is_some_and(|s| s.checking);

        let handle = self.updates.clone();
        let check_button = popover::btn_ghost(&theme, "Check for updates", "about-check")
            .id("about-check")
            .flex_none()
            .when(checking, |el| el.opacity(0.5))
            .on_click(move |_, _, cx| handle.update(cx, |updates, cx| updates.check_now(cx)));

        let mut actions: Vec<gpui::AnyElement> = Vec::new();
        let button = |id: &'static str, label: &str, primary: bool| {
            if primary {
                popover::btn_primary(&theme, label).id(id)
            } else {
                popover::btn_ghost(&theme, label, id).id(id)
            }
        };
        let notes_button = |id: &'static str, label: &str, primary: bool| {
            let url = notes.clone();
            button(id, label, primary)
                .on_click(move |_, _, cx| {
                    if let Some(url) = &url {
                        cx.open_url(url);
                    }
                })
                .into_any_element()
        };
        let mut detail: Option<gpui::AnyElement> = None;
        if available.is_some() {
            let handle = self.updates.clone();
            match (&manual, &flow) {
                (Some(_), _) => actions.push(notes_button("about-download", "Download", true)),
                (None, Flow::Idle) => {
                    actions.push(
                        button("about-update", "Update", true)
                            .on_click(move |_, _, cx| handle.update(cx, |u, cx| u.start(cx)))
                            .into_any_element(),
                    );
                    actions.push(notes_button("about-notes", "What's new", false));
                }
                (None, Flow::Downloading) => {
                    detail = Some(crate::install_bar::render(
                        &theme,
                        "about-progress",
                        crate::updates::bar_progress(progress).as_ref(),
                    ));
                }
                (None, Flow::Ready(_)) => {
                    if idle {
                        actions.push(
                            button("about-restart", "Restart to update", true)
                                .on_click(move |_, _, cx| handle.update(cx, |u, cx| u.restart(cx)))
                                .into_any_element(),
                        );
                    } else {
                        detail = Some(text(
                            "Ready. Restart once agent runs and terminals finish, or it installs \
                             when you quit.",
                        ));
                    }
                    actions.push(notes_button("about-notes", "What's new", false));
                }
                (None, Flow::Failed(message)) => {
                    detail = Some(widgets::error_strip(&theme, message.clone()).into_any_element());
                    actions.push(
                        button("about-retry", "Retry", true)
                            .on_click(move |_, _, cx| handle.update(cx, |u, cx| u.start(cx)))
                            .into_any_element(),
                    );
                    actions.push(notes_button("about-download", "Download", false));
                }
            }
        }

        let where_line = match (&location, &manual) {
            (_, Some(reason)) => reason.clone(),
            (Some(path), None) => format!("Installed at {path}"),
            (None, None) => String::new(),
        };
        let version_row = widgets::card_row(&theme, true)
            .child(widgets::row_tile(&theme, icons::WIZARD_MARK))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(
                        &theme,
                        format!("Wizard GUI {}", zeron_update::current_version()),
                    ))
                    .child(widgets::meta_line(&theme, vec![text(where_line)])),
            );
        let updates_row = widgets::card_row(&theme, false)
            .child(widgets::row_tile(&theme, icons::REFRESH))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Updates"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![text(status_line(
                                    status.as_ref(),
                                    available.as_deref(),
                                    now,
                                ))],
                            )),
                    )
                    .children(detail)
                    .when(!actions.is_empty(), |el| {
                        el.child(div().flex().flex_row().gap(px(8.0)).children(actions))
                    }),
            )
            .child(check_button);
        let can_update = manual.is_none();
        let handle = self.updates.clone();
        let auto_row = widgets::card_row(&theme, false)
            .when(!can_update, |el| el.opacity(0.55))
            .child(widgets::row_tile(&theme, icons::RESTART))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(&theme, "Install updates automatically"))
                    .child(widgets::meta_line(
                        &theme,
                        vec![text(
                            "Download new versions in the background and install them the \
                             next time Wizard GUI quits or restarts.",
                        )],
                    )),
            )
            .child(
                div()
                    .id("about-auto-install")
                    .flex_none()
                    .size(px(40.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .role(gpui::Role::Switch)
                    .aria_label("Install updates automatically")
                    .aria_toggled(if auto {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .child(widgets::toggle_switch(&theme, auto, "auto-install-updates"))
                    .when(can_update, |el| {
                        el.cursor_pointer().on_click(move |_, _, cx| {
                            handle.update(cx, |u, cx| u.set_auto_install(!auto, cx))
                        })
                    }),
            );

        let card = widgets::section_card(&theme)
            .child(version_row)
            .child(updates_row)
            .child(auto_row);
        let scrollbar = popover::rail(self, "about-page-scrollbar", &theme, cx);
        div()
            .id("about-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                div()
                    .id("about-page")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll.scroll)
                    .child(
                        widgets::page_column()
                            .child(widgets::page_header(&theme, "About", None))
                            .child(
                                widgets::page_subtitle(
                                    &theme,
                                    "Updates come from Wizard's GitHub releases and are checked \
                                     against Wizard's release signing key before they install.",
                                )
                                .max_w(px(512.0))
                                .line_height(px(20.0)),
                            )
                            .when(zeron_update::signature::test_key_build(), |el| {
                                el.child(widgets::warning_strip(
                                    &theme,
                                    "This build trusts a test release key, not Wizard's.",
                                ))
                            })
                            .child(card),
                    ),
            )
            .children(scrollbar)
    }
}

#[cfg(test)]
mod tests {
    use super::status_line;
    use zeron_update::UpdateStatus;

    fn status() -> UpdateStatus {
        serde_json::from_value(serde_json::json!({"currentVersion": "3.6.1"})).unwrap()
    }

    #[test]
    fn the_status_line_says_what_the_last_check_found() {
        assert!(status_line(None, None, 0).contains("engine"));
        let mut s = status();
        assert_eq!(status_line(Some(&s), None, 0), "Not checked yet");
        s.checking = true;
        assert_eq!(status_line(Some(&s), None, 0), "Checking…");
        s.checking = false;
        s.checked_at = Some(0);
        assert_eq!(
            status_line(Some(&s), None, 120_000),
            "Up to date · checked 2m ago"
        );
        assert_eq!(
            status_line(Some(&s), Some("3.7.0"), 120_000),
            "Wizard GUI 3.7.0 is available · checked 2m ago"
        );
        s.error = Some("offline".into());
        assert_eq!(
            status_line(Some(&s), None, 120_000),
            "Couldn't check for updates: offline"
        );
    }
}
