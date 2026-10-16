//! First-run onboarding: a full-window welcome over the Starship artwork that
//! installs agents, reuses sign-ins already on disk for Wizard, and sets the
//! defaults new chats start with. Finishing or skipping records
//! `onboardingCompleted`, so it shows once per install.

use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ObjectFit,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Task, div, img, prelude::FluentBuilder as _, px,
};
use zeron_engine::registry::HarnessDescriptor;
use zeron_engine::wizard_auth::{self, CredentialSource, DetectedCredential};
use zeron_proto::HarnessId;
use zeron_rpc::methods;

use super::Shell;
use crate::appearance::{self, AppearanceMode};
use crate::icons::{self, icon};
use crate::popover::{self, Loadable};
use crate::settings::{self, SavePolicy, widgets};
use crate::theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Agents,
    SignIn,
    Preferences,
    Done,
}

impl Step {
    const ALL: [Step; 5] = [
        Step::Welcome,
        Step::Agents,
        Step::SignIn,
        Step::Preferences,
        Step::Done,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|step| *step == self).unwrap_or(0)
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1).min(Self::ALL.len() - 1)]
    }

    fn back(self) -> Self {
        Self::ALL[self.index().saturating_sub(1)]
    }
}

/// The agents this GUI offers, in picker order, with onboarding blurbs.
const AGENTS: [(HarnessId, &str, &str); 3] = [
    (
        HarnessId::Wizard,
        "Wizard",
        "Wizard's own coding agent, running whichever model you sign it into.",
    ),
    (
        HarnessId::Pi,
        "Pi",
        "The minimal, extensible pi coding agent and its package ecosystem.",
    ),
    (
        HarnessId::ClaudeCode,
        "Claude Code",
        "Anthropic's Claude Code CLI.",
    ),
];

pub(super) struct Onboarding {
    step: Step,
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    installing: Option<HarnessId>,
    /// The engine's last answer about `installing`.
    install_progress: Option<zeron_harness::install_progress::InstallProgress>,
    /// The last failed install, shown under its agent with a Retry.
    install_error: Option<(HarnessId, String)>,
    default_agent: HarnessId,
    credentials: Loadable<Vec<DetectedCredential>>,
    importing: Option<CredentialSource>,
    /// Source → the Wizard provider it activated.
    imported: Option<(CredentialSource, String)>,
    /// Pi providers already holding a credential (read with `credentials`).
    pi_signed_in: Vec<String>,
    /// Source → the Pi provider it signed Pi in with.
    pi_imported: Option<(CredentialSource, String)>,
    error: Option<String>,
    tasks: Vec<Task<()>>,
}

impl Onboarding {
    fn new() -> Self {
        Self {
            step: Step::Welcome,
            harnesses: Loadable::Idle,
            installing: None,
            install_progress: None,
            install_error: None,
            default_agent: HarnessId::Wizard,
            credentials: Loadable::Idle,
            importing: None,
            imported: None,
            pi_signed_in: Vec::new(),
            pi_imported: None,
            error: None,
            tasks: Vec::new(),
        }
    }

    fn installed(&self, harness: HarnessId) -> Option<bool> {
        match &self.harnesses {
            Loadable::Ready(list) => Some(list.iter().any(|d| d.id == harness && d.installed)),
            _ => None,
        }
    }

    fn can_install(&self, harness: HarnessId) -> bool {
        match &self.harnesses {
            Loadable::Ready(list) => list.iter().any(|d| d.id == harness && d.can_install),
            _ => false,
        }
    }
}

impl Shell {
    /// Mount onboarding the first time the app is ready on this install.
    pub(super) fn ensure_onboarding(&mut self, cx: &mut Context<Self>) {
        if self.onboarding.is_some() || settings::current(cx).onboarding_completed {
            return;
        }
        self.onboarding = Some(Onboarding::new());
        self.onboarding_load_harnesses(cx);
        self.onboarding_load_credentials(cx);
    }

