//! The settings sheet, and onboarding, which are one screen.
//!
//! Every decision this file could make has already been made in
//! [`crate::plugins::gui::settings`]: what a provider row says, where its key comes
//! from, what Remove does to the active provider, how long a probe gets, what a
//! step limit may be. This module is the *rendering* and the state machine of
//! the sheet, and nothing else. If a rule about providers appears below, it is
//! in the wrong file.
//!
//! # One sheet, two entrances
//!
//! `docs/gui-design-spec.md`: "Both are the same surface." Onboarding is what
//! opens when [`SettingsView::first_run`] is true — there is no chat to open,
//! because there is nothing to send a message to — and the only differences are
//! the title, the submit verb (`Connect` rather than `Save`) and that it cannot
//! be dismissed with Escape. Two screens would be two places to fix the same
//! bug, which is what the browser GUI's `settings.js` avoided too.
//!
//! # Why the sheet holds a snapshot
//!
//! [`ConfigStore::current`] re-reads the file on every call, which is correct
//! and is also a syscall. The sheet reads it when it opens and after every
//! write, and draws from that snapshot in between — so a redraw during a
//! keystroke does not stat the config, and a change made by another Wizard
//! lands the next time this one writes or reopens. That is the same freshness
//! the browser GUI had over HTTP, for the same reason.

use std::sync::Arc;

use iced::widget::{column, container, row, text, text_input};
use iced::{Border, Element, Length, Padding};

use crate::plugins::gui::oauth::{self, SignIn};
use crate::plugins::gui::settings::{
    ConfigStore, KeySource, NewProvider, Preset, ProviderProbe, SettingsView,
};
use crate::plugins::native::theme::Palette;
use crate::plugins::native::widget::chrome;
use crate::theme::Token;

/// What the "add a provider" half of the sheet is doing.
enum Adding {
    /// Resting: one quiet row that reads `+ Add provider`.
    No,
    /// The picker: the sign-in rows, then the presets.
    Picking,
    /// A form, for the preset at this index in [`SettingsView::presets`], or
    /// [`Form::CUSTOM`] for the synthesized OpenAI-compatible row.
    Filling(Box<Form>),
}

/// The provider form, shared by onboarding, Add and Edit.
///
/// One struct for all three because they differ only in which fields are shown
/// and what the submit button says — and the browser GUI proved that by using
/// one function for all three.
pub struct Form {
    /// The preset this was seeded from, or `None` when editing an existing
    /// provider (whose name is its identity and cannot move).
    preset: Option<usize>,
    /// The provider being edited, when this is an edit.
    editing: Option<String>,
    label: String,
    name: String,
    kind: String,
    base_url: String,
    model: String,
    api_key: String,
    /// The provider is local, so there is no key field at all.
    local: bool,
    /// Set while a save is in flight, so the button says `Checking…` and
    /// cannot be pressed twice.
    saving: bool,
    error: Option<String>,
}

impl Form {
    /// The index [`Adding::Filling`] carries for the synthesized `Custom` row,
    /// which is not in [`SettingsView::presets`] because it is not a preset —
    /// it is the absence of one.
    const CUSTOM: usize = usize::MAX;

    fn from_preset(index: usize, preset: &Preset) -> Self {
        Self {
            preset: Some(index),
            editing: None,
            label: preset.label.to_string(),
            name: preset.name.to_string(),
            kind: preset.kind.to_string(),
            base_url: preset.base_url.to_string(),
            model: preset.model.to_string(),
            api_key: String::new(),
            local: !preset.needs_key,
            saving: false,
            error: None,
        }
    }

    fn custom() -> Self {
        Self {
            preset: Some(Self::CUSTOM),
            editing: None,
            label: "Custom".to_string(),
            name: String::new(),
            kind: "openai".to_string(),
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            local: false,
            saving: false,
            error: None,
        }
    }

    fn edit(row: &crate::plugins::gui::settings::ProviderRow) -> Self {
        Self {
            preset: None,
            editing: Some(row.name.clone()),
            label: row.name.clone(),
            name: row.name.clone(),
            kind: row.kind.clone(),
            base_url: row.base_url.clone(),
            model: row.model.clone(),
            api_key: String::new(),
            local: matches!(row.key, KeySource::NotNeeded | KeySource::Oauth),
            saving: false,
            error: None,
        }
    }

