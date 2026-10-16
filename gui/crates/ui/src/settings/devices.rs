//! Settings → Devices (feature-inventory §1.5): the device registry — name,
//! platform, last-seen, an Online/Offline badge, a "This device" badge, click-to-copy id,
//! and a Rename dialog (Mutate renameDevice) — plus SSH devices: machines this
//! device reaches with its own ssh keys, set up and opened in their own window
//! ([`crate::ssh_devices`]).

use chrono::{DateTime, Utc};
use futures::StreamExt as _;
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, Focusable as _, SharedString, Subscription, Task,
    Window, div, prelude::*, px,
};
use std::path::PathBuf;
use std::time::Duration;

use zeron_proto::WorkspaceScope;
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::settings::widgets;
use crate::ssh_devices::{self, SetupOptions, SetupOutcome, SetupProgress, SshDevice};
use crate::state::AppState;
use crate::theme::Theme;

/// A device that pinged within this window shows a presence dot (engines
/// heartbeat every 15s; 70s tolerates a couple of missed beats).
pub const DEVICE_ONLINE_WINDOW_SECS: i64 = 70;

/// Presence: last-seen within the online window (future timestamps count). Pure.
pub fn device_online(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last_seen
        .is_some_and(|at| now.signed_duration_since(at).num_seconds() <= DEVICE_ONLINE_WINDOW_SECS)
}

/// Compact last-seen line. Pure.
pub fn format_last_seen(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(at) = last_seen else {
        return "never seen".to_string();
    };
    let secs = now.signed_duration_since(at).num_seconds();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Scope-aware copy: a local registry describes only the active local
/// workspace and must not imply that account device metadata is already live.
pub fn devices_subtitle(scope: Option<WorkspaceScope>) -> &'static str {
    match scope {
        Some(WorkspaceScope::Local) => "Devices in this local workspace.",
        Some(WorkspaceScope::Synced) => "Devices synced to this workspace.",
        Some(WorkspaceScope::Development) | None => "Devices in this workspace.",
    }
}

struct RenameDialog {
    device_id: String,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

enum SetupState {
    Running,
    Failed(SharedString),
    Done,
}

/// The setup run shown under "Add SSH device": one line per step, the last
/// one live while it runs.
struct SetupJob {
    host: String,
    steps: Vec<SharedString>,
    state: SetupState,
}

struct SshSection {
    /// Data dir the list was loaded from (`None` until the engine boot set it).
    data_dir: Option<PathBuf>,
    devices: Vec<SshDevice>,
    input: Entity<ComposerInput>,
    /// Host aliases from `~/.ssh/config`, offered as one-click fills.
    suggestions: Vec<String>,
    share_sign_in: bool,
    job: Option<SetupJob>,
    task: Option<Task<()>>,
    error: Option<SharedString>,
    _input_events: Subscription,
}

pub struct DevicesPage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    rename: Option<RenameDialog>,
    /// Device id whose id-chip shows "Copied" right now.
    copied: Option<String>,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    copy_task: Option<Task<()>>,
    ssh: SshSection,
    _observe: Subscription,
}