    fn onboarding_load_harnesses(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(onboarding) = self.onboarding.as_mut() else {
            return;
        };
        onboarding.harnesses = Loadable::Loading;
        let task = cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<HarnessDescriptor>>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |shell, cx| {
                if let Some(onboarding) = shell.onboarding.as_mut() {
                    onboarding.harnesses = match result {
                        Ok(list) => Loadable::Ready(list),
                        Err(error) => Loadable::Error(error),
                    };
                    // Default to the first offered agent that is present.
                    if onboarding.installed(onboarding.default_agent) == Some(false)
                        && let Some((id, _, _)) = AGENTS
                            .iter()
                            .find(|(id, _, _)| onboarding.installed(*id) == Some(true))
                    {
                        onboarding.default_agent = *id;
                    }
                }
                cx.notify();
            })
            .ok();
        });
        onboarding.tasks.push(task);
    }

    fn onboarding_load_credentials(&mut self, cx: &mut Context<Self>) {
        let Some(onboarding) = self.onboarding.as_mut() else {
            return;
        };
        onboarding.credentials = Loadable::Loading;
        let task = cx.spawn(async move |this, cx| {
            let (found, pi_signed_in) = cx
                .background_executor()
                .spawn(async move { (wizard_auth::detect(), wizard_auth::pi_signed_in()) })
                .await;
            this.update(cx, |shell, cx| {
                if let Some(onboarding) = shell.onboarding.as_mut() {
                    onboarding.credentials = Loadable::Ready(found);
                    onboarding.pi_signed_in = pi_signed_in;
                }
                cx.notify();
            })
            .ok();
        });
        onboarding.tasks.push(task);
    }

    fn onboarding_install(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(onboarding) = self.onboarding.as_mut() else {
            return;
        };
        if onboarding.installing.is_some() {
            return;
        }
        onboarding.installing = Some(harness);
        onboarding.install_progress = None;
        onboarding.install_error = None;
        onboarding.error = None;
        let poll = crate::install_bar::poll(
            engine.clone(),
            serde_json::json!({"harness": harness}),
            cx,
            move |shell: &mut Shell, progress| match shell.onboarding.as_mut() {
                Some(onboarding) if onboarding.installing == Some(harness) => {
                    // The last poll can land after the install finished.
                    if progress.is_some() {
                        onboarding.install_progress = progress;
                    }
                    true
                }
                _ => false,
            },
        );
        let Some(onboarding) = self.onboarding.as_mut() else {
            return;
        };
        onboarding.tasks.push(poll);
        let task = cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::INSTALL_HARNESS,
                    serde_json::json!({"harness": harness}),
                )
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<HarnessDescriptor>>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |shell, cx| {
                if let Some(onboarding) = shell.onboarding.as_mut() {
                    onboarding.installing = None;
                    onboarding.install_progress = None;
                    match result {
                        Ok(list) => {
                            onboarding.harnesses = Loadable::Ready(list);
                            crate::pickers::bump_harness_catalog(cx);
                        }
                        Err(error) => onboarding.install_error = Some((harness, error)),
                    }
                }
                cx.notify();
            })
            .ok();
        });
        onboarding.tasks.push(task);
        cx.notify();
    }

    fn onboarding_import(&mut self, source: CredentialSource, cx: &mut Context<Self>) {
        let Some(onboarding) = self.onboarding.as_mut() else {
            return;
        };
        if onboarding.importing.is_some() {
            return;
        }
        onboarding.importing = Some(source);
        onboarding.error = None;
        // One click signs in every agent that can use it: Wizard (unless this
        // is Wizard's own sign-in) and Pi when it is installed.
        let to_wizard = source != CredentialSource::Wizard;
        let to_pi = onboarding.installed(HarnessId::Pi) == Some(true);
        let task = cx.spawn(async move |this, cx| {
            let (wizard, pi) = cx
                .background_executor()
                .spawn(async move {
                    let err = |e: anyhow::Error| format!("{e:#}");
                    (
                        to_wizard.then(|| wizard_auth::import(source).map_err(err)),
                        to_pi.then(|| wizard_auth::import_into_pi(source).map_err(err)),
                    )
                })
                .await;
            this.update(cx, |shell, cx| {
                if let Some(onboarding) = shell.onboarding.as_mut() {
                    onboarding.importing = None;
                    let mut errors = Vec::new();
                    match wizard {
                        Some(Ok(provider)) => onboarding.imported = Some((source, provider)),
                        Some(Err(error)) => errors.push(format!("Wizard: {error}")),
                        None => {}
                    }
                    match pi {
                        Some(Ok(provider)) => {
                            onboarding.pi_signed_in.push(provider.clone());
                            onboarding.pi_imported = Some((source, provider));
                        }
                        Some(Err(error)) => errors.push(format!("Pi: {error}")),
                        None => {}
                    }
                    onboarding.error = (!errors.is_empty()).then(|| errors.join("\n"));
                }
                cx.notify();
            })
            .ok();
        });
        onboarding.tasks.push(task);
        cx.notify();
    }

    fn onboarding_go(&mut self, step: Step, cx: &mut Context<Self>) {
        if let Some(onboarding) = self.onboarding.as_mut() {
            onboarding.step = step;
            onboarding.error = None;
        }
        cx.notify();
    }

    /// Record completion and start new chats on the chosen agent.
    fn finish_onboarding(&mut self, cx: &mut Context<Self>) {
        let default_agent = self
            .onboarding
            .take()
            .map(|onboarding| onboarding.default_agent)
            .unwrap_or(HarnessId::Wizard);
        self.settings.onboarding_completed = true;
        settings::update(SavePolicy::Immediate, cx, |settings| {
            settings.onboarding_completed = true;
        });
        self.composer
            .read(cx)
            .pickers()
            .clone()
            .update(cx, |pickers, cx| pickers.pick_harness(default_agent, cx));
        cx.notify();
    }

    pub(super) fn render_onboarding(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let step = self.onboarding.as_ref()?.step;
        let theme = Theme::of(cx).clone();
        let card_theme = theme.for_popup();
        let body = match step {
            Step::Welcome => self.render_onboarding_welcome(&card_theme, cx),
            Step::Agents => self.render_onboarding_agents(&card_theme, cx),
            Step::SignIn => self.render_onboarding_sign_in(&card_theme, cx),
            Step::Preferences => self.render_onboarding_preferences(&card_theme, cx),
            Step::Done => self.render_onboarding_done(&card_theme, cx),
        };
        let dots = div()
            .flex()
            .flex_row()
            .gap(px(6.0))
            .children(Step::ALL.iter().map(|item| {
                div().size(px(6.0)).rounded_full().bg(if *item == step {
                    gpui::white()
                } else {
                    gpui::white().opacity(0.35)
                })
            }));
        let card = div()
            .w(px(540.0))
            .max_w_full()
            .p(px(28.0))
            .rounded(px(18.0))
            .bg(popover::surface_bg(&card_theme))
            .border_1()
            .border_color(crate::theme::hairline(0.12))
            .when(!card_theme.is_frost(), |el| el.shadow_lg())
            .flex()
            .flex_col()
            .gap(px(18.0))
            .text_color(card_theme.text)
            .child(body);
        let backdrop = settings::wizard_background_path(cx).map(|path| {
            img(path)
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .object_fit(ObjectFit::Cover)
        });
        // The overlay covers the titlebar strip; keep the window draggable.
        let drag = self.titlebar_drag_region(
            "onboarding-titlebar-drag",
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(Theme::TITLEBAR_HEIGHT)),
            cx,
        );
        Some(
            div()
                .id("onboarding")
                .absolute()
                .inset_0()
                .occlude()
                .bg(gpui::black())
                .children(backdrop)
                .child(div().absolute().inset_0().bg(gpui::black().opacity(0.52)))
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .pt(px(Theme::TITLEBAR_HEIGHT))
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(18.0))
                        .child(crate::motion::fade_in(
                            SharedString::from(format!("onboarding-step-{}", step.index())),
                            div().child(crate::frost::frosted(18.0, crate::frost::MENU_BLUR, card)),
                        ))
                        .child(dots),
                )
                .child(drag)
                .child(
                    div()
                        .absolute()
                        .left(px(20.0))
                        .top(px(Theme::TITLEBAR_HEIGHT + 8.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .text_color(gpui::white())
                        .child(
                            icon(icons::WIZARD_MARK)
                                .size(px(18.0))
                                .text_color(gpui::white()),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(13.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("Wizard GUI"),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_onboarding_welcome(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(18.0))
            .child(
                div()
                    .size(px(44.0))
                    .rounded(px(12.0))
                    .bg(theme.text)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        icon(icons::WIZARD_MARK)
                            .size(px(24.0))
                            .text_color(theme.on_solid),
                    ),
            )
            .child(title(theme, "Welcome to Wizard GUI"))
            .child(copy(
                theme,
                "Run Wizard, Pi, and Claude Code side by side across your projects, \
                 and on the machines you reach over SSH. Setup takes about a minute.",
            ))
            .child(
                footer()
                    .child(
                        popover::btn_ghost(theme, "Skip setup", "onboarding-skip")
                            .id("onboarding-skip")
                            .on_click(cx.listener(|this, _, _, cx| this.finish_onboarding(cx))),
                    )
                    .child(
                        popover::btn_primary(theme, "Get started")
                            .id("onboarding-start")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.onboarding_go(Step::Agents, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .pt(px(14.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::ui_rems(11.5))
                    .line_height(px(16.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(
                        "Wizard GUI is built on Zeron, the open-source agent controller \
                         created by the Zeron team (zeron.sh). Thank you to its creators.",
                    ),
            )
            .into_any_element()
    }

    fn render_onboarding_agents(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let onboarding = self.onboarding.as_ref().expect("onboarding step");
        let loading = matches!(onboarding.harnesses, Loadable::Idle | Loadable::Loading);
        let list_error = match &onboarding.harnesses {
            Loadable::Error(error) => Some(error.clone()),
            _ => None,
        };
        let rows: Vec<AnyElement> = AGENTS
            .iter()
            .map(|(harness, name, blurb)| {
                let harness = *harness;
                let installed = onboarding.installed(harness);
                let installing = onboarding.installing == Some(harness);
                let selected = onboarding.default_agent == harness;
                let failed = onboarding
                    .install_error
                    .as_ref()
                    .filter(|(id, _)| *id == harness)
                    .map(|(_, error)| error.clone());
                let (brand, tint) = crate::pickers::harness_brand_icon(harness);
                let status: AnyElement = if installing {
                    status_text(theme, "")
                } else {
                    match installed {
                        None => status_text(theme, ""),
                        Some(true) => div()
                            .id(SharedString::from(format!(
                                "onboarding-default-{harness:?}"
                            )))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(10.0))
                            .py(px(5.0))
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(if selected {
                                theme.text.opacity(0.6)
                            } else {
                                theme.border
                            })
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.glass_hover()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(onboarding) = this.onboarding.as_mut() {
                                    onboarding.default_agent = harness;
                                }
                                cx.notify();
                            }))
                            .when(selected, |el| {
                                el.child(icon(icons::CHECK).size(px(12.0)).text_color(theme.text))
                            })
                            .child(if selected { "Default" } else { "Make default" })
                            .into_any_element(),
                        // A failed install's Retry sits with its output below.
                        Some(false) if failed.is_some() => status_text(theme, ""),
                        Some(false) if onboarding.can_install(harness) => {
                            popover::btn_primary(theme, "Install")
                                .id(SharedString::from(format!(
                                    "onboarding-install-{harness:?}"
                                )))
                                .when(onboarding.installing.is_some(), |el| el.opacity(0.5))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.onboarding_install(harness, cx)
                                }))
                                .into_any_element()
                        }
                        Some(false) => status_text(theme, "Not installed"),
                    }
                };
                let row = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .py(px(10.0))
                    .child(
                        div()
                            .flex_none()
                            .size(px(34.0))
                            .rounded(px(9.0))
                            .border_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                icon(brand)
                                    .size(px(16.0))
                                    .text_color(tint.unwrap_or(theme.text)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(13.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(*name),
                            )
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(theme.text_muted)
                                    .child(*blurb),
                            ),
                    )
                    .child(div().flex_none().child(status));
                // Installs report under their row, indented to the text.
                let below: Option<AnyElement> = if installing {
                    Some(crate::install_bar::render(
                        theme,
                        format!("onboarding-install-progress-{harness:?}"),
                        onboarding.install_progress.as_ref(),
                    ))
                } else {
                    failed.map(|error| {
                        crate::install_bar::render_error(
                            theme,
                            format!("onboarding-install-error-{harness:?}"),
                            &format!("Couldn't install {name}: {error}"),
                            cx.listener(move |this, _, _, cx| this.onboarding_install(harness, cx)),
                        )
                    })
                };
                div()
                    .flex()
                    .flex_col()
                    .child(row)
                    .when_some(below, |el, below| {
                        el.child(div().pl(px(46.0)).pb(px(8.0)).child(below))
                    })
                    .into_any_element()
            })
            .collect();
        let error = onboarding.error.clone().or(list_error);
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(title(theme, "Choose your agents"))
            .child(copy(
                theme,
                "Install the agents you want. New chats start on the default; \
                 you can switch agents per chat at any time.",
            ))
            .child(if loading {
                popover::skeleton_rows("onboarding-agents", theme, 3, cx.entity_id(), cx)
            } else {
                div().flex().flex_col().children(rows).into_any_element()
            })
            .children(error.map(|error| error_line(theme, error)))
            .child(self.onboarding_nav(theme, Step::Agents, "Continue", cx))
            .into_any_element()
    }

    fn render_onboarding_sign_in(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let onboarding = self.onboarding.as_ref().expect("onboarding step");
        let content: AnyElement = match &onboarding.credentials {
            Loadable::Idle | Loadable::Loading => {
                popover::skeleton_rows("onboarding-sign-in", theme, 2, cx.entity_id(), cx)
            }
            Loadable::Error(error) => error_line(theme, error.clone()),
            Loadable::Ready(found) if found.is_empty() => copy(
                theme,
                "No Codex or Grok CLI sign-in was found on this computer. You can sign \
                 Wizard in later from a terminal with `wizard --login chatgpt` or \
                 `wizard --login xai`.",
            )
            .into_any_element(),
            Loadable::Ready(found) => div()
                .flex()
                .flex_col()
                .children(found.iter().map(|credential| {
                    let source = credential.source;
                    let using = onboarding
                        .imported
                        .as_ref()
                        .is_some_and(|(imported, _)| *imported == source);
                    let ready = source == CredentialSource::Wizard;
                    let busy = onboarding.importing == Some(source);
                    // Wizard's own sign-in is already Wizard's; it can still
                    // sign Pi in when Pi has nothing yet.
                    let pi_wants = onboarding.installed(HarnessId::Pi) == Some(true)
                        && onboarding.pi_signed_in.is_empty();
                    let action: AnyElement =
                        if ready && pi_wants && !busy {
                            popover::btn_primary(theme, "Use for Pi")
                                .id("onboarding-use-wizard-for-pi")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.onboarding_import(source, cx)
                                }))
                                .into_any_element()
                        } else if using || (ready && onboarding.imported.is_none()) {
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(4.0))
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text)
                                .child(icon(icons::CHECK).size(px(12.0)).text_color(theme.text))
                                .child(if using { "In use" } else { "Ready" })
                                .into_any_element()
                        } else if busy {
                            status_text(theme, "Connecting…")
                        } else {
                            popover::btn_primary(theme, if ready { "Use again" } else { "Use" })
                                .id(SharedString::from(format!("onboarding-use-{source:?}")))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.onboarding_import(source, cx)
                                }))
                                .into_any_element()
                        };
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(12.0))
                        .py(px(10.0))
                        .child(widgets::row_tile(
                            theme,
                            match source {
                                CredentialSource::Wizard => icons::WIZARD_MARK,
                                _ => icons::KEY_MINIMALISTIC,
                            },
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .text_size(crate::typography::ui_rems(13.5))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .child(credential.title.clone()),
                                )
                                .children(credential.detail.clone().map(|detail| {
                                    div()
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .text_color(theme.text_muted)
                                        .truncate()
                                        .child(detail)
                                })),
                        )
                        .child(div().flex_none().child(action))
                        .into_any_element()
                }))
                .into_any_element(),
        };
        let imported = onboarding.imported.as_ref().map(|(_, provider)| {
            copy(
                theme,
                format!("Wizard will use the \"{provider}\" provider for new chats."),
            )
            .into_any_element()
        });
        let pi_imported = onboarding.pi_imported.as_ref().map(|(_, provider)| {
            let name = match provider.as_str() {
                "openai-codex" => "your ChatGPT account",
                "xai" => "your xAI account",
                other => other,
            };
            copy(theme, format!("Pi is signed in with {name}.")).into_any_element()
        });
        let error = onboarding.error.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(title(theme, "Sign in your agents"))
            .child(copy(
                theme,
                "Wizard and Pi can reuse an account you're already signed in to with the \
                 Codex or Grok CLI on this computer. Nothing leaves this machine. The tools \
                 then share one sign-in, so if one is signed out later, sign in again.",
            ))
            .child(content)
            .children(imported)
            .children(pi_imported)
            .children(error.map(|error| error_line(theme, error)))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child("Claude Code keeps its own sign-in — see Settings → Agents."),
            )
            .child(self.onboarding_nav(theme, Step::SignIn, "Continue", cx))
            .into_any_element()
    }

    fn render_onboarding_preferences(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = settings::current(cx);
        let mode = appearance::mode(cx);
        let toggle_row = |id: &'static str,
                          name: &'static str,
                          detail: &'static str,
                          on: bool,
                          apply: fn(bool, &mut App),
                          cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .py(px(10.0))
                .cursor_pointer()
                .on_click(cx.listener(move |_, _, _, cx| {
                    apply(!on, cx);
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(13.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(name),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child(detail),
                        ),
                )
                .child(widgets::toggle_switch(theme, on, id))
        };
        let modes = [
            (AppearanceMode::System, "System"),
            (AppearanceMode::Light, "Light"),
            (AppearanceMode::Dark, "Dark"),
        ];
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(title(theme, "Make it yours"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(toggle_row(
                        "onboarding-compact",
                        "Compact mode",
                        "Fold each turn's tool calls and thinking into one line, \
                         so replies stay front and center.",
                        current.transcript_compact_mode,
                        settings::set_transcript_compact_mode,
                        cx,
                    ))
                    .child(toggle_row(
                        "onboarding-artwork",
                        "Starship artwork",
                        "Show the Starship backdrop behind new chats.",
                        current.new_thread_wizard_background,
                        settings::set_new_thread_wizard_background,
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(12.0))
                            .py(px(10.0))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(crate::typography::ui_rems(13.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child("Appearance"),
                            )
                            .child(div().flex().flex_row().gap(px(4.0)).children(
                                modes.into_iter().map(|(choice, label)| {
                                    let selected = choice == mode;
                                    div()
                                        .id(SharedString::from(format!(
                                            "onboarding-appearance-{label}"
                                        )))
                                        .px(px(10.0))
                                        .py(px(5.0))
                                        .rounded(px(8.0))
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .when(selected, |el| {
                                            el.bg(theme.text).text_color(theme.on_solid)
                                        })
                                        .when(!selected, |el| {
                                            el.text_color(theme.text_muted)
                                                .hover(|s| s.bg(theme.glass_hover()))
                                        })
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            appearance::set_mode(choice, cx);
                                            cx.notify();
                                        }))
                                        .child(label)
                                }),
                            )),
                    ),
            )
            .child(self.onboarding_nav(theme, Step::Preferences, "Continue", cx))
            .into_any_element()
    }

    fn render_onboarding_done(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let tips = [
            "Start a chat from the composer; pick Wizard, Pi, or Claude Code per chat.",
            "Add remote machines in Settings → Devices → Add SSH device.",
            "Install Pi plugins for Pi or Wizard in Settings → Plugins.",
            "Everything here lives in Settings — the gear at the bottom left.",
        ];
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(title(theme, "You're all set"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .children(tips.into_iter().map(|tip| {
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .text_size(crate::typography::ui_rems(13.0))
                            .line_height(px(19.0))
                            .text_color(theme.text_muted)
                            .child(div().flex_none().child("•"))
                            .child(div().flex_1().child(tip))
                    })),
            )
            .child(
                footer()
                    .child(
                        popover::btn_ghost(theme, "Back", "onboarding-done-back")
                            .id("onboarding-done-back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.onboarding_go(Step::Done.back(), cx)
                            })),
                    )
                    .child(
                        popover::btn_primary(theme, "Start chatting")
                            .id("onboarding-finish")
                            .on_click(cx.listener(|this, _, _, cx| this.finish_onboarding(cx))),
                    ),
            )
            .into_any_element()
    }

    fn onboarding_nav(
        &mut self,
        theme: &Theme,
        step: Step,
        next_label: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back_id = SharedString::from(format!("onboarding-back-{}", step.index()));
        let next_id = SharedString::from(format!("onboarding-next-{}", step.index()));
        footer()
            .child(
                popover::btn_ghost(theme, "Back", back_id.clone())
                    .id(back_id)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.onboarding_go(step.back(), cx)),
                    ),
            )
            .child(
                popover::btn_primary(theme, next_label)
                    .id(next_id)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.onboarding_go(step.next(), cx)),
                    ),
            )
            .into_any_element()
    }
}

fn title(theme: &Theme, text: &'static str) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(21.0))
        .line_height(px(26.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .child(text)
}

fn copy(theme: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(13.5))
        .line_height(px(20.0))
        .text_color(theme.text_muted)
        .child(text.into())
}

fn footer() -> gpui::Div {
    div()
        .pt(px(4.0))
        .flex()
        .flex_row()
        .justify_end()
        .items_center()
        .gap(px(8.0))
}

fn status_text(theme: &Theme, text: &'static str) -> AnyElement {
    div()
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text_muted)
        .child(text)
        .into_any_element()
}

fn error_line(theme: &Theme, error: String) -> AnyElement {
    div()
        .text_size(crate::typography::ui_rems(12.0))
        .line_height(px(17.0))
        .text_color(theme.danger)
        .child(error)
        .into_any_element()
}
