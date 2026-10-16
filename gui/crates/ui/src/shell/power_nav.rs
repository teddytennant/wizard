//! Power user mode on the shell: which pane holds the navigation cursor, what
//! each navigation action does there, the focus rings, the `?` cheat sheet,
//! the which-key hint after a prefix key, and the archive confirmation.
//!
//! The keys themselves live in [`crate::power_keys`]. They are bound in the
//! gpui keymap and scoped to the key context of the shell's `unfocused`
//! target, so navigation mode is simply "that target has focus".

use super::*;
use crate::power_keys::{self, Command, Dir, Pane};

/// A focus ring laid over an element: absolute, never takes pointer input.
/// Callers set the inset.
pub fn nav_ring(theme: &Theme, radius: f32) -> gpui::Div {
    div()
        .absolute()
        .rounded(px(radius))
        .border_2()
        .border_color(theme.accent.opacity(0.85))
}

impl Shell {
    /// Route every navigation action to [`Self::run_nav`].
    pub(super) fn with_power_actions(
        el: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        macro_rules! route {
            ($el:expr, $($action:ident => $command:ident),* $(,)?) => {
                $el$(.on_action(cx.listener(|this, _: &power_keys::$action, window, cx| {
                    this.run_nav(Command::$command, window, cx)
                })))*
            };
        }
        let el = route!(el,
            Down => Down,
            Up => Up,
            Top => Top,
            Bottom => Bottom,
            HalfPageDown => HalfPageDown,
            HalfPageUp => HalfPageUp,
            Open => Open,
            Yank => Yank,
            Archive => Archive,
            Delete => Delete,
            Filter => Filter,
            FocusLeft => FocusLeft,
            FocusRight => FocusRight,
            FocusUp => FocusUp,
            FocusDown => FocusDown,
            FocusNext => FocusNext,
            FocusComposer => FocusComposer,
            NextTab => NextTab,
            PrevTab => PrevTab,
            NewChat => NewChat,
            PickAgent => PickAgent,
            PickModel => PickModel,
            PickEffort => PickEffort,
            PickDevice => PickDevice,
            Settings => Settings,
            Palette => Palette,
            CheatSheet => CheatSheet,
            NextSession => NextSession,
            PrevSession => PrevSession,
            TogglePanelSidebar => ToggleSidebar,
            TogglePanelRight => ToggleRight,
            TogglePanelTerminal => ToggleTerminal,
            Back => Back,
        );
        el.on_action(
            cx.listener(|this, tab: &power_keys::SelectTab, window, cx| {
                this.run_nav(Command::SelectTab(tab.0), window, cx)
            }),
        )
    }

