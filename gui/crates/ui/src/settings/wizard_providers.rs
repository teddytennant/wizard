//! Settings → Wizard: the model providers Wizard runs on the selected device.
//!
//! Add one from a preset — an account sign-in, an API key, a local server, or
//! any OpenAI-compatible endpoint — reuse a Codex or Grok CLI sign-in, pick
//! the provider new sessions start on, or remove one. Provider state lives in
//! the target device's `~/.wizard`, so every change goes through the engine's
//! relay-forwardable `*WizardProvider*` RPCs and the page-header device
//! switcher (the Agents pattern) retargets them. The composer's Wizard model
//! list reloads after each change, so new providers' models show up at once.
//!
//! Usage shows each signed-in subscription's plan limits (xAI, ChatGPT) with
//! the Accounts page's meters, read through `WizardUsage` on the same device.

use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Task, Window, div, prelude::*,
    px,
};
use zeron_engine::wizard_auth::CredentialSource;
use zeron_engine::wizard_auth::providers::{
    Account, LoginState, LoginStatus, ProviderPreset, ProviderRow, WizardProviders,
};
use zeron_engine::wizard_auth::usage::{SubscriptionUsage, WizardUsage};
use zeron_rpc::methods;

use crate::composer::ComposerInput;
use crate::icons::{self, icon};
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const LOGIN_POLL: Duration = Duration::from_millis(1000);

/// The one change in flight — each reply carries the refreshed list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    Add(String),
    Remove(String),
    Activate(String),
    Import(CredentialSource),
}

pub struct WizardProvidersPage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    /// Which device's Wizard is shown/edited; `None` = this device.
    target_device: Option<String>,
    device_menu_open: bool,
    device_menu_pressed_open: bool,
    providers: Loadable<WizardProviders>,
    load_task: Option<Task<()>>,
    /// Each signed-in subscription's plan limits. Loaded with the page and on
    /// Refresh; a slow account never holds up the provider list.
    usage: Loadable<WizardUsage>,
    usage_task: Option<Task<()>>,
    pending: Option<Pending>,
    op_task: Option<Task<()>>,
    error: Option<String>,
    notice: Option<String>,
    /// The preset the add form is showing.
    preset: Option<String>,
    name: Entity<ComposerInput>,
    base_url: Entity<ComposerInput>,
    model: Entity<ComposerInput>,
    account_id: Entity<ComposerInput>,
    /// A pasted API key. Never rendered — only its last four characters.
    api_key: Option<String>,
    login: Option<LoginStatus>,
    login_task: Option<Task<()>>,
}