    /// Whether the name is the user's to type. Only the `Custom` row: every
    /// other name is the credentials key a preset already owns, and an edit
    /// cannot move a name without orphaning the stored key under the old one.
    fn names_itself(&self) -> bool {
        self.preset == Some(Self::CUSTOM)
    }

    fn as_new_provider(&self) -> NewProvider {
        NewProvider {
            name: self.name.clone(),
            kind: self.kind.clone(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            // Blank keeps whatever is stored, which is what makes an edit that
            // only changes the model safe.
            api_key: match self.api_key.trim().is_empty() {
                true => None,
                false => Some(self.api_key.clone()),
            },
            activate: true,
        }
    }
}

/// What the sheet can be told.
#[derive(Debug, Clone)]
pub enum Message {
    /// The config was re-read, after a write or on open.
    Loaded(Box<SettingsView>),
    Use(String),
    Test(String),
    Tested(String, Box<ProviderProbe>),
    Edit(String),
    Remove(String),
    Add,
    Pick(usize),
    Cancel,
    Field(Field, String),
    /// Dismiss the whole sheet. Distinct from [`Message::Cancel`], which backs
    /// out of a form and leaves the sheet up: one ✕ that did both would throw
    /// away a half-typed provider whenever somebody meant to go back a step.
    Close,
    Submit,
    Submitted(Box<Result<(SettingsView, ProviderProbe), String>>),
    StepLimit(String),
    CommitStepLimit,
    SignIn(&'static str),
    SignInStarted(&'static str, Box<Result<String, String>>),
    /// The one-second poll while a sign-in is in the other window.
    SignInPolled(Box<oauth::Status>),
}

/// Which field of the provider form moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    BaseUrl,
    Model,
    ApiKey,
}

/// The sheet's state.
pub struct Sheet {
    store: Arc<ConfigStore>,
    sign_in: Arc<SignIn>,
    /// The config as of the last read. See the module header.
    pub view: SettingsView,
    adding: Adding,
    editing: Option<Form>,
    /// Per-provider feedback, from Test / Use / Remove.
    status: Vec<(String, String)>,
    /// The step-limit field's text, which is edited before it is parsed.
    steps: String,
    steps_note: Option<String>,
    /// What a sign-in is doing, for the note under the sign-in rows.
    sign_in_note: Option<String>,
    /// A sign-in is in flight, so the poll subscription runs.
    pub awaiting_sign_in: bool,
}

impl Sheet {
    pub fn new(store: Arc<ConfigStore>, sign_in: Arc<SignIn>) -> Self {
        let view = crate::plugins::gui::settings::view(&store.current());
        Self {
            store,
            sign_in,
            steps: view.max_steps.to_string(),
            view,
            adding: Adding::No,
            editing: None,
            status: Vec::new(),
            steps_note: None,
            sign_in_note: None,
            awaiting_sign_in: false,
        }
    }

    /// Whether this is the first run, so a surface opens onboarding rather than
    /// a chat.
    pub fn first_run(&self) -> bool {
        self.view.first_run
    }

    /// Open the sheet with the add-provider picker already expanded, which is
    /// where `/provider` and `/login` deep-link to.
    pub fn open_picker(&mut self) {
        self.adding = Adding::Picking;
        self.editing = None;
    }

    fn form(&mut self) -> Option<&mut Form> {
        match &mut self.adding {
            Adding::Filling(form) => Some(form),
            _ => self.editing.as_mut(),
        }
    }

    fn reload(&mut self, view: SettingsView) {
        self.steps = view.max_steps.to_string();
        self.view = view;
        self.adding = Adding::No;
        self.editing = None;
    }

    fn note(&mut self, provider: &str, text: String) {
        self.status.retain(|(name, _)| name != provider);
        self.status.push((provider.to_string(), text));
    }