impl DevicesPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let input = cx.new(|cx| ComposerInput::new("Host from ~/.ssh/config, or user@host", cx));
        let input_events = cx.subscribe(&input, |this: &mut Self, input, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                let host = input.read(cx).text().trim().to_string();
                this.start_ssh_setup(host, cx);
            }
        });
        Self {
            state,
            scroll: widgets::PageScroll::default(),
            rename: None,
            copied: None,
            error: None,
            task: None,
            copy_task: None,
            ssh: SshSection {
                data_dir: None,
                devices: Vec::new(),
                input,
                suggestions: ssh_devices::ssh_config_hosts(),
                share_sign_in: true,
                job: None,
                task: None,
                error: None,
                _input_events: input_events,
            },
            _observe: observe,
        }
    }

    /// Load the saved SSH hosts once the data dir is known.
    fn ensure_ssh_loaded(&mut self, cx: &Context<Self>) {
        if self.ssh.data_dir.is_some() {
            return;
        }
        if let Some(dir) = self.state.read(cx).data_dir.clone() {
            self.ssh.devices = ssh_devices::load(&dir);
            self.ssh.data_dir = Some(dir);
        }
    }

    fn save_ssh_devices(&mut self) {
        if let Some(dir) = self.ssh.data_dir.as_deref()
            && let Err(err) = ssh_devices::save(dir, &self.ssh.devices)
        {
            self.ssh.error = Some(format!("Couldn't save SSH devices: {err}").into());
        }
    }

    /// Ask the engine to connect (or drop) SSH devices to match the saved
    /// list, so a ready host shows up in the composer's device picker.
    fn reload_engine_ssh_devices(&self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(err) = engine
                .client()
                .call(methods::RELOAD_SSH_DEVICES, serde_json::json!({}))
                .await
            {
                tracing::warn!(error = %err, "couldn't reload SSH devices");
            }
        })
        .detach();
    }

    fn ssh_setup_running(&self) -> bool {
        self.ssh
            .job
            .as_ref()
            .is_some_and(|job| matches!(job.state, SetupState::Running))
    }

    fn start_ssh_setup(&mut self, host: String, cx: &mut Context<Self>) {
        self.ensure_ssh_loaded(cx);
        if self.ssh_setup_running() {
            return;
        }
        if !ssh_devices::valid_host(&host) {
            self.ssh.error =
                Some("Enter an ssh host: an alias from ~/.ssh/config, or user@host.".into());
            cx.notify();
            return;
        }
        if self.ssh.data_dir.is_none() {
            return;
        }
        self.ssh.error = None;
        ssh_devices::record(&mut self.ssh.devices, &host, None, None);
        self.save_ssh_devices();
        self.ssh
            .input
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        self.ssh.job = Some(SetupJob {
            host: host.clone(),
            steps: Vec::new(),
            state: SetupState::Running,
        });
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<SetupProgress>();
        let options = SetupOptions {
            share_wizard_sign_in: self.ssh.share_sign_in,
        };
        let setup = gpui_tokio::Tokio::spawn(
            cx,
            ssh_devices::setup(host.clone(), options, move |progress| {
                let _ = tx.unbounded_send(progress);
            }),
        );
        self.ssh.task = Some(cx.spawn(async move |this, cx| {
            // The sender lives inside the setup future, so the stream ends
            // exactly when setup finishes.
            while let Some(progress) = rx.next().await {
                this.update(cx, |page, cx| {
                    if let Some(job) = page.ssh.job.as_mut() {
                        match progress {
                            SetupProgress::Step(text) => job.steps.push(text.into()),
                            SetupProgress::Update(text) => match job.steps.last_mut() {
                                Some(last) => *last = text.into(),
                                None => job.steps.push(text.into()),
                            },
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
            let outcome = match setup.await {
                Ok(Ok(outcome)) => Ok(outcome),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join) => Err(join.to_string()),
            };
            this.update(cx, |page, cx| page.finish_ssh_setup(host, outcome, cx))
                .ok();
        }));
        cx.notify();
    }

    fn finish_ssh_setup(
        &mut self,
        host: String,
        outcome: Result<SetupOutcome, String>,
        cx: &mut Context<Self>,
    ) {
        let (ok, platform) = match &outcome {
            Ok(outcome) => (true, Some(outcome.platform.clone())),
            Err(_) => (false, None),
        };
        ssh_devices::record(&mut self.ssh.devices, &host, Some(ok), platform);
        self.save_ssh_devices();
        if ok {
            self.reload_engine_ssh_devices(cx);
        }
        if let Some(job) = self.ssh.job.as_mut().filter(|job| job.host == host) {
            job.state = match outcome {
                Ok(_) => SetupState::Done,
                Err(message) => SetupState::Failed(message.into()),
            };
        }
        self.ssh.task = None;
        cx.notify();
    }

    fn open_ssh_window(&mut self, host: String, cx: &mut Context<Self>) {
        let Some(dir) = self.ssh.data_dir.clone() else {
            return;
        };
        if let Err(err) = ssh_devices::launch_window(&dir, &host) {
            self.ssh.error = Some(format!("Couldn't open {host}: {err:#}").into());
        }
        cx.notify();
    }

    fn remove_ssh_device(&mut self, host: String, cx: &mut Context<Self>) {
        if self.ssh_setup_running() && self.ssh.job.as_ref().is_some_and(|job| job.host == host) {
            return;
        }
        self.ssh.devices.retain(|device| device.host != host);
        if self.ssh.job.as_ref().is_some_and(|job| job.host == host) {
            self.ssh.job = None;
        }
        self.save_ssh_devices();
        self.reload_engine_ssh_devices(cx);
        cx.notify();
    }

    pub(crate) fn open_rename(
        &mut self,
        device_id: String,
        current: String,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| ComposerInput::new("Device name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename(cx);
            }
        });
        self.rename = Some(RenameDialog {
            device_id,
            input,
            _events: events,
        });
        cx.notify();
    }

    /// Escape that reached Settings unclaimed closes the rename dialog first,
    /// so it never closes Settings under the dialog. Returns whether it did.
    pub(crate) fn dismiss_on_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.rename.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if name.is_empty() {
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = serde_json::json!({
            "op": "renameDevice",
            "deviceId": dialog.device_id,
            "name": name,
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |page, cx| {
                if let Err(err) = result {
                    page.error = Some(format!("Rename failed: {err}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn copy_id(&mut self, device_id: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(device_id.clone()));
        self.copied = Some(device_id);
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            this.update(cx, |page, cx| {
                page.copied = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_rename_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let dialog = self.rename.as_ref()?;
        let input = dialog.input.clone();
        let card = popover::dialog_card(&theme)
            .id("rename-device-card")
            .role(gpui::Role::Dialog)
            .aria_label("Rename device")
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.rename = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(popover::dialog_title(&theme, "Rename device"))
            .child(
                div()
                    .mt(px(12.0))
                    .child(popover::dialog_field(input.into_any_element())),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        widgets::text_action(&theme, widgets::ActionTone::Quiet, "Cancel")
                            .id("rename-cancel")
                            .tab_index(0)
                            .role(gpui::Role::Button)
                            .focus_visible(|s| s.border_2().border_color(theme.accent).opacity(1.0))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        widgets::text_action(&theme, widgets::ActionTone::Solid, "Rename")
                            .id("rename-save")
                            .tab_index(0)
                            .role(gpui::Role::Button)
                            .focus_visible(|s| s.border_2().border_color(theme.accent).opacity(1.0))
                            .on_click(cx.listener(|this, _, _, cx| this.submit_rename(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal("rename-device-dialog", viewport, card))
    }

    fn render_ssh_section(
        &mut self,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let running_host = self
            .ssh
            .job
            .as_ref()
            .filter(|job| matches!(job.state, SetupState::Running))
            .map(|job| job.host.clone());
        let busy = running_host.is_some();

        let rows: Vec<AnyElement> = self
            .ssh
            .devices
            .iter()
            .enumerate()
            .map(|(ix, device)| {
                let host = device.host.clone();
                let setting_up = running_host.as_deref() == Some(host.as_str());
                let mut meta: Vec<AnyElement> = Vec::new();
                if let Some(platform) = device.remote_platform.as_deref() {
                    meta.push(
                        div()
                            .child(SharedString::from(platform.to_string()))
                            .into_any_element(),
                    );
                }
                let status = if setting_up {
                    "Setting up…"
                } else {
                    match device.last_setup_ok {
                        Some(true) => "Ready",
                        Some(false) => "Setup failed",
                        None => "Not set up",
                    }
                };
                meta.push(
                    div()
                        .when(device.last_setup_ok == Some(true) && !setting_up, |el| {
                            el.text_color(theme.success_muted.opacity(0.9))
                        })
                        .when(device.last_setup_ok == Some(false) && !setting_up, |el| {
                            el.text_color(theme.danger_muted.opacity(0.9))
                        })
                        .child(SharedString::from(status))
                        .into_any_element(),
                );
                meta.push(
                    div()
                        .child(SharedString::from(format!(
                            "Added {}",
                            format_last_seen(Some(device.added_at), now)
                        )))
                        .into_any_element(),
                );
                let action =
                    |id: (&'static str, usize), glyph: &'static str, label: &'static str| {
                        widgets::ghost_action(theme)
                            .id(id)
                            .opacity(if busy { 0.4 } else { 0.8 })
                            .hover(|s| widgets::ghost_hover(theme, s.opacity(1.0)))
                            .child(
                                crate::icons::icon(glyph)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from(label))
                    };
                let (open_host, setup_host, remove_host) =
                    (host.clone(), host.clone(), host.clone());
                widgets::card_row(theme, ix == 0)
                    .child(widgets::row_tile(theme, crate::icons::REMOTE_SERVER))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, host.clone()))
                            .child(widgets::meta_line(theme, meta)),
                    )
                    .when(device.last_setup_ok == Some(true), |el| {
                        el.child(
                            action(("ssh-open", ix), crate::icons::ARROW_UP_RIGHT, "New window")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_ssh_window(open_host.clone(), cx)
                                })),
                        )
                    })
                    .child(
                        action(
                            ("ssh-setup", ix),
                            crate::icons::RESTART,
                            if device.last_setup_ok.is_none() {
                                "Set up"
                            } else {
                                "Set up again"
                            },
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.start_ssh_setup(setup_host.clone(), cx)
                        })),
                    )
                    .child(
                        action(
                            ("ssh-remove", ix),
                            crate::icons::TRASH_BIN_MINIMALISTIC,
                            "Remove",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_ssh_device(remove_host.clone(), cx)
                        })),
                    )
                    .into_any_element()
            })
            .collect();

        // Add card: host field, ~/.ssh/config suggestions, the sign-in
        // sharing toggle, and the live setup steps.
        let suggestions: Vec<AnyElement> = self
            .ssh
            .suggestions
            .iter()
            .filter(|host| !self.ssh.devices.iter().any(|d| &d.host == *host))
            .take(12)
            .enumerate()
            .map(|(ix, host)| {
                let fill = host.clone();
                div()
                    .id(("ssh-suggestion", ix))
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.06)).text_color(theme.text))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ssh
                            .input
                            .update(cx, |input, cx| input.set_text(fill.clone(), cx));
                        window.focus(&this.ssh.input.focus_handle(cx), cx);
                        cx.notify();
                    }))
                    .child(SharedString::from(host.clone()))
                    .into_any_element()
            })
            .collect();
        let share = self.ssh.share_sign_in;
        let job = self.ssh.job.as_ref().map(|job| {
            let running = matches!(job.state, SetupState::Running);
            let last = job.steps.len().saturating_sub(1);
            let steps: Vec<AnyElement> = job
                .steps
                .iter()
                .enumerate()
                .map(|(ix, step)| {
                    let live = running && ix == last;
                    let failed = matches!(job.state, SetupState::Failed(_)) && ix == last;
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(crate::typography::ui_rems(12.5))
                        .text_color(if live { theme.text } else { theme.text_muted })
                        .child(div().flex_none().w(px(14.0)).flex().justify_center().child(
                            if live {
                                crate::life::life_spinner("ssh-setup-life", 10.0, theme.text_muted)
                                    .into_any_element()
                            } else if failed {
                                crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                    .size(px(14.0))
                                    .text_color(theme.danger_muted)
                                    .into_any_element()
                            } else {
                                crate::icons::icon(crate::icons::CHECK)
                                    .size(px(14.0))
                                    .text_color(theme.success_muted)
                                    .into_any_element()
                            },
                        ))
                        .child(div().min_w_0().child(step.clone()))
                        .into_any_element()
                })
                .collect();
            let host = job.host.clone();
            div()
                .px(px(20.0))
                .py(px(14.0))
                .border_t_1()
                .border_color(theme.border)
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(SharedString::from(format!("Setting up {host}"))),
                )
                .children(steps)
                .when_some(
                    match &job.state {
                        SetupState::Failed(message) => Some(message.clone()),
                        _ => None,
                    },
                    |el, message| el.child(widgets::error_strip(theme, message).mt(px(6.0))),
                )
                .when(matches!(job.state, SetupState::Done), |el| {
                    el.child(
                        div()
                            .mt(px(6.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(crate::typography::ui_rems(12.5))
                                    .text_color(theme.success_muted.opacity(0.9))
                                    .child(SharedString::from(format!(
                                        "{host} is ready — pick it in the device menu above the message box."
                                    ))),
                            )
                            .child(
                                popover::btn_ghost(theme, "New window", "ssh-open-ready")
                                    .id("ssh-open-ready")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_ssh_window(host.clone(), cx)
                                    })),
                            ),
                    )
                })
        });
        let add_card = widgets::section_card(theme)
            .mt(px(16.0))
            .child(
                div()
                    .px(px(20.0))
                    .py(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(widgets::field_label(theme, "Add SSH device"))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(div().flex_1().min_w_0().child(popover::dialog_field(
                                self.ssh.input.clone().into_any_element(),
                            )))
                            .child(
                                popover::btn_primary(theme, "Set up")
                                    .id("ssh-add")
                                    .flex_none()
                                    .when(busy, |el| el.opacity(0.5).cursor_default())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let host =
                                            this.ssh.input.read(cx).text().trim().to_string();
                                        this.start_ssh_setup(host, cx);
                                    })),
                            ),
                    )
                    .when(!suggestions.is_empty(), |el| {
                        el.child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .text_color(theme.text_muted.opacity(0.7))
                                        .child(SharedString::from("From ~/.ssh/config")),
                                )
                                .children(suggestions),
                        )
                    })
                    .child(
                        div()
                            .id("ssh-share-sign-in")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ssh.share_sign_in = !this.ssh.share_sign_in;
                                cx.notify();
                            }))
                            .child(widgets::toggle_switch(theme, share, "ssh-share-sign-in"))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_size(crate::typography::ui_rems(13.0))
                                            .text_color(theme.text)
                                            .child(SharedString::from(
                                                "Share this device's Wizard sign-in",
                                            )),
                                    )
                                    .child(
                                        div()
                                            .text_size(crate::typography::ui_rems(12.0))
                                            .text_color(theme.text_muted.opacity(0.7))
                                            .child(SharedString::from(
                                                "Copies the provider sign-in Wizard uses here, so \
                                                 it works there right away.",
                                            )),
                                    ),
                            ),
                    ),
            )
            .children(job);

        div()
            .mt(px(40.0))
            .flex()
            .flex_col()
            .child(widgets::page_header(
                theme,
                "SSH devices",
                (!self.ssh.devices.is_empty()).then_some(self.ssh.devices.len()),
            ))
            .child(widgets::page_subtitle(
                theme,
                "Run Wizard on another machine you can reach with ssh. Setup uses this \
                 device's SSH keys, copies Wizard and this app's engine there, and opens \
                 the machine in its own window.",
            ))
            .when_some(self.ssh.error.clone(), |el, message| {
                el.child(
                    widgets::error_strip(theme, message)
                        .id("ssh-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ssh.error = None;
                            cx.notify();
                        })),
                )
            })
            .when(!rows.is_empty(), |el| {
                el.child(widgets::section_card(theme).children(rows))
            })
            .child(add_card)
            .into_any_element()
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }
}