impl WizardProvidersPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| {
                ComposerInput::new(placeholder, cx)
                    .with_single_line()
                    .with_text_metrics(13.0, 18.0)
            })
        };
        let mut page = Self {
            state,
            scroll: widgets::PageScroll::default(),
            target_device: None,
            device_menu_open: false,
            device_menu_pressed_open: false,
            providers: Loadable::Idle,
            load_task: None,
            usage: Loadable::Idle,
            usage_task: None,
            pending: None,
            op_task: None,
            error: None,
            notice: None,
            preset: None,
            name: input("Provider name, e.g. groq", cx),
            base_url: input("https://api.example.com/v1", cx),
            model: input("Model id", cx),
            account_id: input("Cloudflare account id", cx),
            api_key: None,
            login: None,
            login_task: None,
        };
        page.load(cx);
        page.load_usage(cx);
        page
    }

    /// Params with the `targetDeviceId` passthrough merged in.
    fn with_target(&self, mut value: serde_json::Value) -> serde_json::Value {
        if let (Some(target), Some(object)) = (&self.target_device, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.device_menu_open = false;
        if self.target_device == target {
            cx.notify();
            return;
        }
        self.target_device = target;
        self.pending = None;
        self.op_task = None;
        self.error = None;
        self.notice = None;
        self.cancel_login(cx);
        self.providers = Loadable::Idle;
        self.load(cx);
        self.load_usage(cx);
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({}));
        let target = self.target_device.clone();
        self.providers = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_WIZARD_PROVIDERS, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<WizardProviders>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                page.providers = match result {
                    Ok(list) => Loadable::Ready(list),
                    Err(error) => Loadable::Error(error),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Ask the target device's Wizard for its subscriptions' plan limits.
    fn load_usage(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({}));
        let target = self.target_device.clone();
        self.usage = Loadable::Loading;
        self.usage_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::WIZARD_USAGE, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<WizardUsage>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                page.usage = match result {
                    Ok(usage) => Loadable::Ready(usage),
                    Err(error) => Loadable::Error(error),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Run one change on the target device; every reply carries the list.
    fn run(&mut self, pending: Pending, params: serde_json::Value, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let method = match &pending {
            Pending::Add(_) => methods::ADD_WIZARD_PROVIDER,
            Pending::Remove(_) => methods::REMOVE_WIZARD_PROVIDER,
            Pending::Activate(_) => methods::SET_ACTIVE_WIZARD_PROVIDER,
            Pending::Import(_) => methods::IMPORT_WIZARD_SIGN_IN,
        };
        let params = self.with_target(params);
        let target = self.target_device.clone();
        self.pending = Some(pending.clone());
        self.error = None;
        self.notice = None;
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<WizardProviders>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                page.pending = None;
                match result {
                    Ok(list) => {
                        page.notice = Some(match &pending {
                            Pending::Add(_) | Pending::Import(_) => format!(
                                "Added. New Wizard chats use {}.",
                                list.active.as_deref().unwrap_or("it")
                            ),
                            Pending::Remove(name) => format!("Removed {name}."),
                            Pending::Activate(name) => {
                                format!("New Wizard chats start on {name}.")
                            }
                        });
                        if matches!(pending, Pending::Add(_)) {
                            page.reset_form(cx);
                        }
                        page.providers = Loadable::Ready(list);
                        crate::pickers::bump_model_catalog(cx);
                    }
                    Err(error) => page.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn presets(&self) -> Vec<ProviderPreset> {
        self.providers
            .ready()
            .map(|list| list.presets.clone())
            .unwrap_or_default()
    }

    fn selected_preset(&self) -> Option<ProviderPreset> {
        let id = self.preset.as_deref()?;
        self.presets().into_iter().find(|preset| preset.id == id)
    }

    fn choose_preset(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(preset) = self.presets().into_iter().find(|preset| preset.id == id) else {
            return;
        };
        self.preset = Some(id);
        self.api_key = None;
        self.error = None;
        let name = if preset.custom {
            String::new()
        } else {
            preset.name.clone()
        };
        let base_url = if preset.custom {
            String::new()
        } else {
            preset.base_url.clone()
        };
        self.name.update(cx, |input, cx| input.set_text(name, cx));
        self.base_url
            .update(cx, |input, cx| input.set_text(base_url, cx));
        self.model
            .update(cx, |input, cx| input.set_text(preset.model.clone(), cx));
        self.account_id
            .update(cx, |input, cx| input.set_text("", cx));
        cx.notify();
    }

    fn reset_form(&mut self, cx: &mut Context<Self>) {
        self.preset = None;
        self.api_key = None;
        for input in [&self.name, &self.base_url, &self.model, &self.account_id] {
            input.update(cx, |input, cx| input.set_text("", cx));
        }
    }

    /// Take the API key from the clipboard: it is never typed into (or shown
    /// by) a visible field.
    fn paste_key(&mut self, cx: &mut Context<Self>) {
        let key = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty() && !text.contains(char::is_whitespace));
        match key {
            Some(key) => {
                self.api_key = Some(key);
                self.error = None;
            }
            None => self.error = Some("Copy your API key first, then paste it here.".into()),
        }
        cx.notify();
    }

    fn add(&mut self, cx: &mut Context<Self>) {
        let Some(preset) = self.selected_preset() else {
            return;
        };
        let text = |input: &Entity<ComposerInput>, cx: &mut Context<Self>| {
            let value = input.read(cx).text().trim().to_string();
            (!value.is_empty()).then_some(value)
        };
        let mut params = serde_json::json!({ "preset": preset.id });
        let object = params.as_object_mut().expect("object");
        if preset.custom
            && let Some(name) = text(&self.name, cx)
        {
            object.insert("name".into(), name.into());
        }
        if (preset.custom || preset.local)
            && let Some(url) = text(&self.base_url, cx)
        {
            object.insert("baseUrl".into(), url.into());
        }
        if let Some(model) = text(&self.model, cx) {
            object.insert("model".into(), model.into());
        }
        if preset.needs_account_id
            && let Some(account) = text(&self.account_id, cx)
        {
            object.insert("accountId".into(), account.into());
        }
        if let Some(key) = &self.api_key {
            object.insert("apiKey".into(), key.clone().into());
        }
        self.run(Pending::Add(preset.id.clone()), params, cx);
    }

    fn start_login(&mut self, account: Account, cx: &mut Context<Self>) {
        if self.login.is_some() || self.pending.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let target = self.target_device.clone();
        let start = self.with_target(serde_json::json!({ "account": account }));
        self.error = None;
        self.notice = None;
        self.login = Some(LoginStatus {
            login_id: String::new(),
            account,
            state: LoginState::Running,
            url: None,
            message: None,
            providers: None,
        });
        let poll_target = target.clone();
        self.login_task = Some(cx.spawn(async move |this, cx| {
            let parse = |value: Result<serde_json::Value, zeron_rpc::RpcError>| {
                value.map_err(|error| error.to_string()).and_then(|value| {
                    serde_json::from_value::<LoginStatus>(value).map_err(|e| e.to_string())
                })
            };
            let mut status = parse(
                engine
                    .client()
                    .call(methods::START_WIZARD_LOGIN, start)
                    .await,
            );
            loop {
                let finished = match &status {
                    Ok(status) => status.state != LoginState::Running,
                    Err(_) => true,
                };
                let keep = this
                    .update(cx, |page, cx| {
                        if page.target_device != poll_target || page.login.is_none() {
                            return false;
                        }
                        match &status {
                            Ok(current) => page.login = Some(current.clone()),
                            Err(error) => {
                                page.login = None;
                                page.error = Some(error.clone());
                            }
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep || finished {
                    break;
                }
                cx.background_executor().timer(LOGIN_POLL).await;
                let login_id = status
                    .as_ref()
                    .map(|status| status.login_id.clone())
                    .unwrap_or_default();
                let mut poll = serde_json::json!({ "loginId": login_id });
                if let (Some(target), Some(object)) = (&poll_target, poll.as_object_mut()) {
                    object.insert("targetDeviceId".into(), serde_json::json!(target));
                }
                status = parse(engine.client().call(methods::POLL_WIZARD_LOGIN, poll).await);
            }
            this.update(cx, |page, cx| {
                let Some(status) = page.login.take() else {
                    return;
                };
                match status.state {
                    LoginState::Done => {
                        page.notice = Some(format!(
                            "Signed in. New Wizard chats use {}.",
                            status.message.as_deref().unwrap_or("it")
                        ));
                        page.reset_form(cx);
                        if let Some(list) = status.providers {
                            page.providers = Loadable::Ready(list);
                        } else {
                            page.load(cx);
                        }
                        page.load_usage(cx);
                        crate::pickers::bump_model_catalog(cx);
                    }
                    LoginState::Failed => page.error = status.message,
                    LoginState::Running => {}
                }
                cx.notify();
            })
            .ok();
        }));
        let _ = target;
        cx.notify();
    }

    fn cancel_login(&mut self, cx: &mut Context<Self>) {
        self.login_task = None;
        let Some(login) = self.login.take() else {
            return;
        };
        if login.login_id.is_empty() {
            return;
        }
        if let Some(engine) = self.state.read(cx).engine().cloned() {
            let params = self.with_target(serde_json::json!({ "loginId": login.login_id }));
            cx.spawn(async move |_, _| {
                let _ = engine
                    .client()
                    .call(methods::CANCEL_WIZARD_LOGIN, params)
                    .await;
            })
            .detach();
        }
        cx.notify();
    }

    /// Account sign-in opens a browser on the device that runs Wizard, so it
    /// only works for the machine this window is on.
    fn target_is_this_machine(&self) -> bool {
        self.target_device.is_none() && crate::ssh_devices::remote_host().is_none()
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    fn spinner_line(
        &self,
        key: &str,
        label: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .text_size(crate::typography::ui_rems(12.5))
            .text_color(theme.text_muted)
            .child(crate::loaders::mini_mono_spinner(
                key.to_string(),
                1.5,
                theme.text_muted,
                cx.entity_id(),
                cx,
            ))
            .child(SharedString::from(label.to_string()))
            .into_any_element()
    }

    /// A ghost action that goes inert while another change runs.
    fn action(
        &self,
        theme: &Theme,
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let idle = self.pending.is_none() && self.login.is_none();
        widgets::ghost_action(theme)
            .id(id)
            .flex_none()
            .when(!idle, |el| el.opacity(0.4).cursor_default())
            .when(idle, |el| {
                el.hover(|s| widgets::ghost_hover(theme, s))
                    .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            })
            .child(SharedString::from(label))
            .into_any_element()
    }

    /// The page-header device switcher (the Agents/Accounts pattern).
    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (mut devices, local_id) = {
            let s = self.state.read(cx);
            (s.devices.clone(), s.local_device_id.clone())
        };
        devices.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let effective = self.target_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|d| Some(d.id.as_str()) == effective.as_deref())
            .cloned();
        let platform_glyph = |platform: &str| match platform {
            "macos" | "darwin" => icons::LAPTOP,
            "ios" | "android" => icons::SMARTPHONE,
            _ => icons::MONITOR,
        };
        let trigger_label: SharedString = selected
            .as_ref()
            .map(|d| d.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let trigger_glyph = platform_glyph(
            selected
                .as_ref()
                .map(|d| d.platform.as_str())
                .unwrap_or("macos"),
        );
        let emerald = theme.success;
        let open = self.device_menu_open;
        let mut trigger =
            div()
                .id("wizard-providers-device-switcher")
                .flex_none()
                .h(px(28.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .bg(if open {
                    crate::theme::ink(0.06)
                } else {
                    gpui::transparent_black()
                })
                .when(!open, |el| el.hover(|s| s.bg(crate::theme::ink(0.04))))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.device_menu_pressed_open = this.device_menu_open;
                    }),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    let pressed_open = std::mem::take(&mut this.device_menu_pressed_open);
                    this.device_menu_open = !pressed_open && !this.device_menu_open;
                    cx.notify();
                }))
                .child(
                    icon(trigger_glyph)
                        .size(px(16.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(trigger_label),
                )
                .child(div().size(px(6.0)).rounded_full().flex_none().bg(
                    if effective == local_id {
                        emerald
                    } else {
                        crate::theme::ink(0.2)
                    },
                ))
                .child(
                    icon(icons::SORT_VERTICAL)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.4 })),
                );
        if open {
            let theme = &theme.for_popup();
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.device_menu_open = false;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Devices"))
                .children(devices.into_iter().enumerate().map(|(ix, d)| {
                    let is_active = Some(d.id.as_str()) == effective.as_deref();
                    let is_local = local_id.as_deref() == Some(d.id.as_str());
                    let pick_id = d.id.clone();
                    popover::menu_row(theme, is_active, format!("wizard-providers-device-{ix}"))
                        .id(("wizard-providers-device-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let target = (!is_local).then(|| pick_id.clone());
                            this.set_target_device(target, cx);
                        }))
                        .child(
                            icon(platform_glyph(&d.platform))
                                .size(px(16.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(d.name.clone())),
                        )
                        .when(is_local, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(10.5))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from("You")),
                            )
                        })
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu(
                "wizard-providers-device-menu",
                menu,
                None,
            ));
        }
        trigger.into_any_element()
    }

    fn render_providers(
        &self,
        list: &WizardProviders,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let removable = list.providers.len() > 1;
        let rows: Vec<AnyElement> = list
            .providers
            .iter()
            .enumerate()
            .map(|(ix, provider)| self.render_provider_row(ix, provider, removable, theme, cx))
            .collect();
        let card = if rows.is_empty() {
            widgets::section_card(theme)
                .p(px(16.0))
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.text_muted)
                .child(SharedString::from(
                    "Wizard has no providers on this device yet. Add one below.",
                ))
        } else {
            widgets::section_card(theme).children(rows)
        };
        div()
            .child(
                div()
                    .mt(px(24.0))
                    .child(widgets::field_label(theme, "Providers")),
            )
            .child(div().mt(px(10.0)).child(card))
            .into_any_element()
    }

    fn render_provider_row(
        &self,
        ix: usize,
        provider: &ProviderRow,
        removable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let glyph = match provider.kind.as_str() {
            "xaioauth" | "chatgptoauth" => icons::GLOBAL,
            _ if provider.local => icons::MONITOR,
            _ => icons::KEY_MINIMALISTIC,
        };
        let mut meta = vec![
            div()
                .child(SharedString::from(kind_label(&provider.kind)))
                .into_any_element(),
            div()
                .child(SharedString::from(provider.model.clone()))
                .into_any_element(),
        ];
        if !provider.signed_in {
            meta.push(
                div()
                    .text_color(theme.warning_muted.opacity(0.9))
                    .child(SharedString::from(match provider.kind.as_str() {
                        "xaioauth" | "chatgptoauth" => "Signed out — sign in again below",
                        _ => "No API key",
                    }))
                    .into_any_element(),
            );
        }
        let busy = matches!(&self.pending, Some(Pending::Remove(n) | Pending::Activate(n)) if *n == provider.name);
        let name = provider.name.clone();
        let remove_name = provider.name.clone();
        widgets::card_row(theme, ix == 0)
            .child(widgets::row_tile(theme, glyph))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, provider.name.clone()))
                    .child(widgets::meta_line(theme, meta)),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .map(|el| {
                        if busy {
                            el.child(self.spinner_line(
                                &format!("wizard-provider-busy-{ix}"),
                                "Saving…",
                                theme,
                                cx,
                            ))
                        } else if provider.active {
                            el.child(widgets::badge_active(theme, "Active"))
                        } else {
                            el.child(self.action(
                                theme,
                                ("wizard-provider-activate", ix),
                                "Make active",
                                move |this, cx| {
                                    this.run(
                                        Pending::Activate(name.clone()),
                                        serde_json::json!({ "name": name }),
                                        cx,
                                    )
                                },
                                cx,
                            ))
                        }
                    })
                    .when(removable && !busy, |el| {
                        el.child(self.action(
                            theme,
                            ("wizard-provider-remove", ix),
                            "Remove",
                            move |this, cx| {
                                this.run(
                                    Pending::Remove(remove_name.clone()),
                                    serde_json::json!({ "name": remove_name }),
                                    cx,
                                )
                            },
                            cx,
                        ))
                    }),
            )
            .into_any_element()
    }

    /// Settings → Wizard → Usage: a card per signed-in subscription, meters
    /// XOR a quiet note, and Refresh beside the heading.
    fn render_usage(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let loading = matches!(self.usage, Loadable::Idle | Loadable::Loading);
        let refresh = if loading {
            self.spinner_line("wizard-usage-loading", "Checking…", theme, cx)
        } else {
            widgets::ghost_action(theme)
                .id("wizard-usage-refresh")
                .flex_none()
                .hover(|s| widgets::ghost_hover(theme, s))
                .on_click(cx.listener(|page, _, _, cx| page.load_usage(cx)))
                .child(SharedString::from("Refresh"))
                .into_any_element()
        };
        let quiet = |text: String| {
            widgets::section_card(theme)
                .p(px(16.0))
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.text_muted)
                .child(SharedString::from(text))
                .into_any_element()
        };
        let body = match &self.usage {
            Loadable::Idle | Loadable::Loading => widgets::section_card(theme)
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "wizard-usage-skeleton",
                    theme,
                    2,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => {
                widgets::error_strip(theme, message.clone()).into_any_element()
            }
            Loadable::Ready(usage) if usage.subscriptions.is_empty() => quiet(
                "Wizard isn't signed in to a subscription on this device. Sign in to xAI or \
                 ChatGPT below to see its plan limits here."
                    .into(),
            ),
            Loadable::Ready(usage) => {
                let now = chrono::Utc::now();
                let rows: Vec<AnyElement> = usage
                    .subscriptions
                    .iter()
                    .enumerate()
                    .map(|(ix, subscription)| {
                        self.render_subscription(ix, subscription, theme, now)
                    })
                    .collect();
                widgets::section_card(theme)
                    .children(rows)
                    .into_any_element()
            }
        };
        div()
            .child(
                div()
                    .mt(px(24.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(widgets::field_label(theme, "Usage"))
                    .child(refresh),
            )
            .child(div().mt(px(10.0)).child(body))
            .into_any_element()
    }

    fn render_subscription(
        &self,
        ix: usize,
        subscription: &SubscriptionUsage,
        theme: &Theme,
        now: chrono::DateTime<chrono::Utc>,
    ) -> AnyElement {
        let title = match &subscription.plan {
            Some(plan) => format!("{} · {plan}", subscription.name),
            None => subscription.name.clone(),
        };
        let small = |text: String, opacity: f32| {
            div()
                .mt(px(6.0))
                .truncate()
                .text_size(crate::typography::ui_rems(11.5))
                .text_color(theme.text_muted.opacity(opacity))
                .child(SharedString::from(text))
        };
        widgets::card_row(theme, ix == 0)
            .child(widgets::row_tile(theme, icons::GLOBAL))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, title))
                    .when(!subscription.windows.is_empty(), |el| {
                        el.child(div().mt(px(6.0)).flex().flex_col().gap(px(4.0)).children(
                            subscription.windows.iter().map(|window| {
                                crate::settings::accounts::render_usage_meter(window, theme)
                            }),
                        ))
                        .child(small(
                            crate::settings::accounts::usage_resets(&subscription.windows, now),
                            0.45,
                        ))
                    })
                    .when_some(subscription.products.clone(), |el, products| {
                        el.child(small(products, 0.6))
                    })
                    .when_some(subscription.note.clone(), |el, note| {
                        el.child(small(sentence(&note), 0.6))
                    }),
            )
            .into_any_element()
    }

    fn render_importable(
        &self,
        list: &WizardProviders,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if list.importable.is_empty() {
            return None;
        }
        let rows: Vec<AnyElement> = list
            .importable
            .iter()
            .enumerate()
            .map(|(ix, found)| {
                let source = found.source;
                let busy = self.pending == Some(Pending::Import(source));
                let mut meta = Vec::new();
                if let Some(detail) = found.detail.clone() {
                    meta.push(div().child(SharedString::from(detail)).into_any_element());
                }
                widgets::card_row(theme, ix == 0)
                    .child(widgets::row_tile(theme, icons::KEY_MINIMALISTIC))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, found.title.clone()))
                            .child(widgets::meta_line(theme, meta)),
                    )
                    .child(if busy {
                        self.spinner_line(&format!("wizard-import-{ix}"), "Adding…", theme, cx)
                    } else {
                        self.action(
                            theme,
                            ("wizard-import", ix),
                            "Use",
                            move |this, cx| {
                                this.run(
                                    Pending::Import(source),
                                    serde_json::json!({ "source": source }),
                                    cx,
                                )
                            },
                            cx,
                        )
                    })
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .child(
                    div()
                        .mt(px(24.0))
                        .child(widgets::field_label(theme, "Sign-ins on this device")),
                )
                .child(
                    div()
                        .mt(px(10.0))
                        .child(widgets::section_card(theme).children(rows)),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .text_size(crate::typography::ui_rems(widgets::ROW_DESCRIPTION_SIZE))
                        .text_color(theme.text_muted.opacity(0.65))
                        .child(SharedString::from(
                            "Wizard and that CLI then share one sign-in; if either is signed out \
                             later, sign in again.",
                        )),
                )
                .into_any_element(),
        )
    }

    fn render_add(
        &self,
        list: &WizardProviders,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chips = list
            .presets
            .iter()
            .map(|preset| {
                let selected = self.preset.as_deref() == Some(preset.id.as_str());
                let id = preset.id.clone();
                div()
                    .id(SharedString::from(format!("wizard-preset-{}", preset.id)))
                    .px(px(10.0))
                    .py(px(5.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if selected {
                        theme.text.opacity(0.5)
                    } else {
                        theme.border
                    })
                    .when(selected, |el| el.bg(crate::theme::ink(0.06)))
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.04)).text_color(theme.text))
                    .on_click(cx.listener(move |this, _, _, cx| this.choose_preset(id.clone(), cx)))
                    .child(SharedString::from(preset.label.clone()))
            })
            .collect::<Vec<_>>();
        let form = self
            .selected_preset()
            .map(|preset| self.render_form(&preset, theme, cx));
        div()
            .child(
                div()
                    .mt(px(28.0))
                    .child(widgets::field_label(theme, "Add a provider")),
            )
            .child(
                div()
                    .mt(px(10.0))
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(6.0))
                    .children(chips),
            )
            .children(form)
            .into_any_element()
    }

    fn render_form(
        &self,
        preset: &ProviderPreset,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let field = |label: &'static str, input: &Entity<ComposerInput>| {
            div()
                .mt(px(12.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(label)),
                )
                .child(popover::dialog_field(input.clone().into_any_element()))
        };
        let mut body = div().p(px(16.0)).flex().flex_col().child(
            div()
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.text_muted)
                .child(SharedString::from(preset.description.clone())),
        );
        if let Some(account) = preset.account.as_deref() {
            let account = if account == "chatgpt" {
                Account::ChatGpt
            } else {
                Account::Xai
            };
            return widgets::section_card(theme)
                .mt(px(12.0))
                .child(body.child(self.render_login(account, theme, cx)))
                .into_any_element();
        }
        if preset.custom {
            body = body.child(field("Name", &self.name));
        }
        if preset.custom || preset.local {
            body = body.child(field("Base URL", &self.base_url));
        }
        if preset.needs_account_id {
            body = body.child(field("Account id", &self.account_id));
        }
        body = body.child(field("Model", &self.model));
        if preset.needs_key {
            let key_line: AnyElement = match &self.api_key {
                Some(key) => div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(icon(icons::CHECK).size(px(14.0)).text_color(theme.success))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text)
                            .child(SharedString::from(format!("Key ready ••••{}", tail(key)))),
                    )
                    .child(self.action(
                        theme,
                        "wizard-key-clear",
                        "Clear",
                        |this, cx| {
                            this.api_key = None;
                            cx.notify();
                        },
                        cx,
                    ))
                    .into_any_element(),
                None => self.action(
                    theme,
                    "wizard-key-paste",
                    "Paste API key from clipboard",
                    |this, cx| this.paste_key(cx),
                    cx,
                ),
            };
            body = body.child(
                div()
                    .mt(px(12.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(if preset.key_optional {
                                "API key (optional)"
                            } else {
                                "API key"
                            })),
                    )
                    .child(key_line),
            );
        }
        let adding = self.pending == Some(Pending::Add(preset.id.clone()));
        body = body.child(
            div()
                .mt(px(16.0))
                .flex()
                .flex_row()
                .justify_end()
                .child(if adding {
                    self.spinner_line("wizard-add-spinner", "Adding…", theme, cx)
                } else {
                    popover::btn_primary(theme, "Add provider")
                        .id("wizard-add-provider")
                        .when(self.pending.is_some() || self.login.is_some(), |el| {
                            el.opacity(0.5)
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.add(cx)))
                        .into_any_element()
                }),
        );
        widgets::section_card(theme)
            .mt(px(12.0))
            .child(body)
            .into_any_element()
    }

    fn render_login(&self, account: Account, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let label = match account {
            Account::Xai => "Sign in with xAI",
            Account::ChatGpt => "Sign in with ChatGPT",
        };
        if !self.target_is_this_machine() {
            return div()
                .mt(px(12.0))
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.warning_muted.opacity(0.9))
                .child(SharedString::from(
                    "Account sign-in opens a browser on the machine that runs Wizard, so it only \
                     works for this computer. For another machine, share this device's sign-in \
                     from Settings → Devices, or add an API key.",
                ))
                .into_any_element();
        }
        match &self.login {
            Some(login) if login.account == account => {
                let url = login.url.clone();
                div()
                    .mt(px(12.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(self.spinner_line(
                        "wizard-login-spinner",
                        "Finish signing in in your browser…",
                        theme,
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(6.0))
                            .when_some(url, |el, url| {
                                el.child(
                                    widgets::ghost_action(theme)
                                        .id("wizard-login-open")
                                        .hover(|s| widgets::ghost_hover(theme, s))
                                        .on_click(move |_, _, cx| cx.open_url(&url))
                                        .child(SharedString::from("Open the sign-in page"))
                                        .child(
                                            icon(icons::ARROW_UP_RIGHT)
                                                .size(px(12.0))
                                                .text_color(theme.text_muted),
                                        ),
                                )
                            })
                            .child(
                                widgets::ghost_action(theme)
                                    .id("wizard-login-cancel")
                                    .hover(|s| widgets::ghost_hover(theme, s))
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx)))
                                    .child(SharedString::from("Cancel")),
                            ),
                    )
                    .into_any_element()
            }
            _ => div()
                .mt(px(14.0))
                .flex()
                .flex_row()
                .justify_end()
                .child(
                    popover::btn_primary(theme, label)
                        .id(SharedString::from(format!("wizard-login-{account:?}")))
                        .when(self.pending.is_some() || self.login.is_some(), |el| {
                            el.opacity(0.5)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.start_login(account, cx))),
                )
                .into_any_element(),
        }
    }
}