    /// Apply one message; the returned task carries whatever has to happen off
    /// the draw thread (a probe, a save, opening a browser).
    pub fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::Loaded(view) => {
                self.reload(*view);
                iced::Task::none()
            }
            Message::Use(name) => {
                match crate::plugins::gui::settings::activate_provider(&self.store, &name) {
                    Ok(view) => self.reload(view),
                    Err(err) => self.note(&name, format!("{err:#}")),
                }
                iced::Task::none()
            }
            Message::Test(name) => {
                self.note(&name, "Testing…".to_string());
                let store = Arc::clone(&self.store);
                iced::Task::perform(
                    async move {
                        crate::plugins::gui::settings::test_provider(&store, &name)
                            .await
                            .map(|probe| (name, probe))
                    },
                    |done| match done {
                        Some((name, probe)) => Message::Tested(name, Box::new(probe)),
                        // The provider vanished between the click and the
                        // probe: another Wizard removed it. Re-read rather
                        // than reporting a failure of a provider that is gone.
                        None => Message::Cancel,
                    },
                )
            }
            Message::Tested(name, probe) => {
                let text = match (probe.ok, probe.error) {
                    (true, _) if probe.models.is_empty() => "Answered — no models".to_string(),
                    (true, _) => format!("Answered — {} models", probe.models.len()),
                    (false, Some(why)) => why,
                    (false, None) => "no answer".to_string(),
                };
                self.note(&name, text);
                iced::Task::none()
            }
            Message::Edit(name) => {
                if let Some(row) = self.view.providers.iter().find(|row| row.name == name) {
                    self.editing = Some(Form::edit(row));
                    self.adding = Adding::No;
                }
                iced::Task::none()
            }
            Message::Remove(name) => {
                match crate::plugins::gui::settings::forget_provider(&self.store, &name) {
                    Ok(view) => self.reload(view),
                    Err(err) => self.note(&name, format!("{err:#}")),
                }
                iced::Task::none()
            }
            Message::Add => {
                self.open_picker();
                iced::Task::none()
            }
            Message::Pick(index) => {
                self.adding = Adding::Filling(Box::new(match self.view.presets.get(index) {
                    Some(preset) => Form::from_preset(index, preset),
                    None => Form::custom(),
                }));
                iced::Task::none()
            }
            // Nothing to do here: the app closes the screen. Listed rather
            // than folded into `Cancel` so that `update`'s decision is made on
            // the message and not on a guess about which one the ✕ sent.
            Message::Close => iced::Task::none(),
            Message::Cancel => {
                self.adding = Adding::No;
                self.editing = None;
                self.sign_in_note = None;
                let store = Arc::clone(&self.store);
                iced::Task::perform(
                    async move { crate::plugins::gui::settings::view(&store.current()) },
                    |view| Message::Loaded(Box::new(view)),
                )
            }
            Message::Field(field, value) => {
                if let Some(form) = self.form() {
                    match field {
                        Field::Name => form.name = value,
                        Field::BaseUrl => form.base_url = value,
                        Field::Model => form.model = value,
                        Field::ApiKey => form.api_key = value,
                    }
                }
                iced::Task::none()
            }
            Message::Submit => {
                let Some(form) = self.form() else {
                    return iced::Task::none();
                };
                if form.saving {
                    return iced::Task::none();
                }
                form.saving = true;
                form.error = None;
                let request = form.as_new_provider();
                let store = Arc::clone(&self.store);
                iced::Task::perform(
                    async move {
                        crate::plugins::gui::settings::save_provider(&store, request)
                            .await
                            .map_err(|err| err.to_string())
                    },
                    |done| Message::Submitted(Box::new(done)),
                )
            }
            Message::Submitted(done) => {
                match *done {
                    Ok((view, probe)) => {
                        let name = view.active.clone().unwrap_or_default();
                        self.reload(view);
                        // Saved either way — a typo'd key leaves an editable
                        // row rather than vanishing — so the probe's verdict is
                        // a note on the row, not a reason to keep the form up.
                        if let Some(why) = probe.error {
                            self.note(&name, format!("Saved, but it did not answer: {why}"));
                        }
                    }
                    Err(why) => {
                        if let Some(form) = self.form() {
                            form.saving = false;
                            form.error = Some(why);
                        }
                    }
                }
                iced::Task::none()
            }
            Message::StepLimit(value) => {
                self.steps = value;
                iced::Task::none()
            }
            Message::CommitStepLimit => {
                // An unparseable or unchanged value reverts the field and says
                // nothing, exactly as the browser's blur handler did: this is a
                // number box, not a form, and there is no Save to press.
                let Ok(steps) = self.steps.trim().parse::<u32>() else {
                    self.steps = self.view.max_steps.to_string();
                    return iced::Task::none();
                };
                if steps == self.view.max_steps {
                    return iced::Task::none();
                }
                match crate::plugins::gui::settings::set_step_limit(&self.store, steps) {
                    Ok(view) => {
                        self.steps_note = Some(match steps {
                            0 => "Saved — no limit".to_string(),
                            _ => "Saved".to_string(),
                        });
                        self.reload(view);
                    }
                    Err(why) => {
                        self.steps = self.view.max_steps.to_string();
                        self.steps_note = Some(why.to_string());
                    }
                }
                iced::Task::none()
            }
            Message::SignIn(provider) => {
                self.sign_in_note = Some(format!("Opening {provider} in your browser…"));
                self.awaiting_sign_in = true;
                let sign_in = Arc::clone(&self.sign_in);
                let store = Arc::clone(&self.store);
                iced::Task::perform(
                    async move {
                        sign_in
                            .begin(provider, store)
                            .await
                            .map_err(|err| format!("{err:#}"))
                    },
                    move |done| Message::SignInStarted(provider, Box::new(done)),
                )
            }
            Message::SignInStarted(provider, done) => match *done {
                Ok(url) => {
                    self.sign_in_note =
                        Some(format!("Waiting for {provider} in the other window…"));
                    // Best-effort: on a box with no browser the URL is still
                    // the thing to act on, so it goes in the note rather than
                    // the failure being the whole message.
                    crate::plugins::gui::open_browser(&url);
                    // The URL is in the note too, because a box with no
                    // `xdg-open` still has a person who can paste it.
                    self.sign_in_note = Some(format!(
                        "Waiting for {provider} in the other window — {url}"
                    ));
                    iced::Task::none()
                }
                Err(why) => {
                    self.awaiting_sign_in = false;
                    self.sign_in_note = Some(why);
                    iced::Task::none()
                }
            },
            Message::SignInPolled(status) => match *status {
                oauth::Status::Pending { .. } => iced::Task::none(),
                oauth::Status::Done { provider } => {
                    self.awaiting_sign_in = false;
                    self.sign_in_note = Some(format!("Signed in — {provider} is configured."));
                    let store = Arc::clone(&self.store);
                    iced::Task::perform(
                        async move { crate::plugins::gui::settings::view(&store.current()) },
                        |view| Message::Loaded(Box::new(view)),
                    )
                }
                oauth::Status::Failed { error, .. } => {
                    self.awaiting_sign_in = false;
                    self.sign_in_note = Some(error);
                    iced::Task::none()
                }
                oauth::Status::Idle => {
                    self.awaiting_sign_in = false;
                    self.sign_in_note = Some("the sign-in was not completed".to_string());
                    iced::Task::none()
                }
            },
        }
    }