impl popover::ScrollRailHost for DevicesPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

/// Human platform label (zeron settings.devices.tsx `platformLabel`).
pub fn platform_label(platform: &str) -> &str {
    match platform {
        "macos" | "darwin" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        "web" => "Web",
        "ios" => "iOS",
        "android" => "Android",
        other => other,
    }
}

/// Short device id for the click-to-copy chip (`abcd1234…wxyz`).
pub fn short_id(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

impl Render for DevicesPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let now = Utc::now();
        let (devices, local_id, workspace_scope) = {
            let state = self.state.read(cx);
            (
                state.devices.clone(),
                state.local_device_id.clone(),
                state.workspace_scope,
            )
        };
        let copied = self.copied.clone();
        let dialog = self.render_rename_dialog(window.viewport_size(), cx);
        // A window already driving a remote engine manages SSH devices from
        // the main window instead.
        let ssh_section = if ssh_devices::remote_host().is_none() {
            self.ensure_ssh_loaded(cx);
            Some(self.render_ssh_section(&theme, now, cx))
        } else {
            None
        };
        // Split into this device and the rest; each renders as rows in one
        // block, like every other settings page.
        let (local, others): (Vec<_>, Vec<_>) = devices
            .into_iter()
            .enumerate()
            .partition(|(_, device)| local_id.as_deref() == Some(device.id.as_str()));
        let device_row = |ix: usize, device: zeron_proto::Device, first: bool| {
            let online = device_online(device.last_seen_at, now);
            let is_local = local_id.as_deref() == Some(device.id.as_str());
            let id_copied = copied.as_deref() == Some(device.id.as_str());
            let copy_id = device.id.clone();
            let rename_id = device.id.clone();
            let rename_name = device.name.clone();
            let mut meta: Vec<AnyElement> = vec![
                div()
                    .child(SharedString::from(
                        platform_label(&device.platform).to_string(),
                    ))
                    .into_any_element(),
            ];
            if let Some(version) = device.version.as_deref().filter(|v| !v.is_empty()) {
                meta.push(
                    div()
                        .child(SharedString::from(format!("v{version}")))
                        .into_any_element(),
                );
            }
            // Presence only says something about other devices.
            if !is_local {
                meta.push(if online {
                    div()
                        .text_color(theme.success_muted)
                        .child(SharedString::from("Online"))
                        .into_any_element()
                } else {
                    div()
                        .child(SharedString::from(format!(
                            "Last seen {}",
                            format_last_seen(device.last_seen_at, now)
                        )))
                        .into_any_element()
                });
            }
            widgets::card_row(&theme, first)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(160.0))
                        .child(widgets::row_title(&theme, device.name.clone()))
                        .child(widgets::meta_line(&theme, meta)),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(
                            widgets::text_action(
                                &theme,
                                widgets::ActionTone::Quiet,
                                if id_copied { "Copied" } else { "Copy ID" },
                            )
                            .id(("device-id", ix))
                            .tab_index(0)
                            .role(gpui::Role::Button)
                            .aria_label(format!("Copy device ID {}", device.id))
                            .focus_visible(|s| s.border_2().border_color(theme.accent))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.copy_id(copy_id.clone(), cx);
                                },
                            )),
                        )
                        .child(
                            widgets::text_action(&theme, widgets::ActionTone::Filled, "Rename")
                                .id(("device-rename", ix))
                                .tab_index(0)
                                .role(gpui::Role::Button)
                                .aria_label(format!("Rename {}", device.name))
                                .focus_visible(|s| s.border_2().border_color(theme.accent))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_rename(rename_id.clone(), rename_name.clone(), cx);
                                })),
                        ),
                )
                .into_any_element()
        };
        let local_block = (!local.is_empty()).then(|| {
            let mut block = widgets::section_card(&theme).mt(px(8.0));
            for (n, (ix, device)) in local.into_iter().enumerate() {
                block = block.child(device_row(ix, device, n == 0));
            }
            block
        });
        let others_block = if others.is_empty() {
            widgets::section_card(&theme).mt(px(8.0)).child(
                widgets::card_row(&theme, true).child(
                    div()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "Sign in on another device to see it here.",
                        )),
                ),
            )
        } else {
            let mut block = widgets::section_card(&theme).mt(px(8.0));
            for (n, (ix, device)) in others.into_iter().enumerate() {
                block = block.child(device_row(ix, device, n == 0));
            }
            block
        };
        let card = div()
            .flex()
            .flex_col()
            .when_some(local_block, |el, block| {
                el.child(widgets::section_label(&theme, "This device").mt(px(28.0)))
                    .child(block)
            })
            // A local-only workspace never has other devices to list.
            .when(workspace_scope != Some(WorkspaceScope::Local), |el| {
                el.child(widgets::section_label(&theme, "Other devices").mt(px(28.0)))
                    .child(others_block)
            });

        let scrollbar = popover::rail(self, "devices-page-scrollbar", &theme, cx);
        div()
            .id("devices-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                crate::edge_fade::edge_faded(
                    16.0,
                    true,
                    true,
                    div()
                        .id("devices-page")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll.scroll)
                        .child(
                            widgets::page_column()
                                .child(widgets::page_header(&theme, "Devices", None))
                                .child(widgets::page_subtitle(
                                    &theme,
                                    devices_subtitle(workspace_scope),
                                ))
                                .child(card)
                                .children(ssh_section),
                        ),
                )
                .fade_overflow_y(&self.scroll.scroll),
            )
            .children(scrollbar)
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn presence_window() {
        let now = Utc::now();
        assert!(device_online(Some(now - TimeDelta::seconds(10)), now));
        assert!(device_online(Some(now - TimeDelta::seconds(70)), now));
        assert!(!device_online(Some(now - TimeDelta::seconds(71)), now));
        assert!(!device_online(None, now));
        // Clock skew (future) counts as online.
        assert!(device_online(Some(now + TimeDelta::seconds(30)), now));
    }

    #[test]
    fn last_seen_formatting() {
        let now = Utc::now();
        assert_eq!(format_last_seen(None, now), "never seen");
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::seconds(30)), now),
            "just now"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::minutes(5)), now),
            "5m ago"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::hours(3)), now),
            "3h ago"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::days(2)), now),
            "2d ago"
        );
    }

    #[test]
    fn local_subtitle_does_not_claim_synced_metadata() {
        let copy = devices_subtitle(Some(WorkspaceScope::Local));
        assert!(copy.contains("local workspace"));
        assert!(!copy.contains("synced"));
    }
}