impl popover::ScrollRailHost for WizardProvidersPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

impl Render for WizardProvidersPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let count = self.providers.ready().map(|list| list.providers.len());
        let body: AnyElement = match &self.providers {
            Loadable::Idle | Loadable::Loading => widgets::section_card(&theme)
                .mt(px(24.0))
                .p(px(16.0))
                .child(popover::skeleton_rows(
                    "wizard-providers-skeleton",
                    &theme,
                    3,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => div()
                .mt(px(24.0))
                .child(widgets::error_strip(&theme, message.clone()))
                .child(
                    widgets::ghost_action(&theme)
                        .id("wizard-providers-retry")
                        .mt(px(8.0))
                        .hover(|s| widgets::ghost_hover(&theme, s))
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.load(cx);
                            cx.notify();
                        }))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            Loadable::Ready(list) => {
                let list = list.clone();
                div()
                    .when_some(list.config_error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(16.0))
                                .child(widgets::warning_strip(&theme, message)),
                        )
                    })
                    .child(self.render_providers(&list, &theme, cx))
                    .child(self.render_usage(&theme, cx))
                    .children(self.render_importable(&list, &theme, cx))
                    .child(self.render_add(&list, &theme, cx))
                    .into_any_element()
            }
        };
        let error = self.error.clone().map(|message| {
            div()
                .mt(px(16.0))
                .child(widgets::error_strip(&theme, message))
        });
        let notice = self.notice.clone().map(|message| {
            div()
                .mt(px(16.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(theme.success_muted.opacity(0.9))
                .child(
                    icon(icons::CHECK)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.success_muted),
                )
                .child(SharedString::from(message))
        });
        let switcher = self.render_device_switcher(&theme, cx);
        let scrollbar = popover::rail(self, "wizard-providers-page-scrollbar", &theme, cx);
        div()
            .id("wizard-providers-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                div()
                    .id("wizard-providers-page")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll.scroll)
                    .child(
                        widgets::page_column()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .child(widgets::page_header(&theme, "Wizard", count))
                                    .child(switcher),
                            )
                            .child(
                                widgets::page_subtitle(
                                    &theme,
                                    "The model providers Wizard runs on the selected device. \
                                     Every provider's models appear in the composer's Wizard \
                                     model picker; the active one is where new chats start.",
                                )
                                .max_w(px(512.0))
                                .line_height(px(20.0)),
                            )
                            .children(error)
                            .children(notice)
                            .child(body),
                    ),
            )
            .children(scrollbar)
    }
}