    /// One poll of the sign-in in flight, for the app's timer subscription.
    pub fn poll_sign_in(&self) -> Message {
        Message::SignInPolled(Box::new(self.sign_in.status()))
    }

    /// The sheet.
    pub fn view(&self, palette: &Palette) -> Element<'_, Message> {
        let onboarding = self.view.first_run;
        let mut body = column![].spacing(10).width(Length::Fill);

        if let Some(form) = &self.editing {
            body = body.push(chrome::body(form.label.clone(), palette));
            body = body.push(self.form_view(form, "Save", palette));
        } else {
            let mut rows: Vec<Element<'_, Message>> = Vec::new();
            if self.view.providers.is_empty() {
                rows.push(
                    chrome::muted(
                        "None configured — wizard cannot answer until one is.",
                        palette,
                    )
                    .into(),
                );
            }
            for provider in &self.view.providers {
                rows.push(self.provider_row(provider, palette));
            }
            rows.push(chrome::separator(palette));
            rows.push(self.adder(palette));
            body = body.push(chrome::block("providers", rows, palette));

            if !onboarding {
                body = body.push(chrome::hairline(palette));
                body = body.push(chrome::block(
                    "agent",
                    vec![self.step_limit(palette)],
                    palette,
                ));
            }
        }

        let title = match onboarding {
            true => "Set up wizard",
            false => "Settings",
        };
        let head = chrome::spread(
            text(title).size(15.0).color(palette.color(Token::Text)),
            chrome::action("close", Message::Close, palette),
        );
        let foot = chrome::literal(self.view.config_path.clone(), palette);

        let sheet = column![
            head,
            chrome::hairline(palette),
            chrome::scroll(container(body).padding(Padding::new(4.0).top(10.0).bottom(10.0)))
                .height(Length::Fill),
            chrome::hairline(palette),
            foot,
        ]
        .spacing(10);