    /// Per-frame bookkeeping: whether navigation has focus (rings), the
    /// transcript's cursor, and the pending-prefix observer for which-key.
    pub(super) fn sync_nav_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.nav_focus_visible = self.nav_mode(window);
        if !self.power_user_mode() {
            self.cheat_sheet = false;
            self.nav_archive_confirm = None;
        }
        let transcript_nav = self.nav_focus_visible
            && self.nav_pane == Pane::Transcript
            && matches!(self.route, Route::Chat);
        self.transcript
            .update(cx, |t, cx| t.set_nav_active(transcript_nav, cx));
        if self.pending_input_sub.is_none() {
            self.pending_input_sub = Some(cx.observe_pending_input(window, |this, _, cx| {
                if this.power_user_mode() {
                    cx.notify();
                }
            }));
        }
    }

    pub(super) fn power_user_mode(&self) -> bool {
        self.settings.power_user_mode
    }

    /// Whether the navigation target has focus (power user mode only).
    pub(super) fn nav_mode(&self, window: &Window) -> bool {
        self.power_user_mode() && self.unfocused.is_focused(window)
    }

    /// Key context for the navigation target. Settings keep the cursor in the
    /// settings list, which reads as the session list's area.
    pub(super) fn nav_key_context(&self) -> Option<gpui::KeyContext> {
        if !self.power_user_mode() {
            return None;
        }
        Some(power_keys::key_context(self.nav_area()))
    }

    /// The keymap area navigation keys resolve in.
    fn nav_area(&self) -> power_keys::Area {
        if matches!(self.route, Route::Settings(_)) {
            power_keys::Area::Sidebar
        } else {
            self.nav_pane.area().unwrap_or(power_keys::Area::Transcript)
        }
    }

    fn sidebar_open(&self) -> bool {
        !self.settings.sidebar_collapsed
    }

    /// Move the navigation cursor into `pane`. The composer is a text field,
    /// so "navigating" there means focusing the message box.
    pub(super) fn enter_pane(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        let pane = match pane {
            Pane::Sidebar if !self.sidebar_open() => Pane::Transcript,
            Pane::RightPanel if !matches!(self.route, Route::Chat) || !self.right_pane_open(cx) => {
                Pane::Transcript
            }
            pane => pane,
        };
        self.nav_pane = pane;
        let transcript_active = pane == Pane::Transcript && matches!(self.route, Route::Chat);
        self.transcript
            .update(cx, |t, cx| t.set_nav_active(transcript_active, cx));
        if pane == Pane::Composer {
            if matches!(self.route, Route::Chat) {
                window.focus(&self.composer.focus_handle(cx), cx);
            }
        } else {
            if pane == Pane::Sidebar && self.nav_sidebar_cursor.is_none() {
                self.nav_sidebar_cursor = self.state.read(cx).selected_chat.clone();
            }
            window.focus(&self.unfocused, cx);
        }
        cx.notify();
    }

    /// Leave a text field for navigation: back to the pane the cursor was in
    /// before, the transcript when there is none.
    pub(super) fn leave_to_navigation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = match self.nav_pane {
            Pane::Composer => {
                if matches!(self.route, Route::Chat) && self.state.read(cx).selected_chat.is_some()
                {
                    Pane::Transcript
                } else {
                    Pane::Sidebar
                }
            }
            pane => pane,
        };
        self.enter_pane(pane, window, cx);
    }

    /// `i` means insert: a message box in vim normal mode starts typing.
    fn composer_vim_insert(&mut self, cx: &mut Context<Self>) {
        let input = self.composer.read(cx).input.clone();
        input.update(cx, |input, cx| input.vim_insert(cx));
    }

    fn move_pane(&mut self, dir: Dir, window: &mut Window, cx: &mut Context<Self>) {
        let from = if self.composer.focus_handle(cx).contains_focused(window, cx) {
            Pane::Composer
        } else {
            self.nav_pane
        };
        let right_open = matches!(self.route, Route::Chat) && self.right_pane_open(cx);
        let to = power_keys::move_focus(from, dir, self.sidebar_open(), right_open);
        self.enter_pane(to, window, cx);
    }

    /// The sidebar rows navigation walks: the session list in chat, the
    /// section list in settings.
    fn sidebar_step(&mut self, step: SidebarStep, window: &mut Window, cx: &mut Context<Self>) {
        if let Route::Settings(current) = self.route {
            let sections: Vec<SettingsSection> = SettingsSection::ALL
                .into_iter()
                .filter(|s| *s != SettingsSection::Appshots || crate::appshots::is_desktop())
                .collect();
            let at = sections.iter().position(|s| *s == current).unwrap_or(0);
            let next = step.apply(at, sections.len());
            if let Some(&section) = sections.get(next) {
                self.open_settings(section, cx);
                // Opening a page may move focus into it; keep navigating.
                window.focus(&self.unfocused, cx);
            }
            return;
        }
        let order = self.sidebar_visible_order(cx);
        if order.is_empty() {
            return;
        }
        let at = self
            .nav_sidebar_cursor
            .as_ref()
            .and_then(|id| order.iter().position(|c| c == id))
            .or_else(|| {
                let selected = self.state.read(cx).selected_chat.clone()?;
                order.iter().position(|c| *c == selected)
            });
        let next = match at {
            Some(at) => step.apply(at, order.len()),
            None => match step {
                SidebarStep::By(d) if d < 0 => order.len() - 1,
                SidebarStep::Last => order.len() - 1,
                _ => 0,
            },
        };
        self.nav_sidebar_cursor = order.get(next).cloned();
        cx.notify();
    }

    fn sidebar_cursor_chat(&self, cx: &Context<Self>) -> Option<String> {
        let order = self.sidebar_visible_order(cx);
        self.nav_sidebar_cursor
            .clone()
            .filter(|id| order.contains(id))
            .or_else(|| self.state.read(cx).selected_chat.clone())
    }

    fn right_tab(&mut self, step: TabStep, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.active_chat.is_empty() {
            return;
        }
        if !self.right_pane_open(cx) {
            self.toggle_right_pane(cx);
        }
        let rows: Vec<RightSurface> = self
            .right_surface_rows(cx)
            .into_iter()
            .map(|(surface, _, _, _)| surface)
            .collect();
        if rows.is_empty() {
            return;
        }
        let current = self.resolved_right_active(cx);
        let at = rows.iter().position(|s| *s == current).unwrap_or(0);
        let next = match step {
            TabStep::Next => (at + 1) % rows.len(),
            TabStep::Prev => (at + rows.len() - 1) % rows.len(),
            TabStep::Index(ix) if ix < rows.len() => ix,
            TabStep::Index(_) => return,
        };
        self.set_right_active(rows[next], cx);
        // Activating a terminal tab asks it for focus; navigation keeps it.
        if self.nav_pane == Pane::RightPanel {
            window.focus(&self.unfocused, cx);
        }
        cx.notify();
    }

    /// Run one navigation command. Every binding in `power_keys` lands here.
    pub(super) fn run_nav(
        &mut self,
        command: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.power_user_mode() {
            return;
        }
        let in_chat = matches!(self.route, Route::Chat);
        // Settings have one list to walk: the section list.
        let pane = if in_chat {
            self.nav_pane
        } else {
            Pane::Sidebar
        };
        match command {
            Command::CheatSheet => {
                self.cheat_sheet = !self.cheat_sheet;
                cx.notify();
            }
            Command::Palette => self.toggle_command_palette(window, cx),
            Command::Filter => self.toggle_command_palette(window, cx),
            Command::Settings => self.open_settings(SettingsSection::Shortcuts, cx),
            Command::Back => {
                if self.cheat_sheet {
                    self.cheat_sheet = false;
                } else if matches!(self.route, Route::Settings(_)) {
                    self.close_settings(cx);
                    // Stay in navigation rather than landing in the message box.
                    self.composer.update(cx, |c, _| c.focus_pending = false);
                    window.focus(&self.unfocused, cx);
                }
                cx.notify();
            }
            Command::FocusComposer => {
                self.enter_pane(Pane::Composer, window, cx);
                self.composer_vim_insert(cx);
            }
            Command::FocusLeft => self.move_pane(Dir::Left, window, cx),
            Command::FocusRight => self.move_pane(Dir::Right, window, cx),
            Command::FocusUp => self.move_pane(Dir::Up, window, cx),
            Command::FocusDown => self.move_pane(Dir::Down, window, cx),
            Command::FocusNext => self.move_pane(Dir::Next, window, cx),
            Command::NewChat => {
                self.open_new_session(None, cx);
                self.enter_pane(Pane::Composer, window, cx);
                self.composer_vim_insert(cx);
            }
            Command::PickAgent | Command::PickModel if in_chat => {
                let pickers = self.composer.read(cx).pickers().clone();
                pickers.update(cx, |p, cx| p.open_model_menu(window, cx));
            }
            Command::PickEffort if in_chat => {
                let pickers = self.composer.read(cx).pickers().clone();
                pickers.update(cx, |p, cx| p.open_effort_menu(window, cx));
            }
            Command::PickDevice if in_chat => {
                if self.state.read(cx).selected_chat.is_some() {
                    self.open_new_session(None, cx);
                }
                let pickers = self.composer.read(cx).pickers().clone();
                pickers.update(cx, |p, cx| p.open_device_menu(window, cx));
            }
            Command::PickAgent | Command::PickModel | Command::PickEffort | Command::PickDevice => {
            }
            Command::NextSession => self.cycle_session(true, cx),
            Command::PrevSession => self.cycle_session(false, cx),
            Command::ToggleSidebar => self.toggle_sidebar(cx),
            Command::ToggleRight if in_chat => {
                self.toggle_right_pane(cx);
                if !self.right_pane_open(cx) && self.nav_pane == Pane::RightPanel {
                    self.enter_pane(Pane::Transcript, window, cx);
                }
            }
            Command::ToggleTerminal if in_chat => self.toggle_terminal(window, cx),
            Command::ToggleRight | Command::ToggleTerminal => {}
            Command::NextTab => self.right_tab(TabStep::Next, window, cx),
            Command::PrevTab => self.right_tab(TabStep::Prev, window, cx),
            Command::SelectTab(ix) => self.right_tab(TabStep::Index(ix), window, cx),
            Command::Down | Command::Up | Command::Top | Command::Bottom => {
                let step = match command {
                    Command::Down => SidebarStep::By(1),
                    Command::Up => SidebarStep::By(-1),
                    Command::Top => SidebarStep::First,
                    _ => SidebarStep::Last,
                };
                match pane {
                    Pane::Sidebar => self.sidebar_step(step, window, cx),
                    Pane::Transcript if in_chat => self.transcript.update(cx, |t, cx| match step {
                        SidebarStep::By(d) => t.nav_step(d, cx),
                        SidebarStep::First => t.nav_edge(false, cx),
                        SidebarStep::Last => t.nav_edge(true, cx),
                    }),
                    _ => {}
                }
            }
            Command::HalfPageDown | Command::HalfPageUp if in_chat => {
                let down = command == Command::HalfPageDown;
                self.transcript
                    .update(cx, |t, cx| t.nav_half_page(down, cx));
            }
            Command::HalfPageDown | Command::HalfPageUp => {}
            Command::Open => match pane {
                Pane::Sidebar if in_chat => {
                    if let Some(chat) = self.sidebar_cursor_chat(cx) {
                        self.open_chat(chat, cx);
                        // Opening from the list keeps you in the list; `i`
                        // (or Enter again from the transcript) goes to typing.
                        self.composer.update(cx, |c, _| c.focus_pending = false);
                        window.focus(&self.unfocused, cx);
                    }
                }
                Pane::Transcript => {
                    self.transcript.update(cx, |t, cx| {
                        t.nav_toggle(cx);
                    });
                }
                Pane::RightPanel if in_chat => {
                    let surface = self.resolved_right_active(cx);
                    match surface {
                        RightSurface::Terminal(_) => self.set_right_active(surface, cx),
                        RightSurface::File(_) | RightSurface::Browser(_) => {
                            self.focus_right_file_editor(surface, window, cx)
                        }
                        _ => {}
                    }
                }
                _ => {}
            },
            Command::Yank => {
                if pane == Pane::Transcript {
                    self.transcript.update(cx, |t, cx| {
                        t.nav_yank(cx);
                    });
                }
            }
            Command::Archive if in_chat && pane == Pane::Sidebar => {
                if let Some(chat) = self.sidebar_cursor_chat(cx) {
                    self.nav_archive_confirm = Some(chat);
                    cx.notify();
                }
            }
            Command::Delete if in_chat && pane == Pane::Sidebar => {
                if let Some(chat) = self.sidebar_cursor_chat(cx) {
                    self.delete_confirm = Some(chat);
                    cx.notify();
                }
            }
            Command::Archive | Command::Delete => {}
        }
    }

    /// Keyboard answers for the confirmation dialogs power mode opens (and
    /// the existing delete dialog, which only took clicks): Enter or y
    /// confirms, Esc or n cancels. Returns whether the key was used.
    pub(super) fn power_confirm_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        if !self.power_user_mode() {
            return false;
        }
        let confirm = matches!(key, "enter" | "y");
        let cancel = matches!(key, "escape" | "n");
        if let Some(chat) = self.nav_archive_confirm.clone() {
            if confirm {
                self.nav_archive_confirm = None;
                self.step_cursor_off(&chat, cx);
                self.archive_chat(chat, cx);
            } else if cancel {
                self.nav_archive_confirm = None;
            }
            cx.notify();
            return true;
        }
        if let Some(chat) = self.delete_confirm.clone() {
            if confirm {
                self.step_cursor_off(&chat, cx);
                self.delete_chat(chat, cx);
            } else if cancel {
                self.delete_confirm = None;
            }
            cx.notify();
            return true;
        }
        if self.cheat_sheet && cancel {
            self.cheat_sheet = false;
            cx.notify();
            return true;
        }
        false
    }

    /// Before a row leaves the list, park the cursor on its neighbor.
    fn step_cursor_off(&mut self, chat: &str, cx: &Context<Self>) {
        let order = self.sidebar_visible_order(cx);
        if let Some(at) = order.iter().position(|c| c == chat) {
            self.nav_sidebar_cursor = order
                .get(at + 1)
                .or_else(|| at.checked_sub(1).and_then(|i| order.get(i)))
                .cloned();
        }
    }

    /// Whether the sidebar row for `chat_id` carries the navigation ring.
    pub(super) fn nav_ring_on_row(&self, chat_id: &str, cx: &Context<Self>) -> bool {
        self.power_user_mode()
            && self.nav_pane == Pane::Sidebar
            && self.nav_focus_visible
            && self.sidebar_cursor_chat(cx).as_deref() == Some(chat_id)
    }

    /// Whether a settings nav row carries the ring.
    pub(super) fn nav_ring_on_settings(&self) -> bool {
        self.power_user_mode() && self.nav_focus_visible
    }

    /// A ring around the whole pane that holds the cursor.
    pub(super) fn pane_ring(&self, pane: Pane, cx: &App) -> Option<AnyElement> {
        (self.power_user_mode() && self.nav_focus_visible && self.nav_pane == pane).then(|| {
            let theme = Theme::of(cx);
            div()
                .absolute()
                .top(px(Theme::TITLEBAR_HEIGHT + 2.0))
                .bottom(px(4.0))
                .left(px(4.0))
                .right(px(4.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(theme.accent.opacity(0.35))
                .into_any_element()
        })
    }

    /// The archive confirmation power mode's `x` / `dd` opens.
    pub(super) fn render_nav_archive_confirm(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let chat_id = self.nav_archive_confirm.clone()?;
        let theme = Theme::of(cx).for_popup();
        let title = transcript::single_line(
            &self
                .state
                .read(cx)
                .chats
                .iter()
                .find(|c| c.id == chat_id)
                .and_then(|c| c.title.clone())
                .unwrap_or_else(|| "New session".into()),
        );
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, "Archive session?"))
            .child(div().mt(px(6.0)).child(popover::dialog_body(
                &theme,
                format!(
                    "\u{201C}{title}\u{201D} moves to Archived. You can restore it from there."
                ),
            )))
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "nav-archive-cancel")
                            .id("nav-archive-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.nav_archive_confirm = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_danger(&theme, "Archive")
                            .id("nav-archive-confirm")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.nav_archive_confirm = None;
                                this.step_cursor_off(&chat_id, cx);
                                this.archive_chat(chat_id.clone(), cx);
                            })),
                    ),
            )
            .child(confirm_keys_hint(
                &theme,
                "Enter or y archives, Esc or n cancels",
            ))
            .into_any_element();
        Some(popover::modal("nav-archive-dialog", viewport, card))
    }

    /// The `?` sheet: every navigation key, grouped by where it works.
    pub(super) fn render_cheat_sheet(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.cheat_sheet || !self.power_user_mode() {
            return None;
        }
        let theme = Theme::of(cx).for_popup();
        let groups = power_keys::sheet(self.settings.vim_composer);
        let key_chip = |keys: String| {
            div()
                .flex_none()
                .w(px(120.0))
                .font_family(theme.font_mono.clone())
                .text_size(crate::typography::ui_rems(11.5))
                .text_color(theme.text)
                .child(SharedString::from(keys))
        };
        let group_el = |title: &'static str, rows: Vec<(String, &'static str)>| {
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .pb(px(4.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.accent)
                        .child(SharedString::from(title)),
                )
                .children(rows.into_iter().map(|(keys, description)| {
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(px(8.0))
                        .child(key_chip(keys))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(crate::typography::ui_rems(11.5))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(description)),
                        )
                }))
        };
        // Three columns: anywhere | session list, transcript | right panel,
        // message box, vim.
        let mut columns: [Vec<AnyElement>; 3] = Default::default();
        for (ix, group) in groups.into_iter().enumerate() {
            let column = match ix {
                0 => 0,
                1 | 2 => 1,
                _ => 2,
            };
            columns[column].push(group_el(group.title, group.rows).into_any_element());
        }
        let [first, second, third] = columns.map(|groups| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(18.0))
                .children(groups)
        });
        let card = popover::dialog_card(&theme)
            .w(px(1040.0_f32.min(f32::from(viewport.width) - 48.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(popover::dialog_title(&theme, "Keyboard"))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("? or Esc closes")),
                    ),
            )
            .child(
                div()
                    .id("cheat-sheet-body")
                    .mt(px(14.0))
                    .max_h(px((f32::from(viewport.height) - 150.0).max(200.0)))
                    .overflow_y_scroll()
                    .flex()
                    .flex_row()
                    .gap(px(28.0))
                    .child(first)
                    .child(second)
                    .child(third),
            )
            .into_any_element();
        Some(popover::modal("cheat-sheet", viewport, card))
    }

    /// Which-key: after a prefix (`Space`, `g`, `d`, `Ctrl-w`), what can
    /// follow it, pinned to the bottom of the window until the next key.
    pub(super) fn render_which_key(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        if !self.nav_mode(window) {
            return None;
        }
        let pending = window.pending_input_keystrokes()?;
        let prefix: Vec<String> = pending.iter().map(|k| k.unparse()).collect();
        let rows = power_keys::continuations(&prefix, self.nav_area());
        if rows.is_empty() {
            return None;
        }
        let theme = Theme::of(cx).for_popup();
        let typed = power_keys::display_keys(&prefix.join(" "));
        Some(
            div()
                .absolute()
                .bottom(px(16.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    popover::popover_card(&theme)
                        .bg(theme.bg)
                        .px(px(14.0))
                        .py(px(10.0))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .text_size(crate::typography::ui_rems(11.5))
                                .text_color(theme.accent)
                                .child(SharedString::from(format!("{typed} \u{2026}"))),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap_x(px(18.0))
                                .gap_y(px(3.0))
                                .max_w(px(560.0))
                                .children(rows.into_iter().map(|(keys, description)| {
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap(px(6.0))
                                        .child(
                                            div()
                                                .font_family(theme.font_mono.clone())
                                                .text_size(crate::typography::ui_rems(11.5))
                                                .text_color(theme.text)
                                                .child(SharedString::from(keys)),
                                        )
                                        .child(
                                            div()
                                                .text_size(crate::typography::ui_rems(11.5))
                                                .text_color(theme.text_muted)
                                                .child(SharedString::from(description)),
                                        )
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// A small pill naming the mode and pane while navigating, so it is clear
    /// that letters are commands right now and not text.
    pub(super) fn render_nav_status(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        if !self.nav_mode(window) || window.pending_input_keystrokes().is_some() {
            return None;
        }
        let theme = Theme::of(cx).for_popup();
        let pane = match (self.route, self.nav_pane) {
            (Route::Settings(_), _) => "Settings",
            (_, Pane::Sidebar) => "Session list",
            (_, Pane::Transcript) => "Transcript",
            (_, Pane::RightPanel) => "Right panel",
            (_, Pane::Composer) => "Message box",
        };
        Some(
            div()
                .absolute()
                .bottom(px(10.0))
                .left(px(12.0))
                .child(
                    div()
                        .px(px(8.0))
                        .py(px(3.0))
                        .rounded(px(6.0))
                        .bg(theme.bg.opacity(0.9))
                        .border_1()
                        .border_color(theme.accent.opacity(0.5))
                        .flex()
                        .flex_row()
                        .gap(px(6.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.accent)
                                .child(SharedString::from("NAV")),
                        )
                        .child(
                            div()
                                .text_color(theme.text_muted)
                                .child(SharedString::from(format!("{pane} \u{00B7} ? for keys"))),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Shell {
    /// The app commands power user mode adds to the slash menu.
    pub(super) fn run_power_workspace_command(
        &mut self,
        command: crate::composer::WorkspaceCommand,
        args: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::composer::WorkspaceCommand as W;
        let in_chat = !self.active_chat.is_empty();
        match command {
            W::Agent => self.run_nav(Command::PickAgent, window, cx),
            W::Effort => self.run_nav(Command::PickEffort, window, cx),
            W::Device => self.run_nav(Command::PickDevice, window, cx),
            W::Archive if in_chat => self.archive_selected_chat(cx),
            W::Export if in_chat => {
                let markdown = {
                    let state = self.state.read(cx);
                    let title = state
                        .selected_chat_row()
                        .and_then(|chat| chat.title.clone())
                        .unwrap_or_else(|| "Conversation".into());
                    transcript_markdown(&title, &state.transcript)
                };
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(markdown));
            }
            W::Theme => {
                let mode = match args.as_deref().map(str::trim) {
                    Some("light") => Some(crate::appearance::AppearanceMode::Light),
                    Some("dark") => Some(crate::appearance::AppearanceMode::Dark),
                    Some("system") => Some(crate::appearance::AppearanceMode::System),
                    _ => None,
                };
                if let Some(mode) = mode {
                    crate::appearance::set_mode(mode, cx);
                    self.schedule_save(cx);
                }
            }
            W::Keys => {
                self.cheat_sheet = true;
                cx.notify();
            }
            W::Sidebar => self.toggle_sidebar(cx),
            W::Panel if in_chat => self.toggle_right_pane(cx),
            W::Next => self.cycle_session(true, cx),
            W::Prev => self.cycle_session(false, cx),
            W::Vim => {
                self.settings.vim_composer = !self.settings.vim_composer;
                self.schedule_save(cx);
                cx.notify();
            }
            _ => {}
        }
    }
}

/// `/export`: the conversation as Markdown, prompts and replies only (tool
/// calls and reasoning stay out). Pure.
pub(super) fn transcript_markdown(
    title: &str,
    entries: &[zeron_doc::SessionMessageEntry],
) -> String {
    let mut out = format!("# {title}\n");
    for entry in entries {
        let text: Vec<&str> = entry
            .parts
            .iter()
            .filter_map(|part| match part {
                zeron_doc::MessagePart::Text { text, .. } if !text.trim().is_empty() => {
                    Some(text.trim())
                }
                _ => None,
            })
            .collect();
        if text.is_empty() {
            continue;
        }
        let who = match entry.role {
            zeron_doc::MessageRole::User => "You",
            zeron_doc::MessageRole::Assistant => "Assistant",
            zeron_doc::MessageRole::System => "System",
        };
        out.push_str(&format!("\n## {who}\n\n{}\n", text.join("\n\n")));
    }
    out
}

/// The keyboard line under a power-mode confirmation's buttons.
pub(super) fn confirm_keys_hint(theme: &Theme, text: &'static str) -> gpui::Div {
    div()
        .mt(px(10.0))
        .flex()
        .justify_end()
        .text_size(crate::typography::ui_rems(11.0))
        .text_color(theme.text_muted)
        .child(SharedString::from(text))
}

/// A step through a list: by n rows (clamped), or to either end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SidebarStep {
    By(isize),
    First,
    Last,
}

impl SidebarStep {
    pub(super) fn apply(self, at: usize, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        match self {
            SidebarStep::By(d) => (at as isize + d).clamp(0, len as isize - 1) as usize,
            SidebarStep::First => 0,
            SidebarStep::Last => len - 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabStep {
    Next,
    Prev,
    Index(usize),
}

#[cfg(test)]
mod tests {
    use super::{SidebarStep, transcript_markdown};
    use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};

    fn entry(role: MessageRole, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: "m".into(),
            role,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
            duration_ms: None,
        }
    }

    fn text(t: &str) -> MessagePart {
        MessagePart::Text {
            id: "p".into(),
            text: t.into(),
        }
    }

    #[test]
    fn export_keeps_prompts_and_replies_only() {
        let entries = vec![
            entry(MessageRole::User, vec![text("fix the build")]),
            entry(
                MessageRole::Assistant,
                vec![
                    MessagePart::Reasoning {
                        id: "r".into(),
                        text: "thinking".into(),
                    },
                    text("Done."),
                    text("  "),
                ],
            ),
            entry(MessageRole::Assistant, vec![]),
        ];
        assert_eq!(
            transcript_markdown("Build", &entries),
            "# Build\n\n## You\n\nfix the build\n\n## Assistant\n\nDone.\n"
        );
    }

    #[test]
    fn sidebar_steps_clamp_instead_of_wrapping() {
        assert_eq!(SidebarStep::By(1).apply(0, 3), 1);
        assert_eq!(SidebarStep::By(1).apply(2, 3), 2);
        assert_eq!(SidebarStep::By(-1).apply(0, 3), 0);
        assert_eq!(SidebarStep::By(-5).apply(4, 6), 0);
        assert_eq!(SidebarStep::First.apply(2, 3), 0);
        assert_eq!(SidebarStep::Last.apply(0, 3), 2);
        assert_eq!(SidebarStep::Last.apply(0, 0), 0);
    }
}