/// How the page names a provider kind.
fn kind_label(kind: &str) -> &str {
    match kind {
        "xaioauth" => "xAI account",
        "chatgptoauth" => "ChatGPT account",
        "xai" => "xAI API",
        "openai" => "OpenAI-compatible",
        "anthropic" => "Anthropic",
        "openrouter" => "OpenRouter",
        "cloudflare" => "Cloudflare Workers AI",
        "ollama" => "Ollama",
        "llamacpp" => "llama.cpp",
        other => other,
    }
}

/// Wizard's notes are lowercase clauses made for a chat line; a settings row
/// starts with a capital.
fn sentence(note: &str) -> String {
    let mut chars = note.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The last four characters of a key, for recognizing it.
fn tail(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_tail_shows_only_the_last_four() {
        assert_eq!(tail("sk-secret-abcd"), "abcd");
        assert_eq!(tail("ab"), "ab");
    }

    #[test]
    fn a_wizard_note_reads_as_a_sentence() {
        assert_eq!(
            sentence("no request yet this session; limits show after the first reply"),
            "No request yet this session; limits show after the first reply"
        );
        assert_eq!(sentence(""), "");
    }

    #[test]
    fn every_preset_kind_has_a_label() {
        for preset in zeron_engine::wizard_auth::providers::presets() {
            assert_ne!(kind_label(&preset.kind), "", "{}", preset.id);
        }
    }
}