        container(
            container(sheet)
                .width(Length::Fixed(if onboarding { 460.0 } else { 560.0 }))
                .max_height(760.0)
                // A card with a fixed width and a rounded border, drawn over a
                // dimmed window: anything that does overflow it lands on the
                // dimmed background outside the card, where it reads as a
                // rendering fault rather than as truncation. The provider rows
                // inside are bounded individually; this is the backstop for
                // whatever gets added to the sheet next.
                .clip(true)
                .padding(Padding::new(18.0))
                .style({
                    let surface = palette.surface;
                    let hairline = palette.hairline;
                    move |_theme| container::Style {
                        background: Some(iced::Background::Color(surface)),
                        border: Border {
                            color: hairline,
                            width: 1.0,
                            radius: 14.0.into(),
                        },
                        ..container::Style::default()
                    }
                }),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .padding(Padding::new(24.0))
        .into()
    }

    /// One configured provider: what it is, where its key comes from, and the
    /// four quiet actions on the right.
    fn provider_row<'a>(
        &'a self,
        provider: &'a crate::plugins::gui::settings::ProviderRow,
        palette: &Palette,
    ) -> Element<'a, Message> {
        let key = match provider.key {
            KeySource::Stored => ("key stored", Token::Muted),
            KeySource::Env => ("key from env", Token::Muted),
            KeySource::Oauth => ("signed in", Token::Muted),
            KeySource::NotNeeded => ("local", Token::Muted),
            // The one coloured word in the list, because it is the one that
            // answers "why is it 401ing".
            KeySource::Missing => ("no key", Token::Warning),
        };
        let mut left = column![
            row![
                chrome::body(provider.name.clone(), palette),
                text(match provider.active {
                    true => "ACTIVE",
                    false => "",
                })
                .size(chrome::LABEL)
                .color(palette.color(Token::Faint)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            // One line, truncated by the clip above rather than reflowed. A
            // model id is a single unbroken token with slashes in it, so
            // wrapping puts two characters of `no key` on a line of their own
            // and calls that a layout.
            row![
                chrome::literal(
                    format!("{} · {} · ", provider.kind, provider.model),
                    palette
                )
                .wrapping(iced::widget::text::Wrapping::None),
                text(key.0)
                    .size(chrome::LITERAL)
                    .font(crate::plugins::native::font::MONO)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .color(palette.color(key.1)),
            ],
        ]
        .spacing(2);
        if let Some((_, note)) = self.status.iter().find(|(name, _)| *name == provider.name) {
            left = left.push(chrome::muted(note.clone(), palette));
        }

        let mut actions = row![].spacing(2);
        if !provider.active {
            actions = actions.push(chrome::action(
                "use",
                Message::Use(provider.name.clone()),
                palette,
            ));
        }
        actions = actions
            .push(chrome::action(
                "test",
                Message::Test(provider.name.clone()),
                palette,
            ))
            .push(chrome::action(
                "edit",
                Message::Edit(provider.name.clone()),
                palette,
            ))
            .push(chrome::danger(
                "remove",
                Message::Remove(provider.name.clone()),
                palette,
            ));

        // The active row is marked by a rule down its left edge, not by a
        // background: "emphasis is brightness, not colour", and a filled row
        // beside four unfilled ones reads as selected rather than as active.
        let accent = palette.color(Token::Accent);
        let marker = container(iced::widget::space().width(2))
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: provider.active.then_some(iced::Background::Color(accent)),
                ..container::Style::default()
            });
        // NOT `spread(left, actions)`. `spread` puts a `Fill` space between
        // its halves and leaves the row `Shrink`, so the description — which
        // carries the model id — grew the row until the buttons were laid out
        // past the sheet. `accounts/fireworks/models/gpt-oss-120b` is a real
        // preset id, and with it `remove` renders as `rem`.
        //
        // Truncating a destructive control is the worst way to run out of
        // room: a half-drawn `remove` is still clickable, and the thing it
        // removes is a configured provider. The description is the elastic
        // part instead, and the buttons keep their intrinsic width.
        container(
            row![
                marker,
                container(left).width(Length::Fill).clip(true),
                actions
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .padding(Padding::new(4.0))
        .into()
    }

    /// The three states of the add-provider slot, in the same place.
    fn adder(&self, palette: &Palette) -> Element<'_, Message> {
        match &self.adding {
            Adding::No => chrome::pick(
                chrome::muted("+  Add provider", palette),
                Message::Add,
                false,
                palette,
            ),
            Adding::Filling(form) => column![
                chrome::body(format!("Add {}", form.label), palette),
                self.form_view(
                    form,
                    if self.view.first_run {
                        "Connect"
                    } else {
                        "Save"
                    },
                    palette
                ),
            ]
            .spacing(8)
            .into(),
            Adding::Picking => {
                let mut rows = column![].spacing(2).width(Length::Fill);
                for (provider, label, meta) in oauth::SUPPORTED {
                    rows = rows.push(chrome::pick(
                        chrome::spread(
                            chrome::body(*label, palette),
                            chrome::muted(*meta, palette),
                        ),
                        Message::SignIn(provider),
                        false,
                        palette,
                    ));
                }
                if let Some(note) = &self.sign_in_note {
                    rows = rows.push(chrome::muted(note.clone(), palette));
                }
                rows = rows.push(chrome::label("or use an API key", palette));
                for (index, preset) in self.view.presets.iter().enumerate() {
                    rows = rows.push(chrome::pick(
                        chrome::spread(
                            chrome::body(preset.label, palette),
                            chrome::literal(host_of(preset.base_url), palette),
                        ),
                        Message::Pick(index),
                        false,
                        palette,
                    ));
                }
                rows = rows.push(chrome::pick(
                    chrome::spread(
                        chrome::body("Custom", palette),
                        chrome::literal("OpenAI-compatible", palette),
                    ),
                    Message::Pick(usize::MAX),
                    false,
                    palette,
                ));
                rows = rows.push(chrome::action("cancel", Message::Cancel, palette));
                rows.into()
            }
        }
    }

    /// The provider form. Which fields appear is the preset's decision, not
    /// this function's opinion.
    fn form_view<'a>(
        &'a self,
        form: &'a Form,
        submit: &'a str,
        palette: &Palette,
    ) -> Element<'a, Message> {
        let mut fields = column![].spacing(10).width(Length::Fill);
        if form.names_itself() {
            fields = fields.push(self.field("name", &form.name, Field::Name, "a name", palette));
        }
        let needs_base_url = form.names_itself()
            || form.editing.is_some()
            || form
                .preset
                .and_then(|index| self.view.presets.get(index))
                .is_some_and(|preset| preset.needs_base_url);
        if needs_base_url {
            fields = fields.push(self.field(
                "base url",
                &form.base_url,
                Field::BaseUrl,
                "https://…",
                palette,
            ));
        }
        fields = fields.push(self.field("model", &form.model, Field::Model, "model tag", palette));
        if !form.local {
            fields = fields.push(self.field(
                "api key",
                &form.api_key,
                Field::ApiKey,
                match form.editing {
                    Some(_) => "unchanged",
                    None => "sk-…",
                },
                palette,
            ));
            if let Some(path) = &self.view.credentials_path {
                fields = fields.push(chrome::literal(format!("stored in {path}"), palette));
            }
        }
        if let Some(error) = &form.error {
            fields = fields.push(
                text(error.clone())
                    .size(chrome::SMALL)
                    .color(palette.color(Token::Error)),
            );
        }
        fields = fields.push(
            row![
                chrome::primary(
                    if form.saving { "Checking…" } else { submit },
                    (!form.saving).then_some(Message::Submit),
                    palette
                ),
                chrome::action("cancel", Message::Cancel, palette),
            ]
            .spacing(8)
            // The two buttons have different heights — `primary` is padded
            // heavier than `action` — and a `row!` tops them out by default,
            // so "cancel" sat a few pixels above "Save"'s baseline.
            .align_y(iced::Alignment::Center),
        );
        fields.into()
    }

    fn field<'a>(
        &self,
        label: &'a str,
        value: &'a str,
        field: Field,
        placeholder: &'a str,
        palette: &Palette,
    ) -> Element<'a, Message> {
        let input = text_input(placeholder, value)
            .on_input(move |text| Message::Field(field, text))
            .on_submit(Message::Submit)
            .size(chrome::UI)
            .font(crate::plugins::native::font::MONO)
            .padding(Padding::new(7.0))
            .style(input_style(palette));
        let input = match field {
            // A key is a secret being typed in front of whoever is behind you.
            Field::ApiKey => input.secure(true),
            _ => input,
        };
        column![chrome::label(label, palette), input]
            .spacing(5)
            .into()
    }

    /// The step limit, which is the only setting that is not a provider.
    fn step_limit(&self, palette: &Palette) -> Element<'_, Message> {
        let input = text_input("0", &self.steps)
            .on_input(Message::StepLimit)
            // Enter and blur both commit; there is no Save button, because a
            // Save button for one number is a form.
            .on_submit(Message::CommitStepLimit)
            .size(chrome::UI)
            .font(crate::plugins::native::font::MONO)
            .align_x(iced::alignment::Horizontal::Right)
            .width(Length::Fixed(84.0))
            .padding(Padding::new(6.0))
            .style(input_style(palette));
        let mut right = row![].spacing(8).align_y(iced::Alignment::Center);
        if let Some(note) = &self.steps_note {
            right = right.push(chrome::muted(note.clone(), palette));
        }
        right = right
            .push(input)
            .push(chrome::action("apply", Message::CommitStepLimit, palette));
        // Not `chrome::spread`, for the reason its own doc gives: the left
        // side here is a sentence with no bound on its width, so it took its
        // intrinsic size first and pushed the input and `apply` off the sheet
        // — at a 480 px window there was a step-limit field with no way to
        // commit it. The explanatory line is the half that can be truncated.
        row![
            container(
                column![
                    chrome::body("Step limit", palette),
                    chrome::muted(
                        "Tool calls one chat may make per turn. 0 is no limit.",
                        palette,
                    ),
                ]
                .spacing(2)
            )
            .width(Length::Fill)
            .clip(true),
            right,
        ]
        .align_y(iced::Alignment::Center)
        .spacing(8)
        .into()
    }
}

/// The one text-input style in the sheet.
fn input_style(
    palette: &Palette,
) -> impl Fn(&iced::Theme, text_input::Status) -> text_input::Style + use<> {
    let canvas = palette.canvas;
    let hairline = palette.hairline;
    let focus = palette.color(Token::Accent);
    let text = palette.color(Token::Text);
    let faint = palette.color(Token::Faint);
    let selection = palette.selection;
    move |_theme, status| text_input::Style {
        background: iced::Background::Color(canvas),
        border: Border {
            color: match status {
                text_input::Status::Focused { .. } => focus,
                _ => hairline,
            },
            width: 1.0,
            radius: 6.0.into(),
        },
        icon: faint,
        placeholder: faint,
        value: text,
        selection,
    }
}

/// The host of a base URL, for the preset list's right column.
///
/// A preset row says a name and where it points, and nothing else. Cloudflare's
/// base URL carries an account-id path template that would make the row three
/// times as wide and say nothing more.
fn host_of(base_url: &str) -> String {
    let without_scheme = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::from_theme(&crate::theme::minimal())
    }

    fn sheet() -> Sheet {
        let config = crate::config::Config::default();
        Sheet::new(
            Arc::new(ConfigStore::new(config)),
            Arc::new(SignIn::default()),
        )
    }

    /// A preset row says a name and a host. Cloudflare's account-id template is
    /// the case that proves it: the full base URL would be most of the row and
    /// would say nothing the name does not.
    #[test]
    fn a_preset_row_shows_the_host_and_not_the_path_template() {
        assert_eq!(host_of("https://api.openai.com/v1"), "api.openai.com");
        assert_eq!(
            host_of("https://api.cloudflare.com/client/v4/accounts/<id>/ai/v1"),
            "api.cloudflare.com"
        );
        assert_eq!(host_of("http://localhost:11434/v1"), "localhost:11434");
    }

    /// An edit that leaves the key blank must send no key at all, or the
    /// stored one is overwritten with an empty string and the provider starts
    /// 401ing after a change to its model.
    #[test]
    fn a_blank_key_field_sends_no_key() {
        let mut form = Form::custom();
        form.name = "mine".to_string();
        form.base_url = "https://x/v1".to_string();
        form.model = "m".to_string();
        assert!(form.as_new_provider().api_key.is_none());
        form.api_key = "sk-abc".to_string();
        assert_eq!(form.as_new_provider().api_key.as_deref(), Some("sk-abc"));
    }

    /// Only the Custom row lets the name be typed. Every other name is the key
    /// under which the credential is stored, and an edit that moved it would
    /// orphan the secret under the old one.
    #[test]
    fn only_a_custom_provider_names_itself() {
        assert!(Form::custom().names_itself());
        let presets = crate::plugins::gui::settings::presets();
        assert!(!Form::from_preset(0, &presets[0]).names_itself());
    }

    /// A step limit that is not a number reverts rather than saving something
    /// nobody typed, and one over the ceiling is refused with the reason.
    #[test]
    fn a_bad_step_limit_reverts_and_a_too_large_one_says_why() {
        let mut sheet = sheet();
        sheet.steps = "not a number".to_string();
        let _ = sheet.update(Message::CommitStepLimit);
        assert_eq!(sheet.steps, sheet.view.max_steps.to_string());
        assert_eq!(sheet.steps_note, None, "a typo is not worth a message");

        sheet.steps = "99999".to_string();
        let _ = sheet.update(Message::CommitStepLimit);
        assert!(
            sheet
                .steps_note
                .as_deref()
                .is_some_and(|note| note.contains("at most")),
            "{:?}",
            sheet.steps_note
        );
    }

    /// A probe that answered with nothing is not the same as a probe that
    /// failed, and the row has to say which — "Answered — no models" is a
    /// working provider with an empty catalogue.
    #[test]
    fn a_probe_reports_answered_failed_and_empty_differently() {
        let mut sheet = sheet();
        let probe = |ok, error: Option<&str>, models: Vec<&str>| {
            Box::new(ProviderProbe {
                ok,
                error: error.map(str::to_string),
                models: models.into_iter().map(str::to_string).collect(),
            })
        };
        let note = |sheet: &Sheet| sheet.status.last().expect("a note").1.clone();

        let _ = sheet.update(Message::Tested(
            "x".to_string(),
            probe(true, None, vec!["a", "b"]),
        ));
        assert_eq!(note(&sheet), "Answered — 2 models");

        let _ = sheet.update(Message::Tested("x".to_string(), probe(true, None, vec![])));
        assert_eq!(note(&sheet), "Answered — no models");

        let _ = sheet.update(Message::Tested(
            "x".to_string(),
            probe(false, Some("401 unauthorized"), vec![]),
        ));
        assert_eq!(note(&sheet), "401 unauthorized");
    }

    /// Onboarding *is* the settings sheet, with no provider configured. If the
    /// two ever became different screens this would still pass — so it also
    /// asserts the agent block is the one thing onboarding does not show, since
    /// a step limit is meaningless before there is anything to run.
    #[test]
    fn onboarding_is_the_same_sheet_with_no_providers() -> Result<(), iced_test::Error> {
        let mut sheet = sheet();
        assert!(sheet.first_run());
        let palette = palette();
        {
            let mut ui = iced_test::simulator(sheet.view(&palette));
            assert!(ui.find("Set up wizard").is_ok());
            assert!(ui.find("Step limit").is_err(), "no agent block yet");
        }

        // The same sheet with a provider is Settings, and it has the block.
        sheet.view.first_run = false;
        sheet
            .view
            .providers
            .push(crate::plugins::gui::settings::ProviderRow {
                name: "xai".to_string(),
                kind: "xai".to_string(),
                base_url: "https://api.x.ai/v1".to_string(),
                model: "grok-4.5".to_string(),
                key: KeySource::Oauth,
                active: true,
            });
        let mut ui = iced_test::simulator(sheet.view(&palette));
        assert!(ui.find("Settings").is_ok());
        assert!(ui.find("Step limit").is_ok());
        assert!(
            ui.find("signed in").is_ok(),
            "the row says where the key is"
        );
        Ok(())
    }

    /// A provider with no key is the one amber thing in the list, because it is
    /// the one that answers "why is it 401ing" before the turn that would.
    #[test]
    fn a_provider_with_no_key_is_marked() -> Result<(), iced_test::Error> {
        let mut sheet = sheet();
        sheet.view.first_run = false;
        sheet
            .view
            .providers
            .push(crate::plugins::gui::settings::ProviderRow {
                name: "openai".to_string(),
                kind: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-5".to_string(),
                key: KeySource::Missing,
                active: true,
            });
        let mut ui = iced_test::simulator(sheet.view(&palette()));
        assert!(ui.find("no key").is_ok());
        Ok(())
    }
}
