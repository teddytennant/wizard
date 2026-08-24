//! Subscription sign-in from the settings sheet.
//!
//! An API key is a string the user can paste; a subscription is not. It is an
//! OAuth round trip: we open a browser on an authorize URL, the user approves,
//! and the provider sends them back to a redirect we exchange for tokens that
//! live in `~/.wizard/`.
//!
//! Both providers only send the browser to the loopback address registered for
//! their client id — OpenAI's `localhost:1455/auth/callback`, xAI's
//! `127.0.0.1:56121/callback` — and ignore anything else, so each flow binds
//! *its* listener itself, exactly as the terminal flows do
//! ([`crate::llm::xai_oauth::login`]). The sheet opens the authorize URL, the
//! flow finishes in a spawned task that outlives the click, and the sheet
//! polls [`Status`] until it settles.
//!
//! One sign-in may be in flight at a time, and a second attempt **replaces**
//! the first rather than racing it: a person signs in to one account at a time,
//! and closing the provider's tab to click sign-in again is the most natural
//! retry there is. Since the flow in flight owns the one port its provider
//! redirects to, replacing it means cancelling it
//! ([`crate::llm::oauth_callback`]) and waiting for that port to come back
//! before binding — otherwise the retry would hit "address already in use" for
//! as long as the abandoned flow sat there.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::config::ProviderConfig;
use crate::gui::settings::{self, ConfigStore};
use crate::llm::oauth_callback::{self, Canceller};
#[cfg(feature = "provider-chatgpt")]
use crate::plugins::chatgpt::oauth as chatgpt_oauth;

use crate::llm::xai_oauth;

/// How long a replaced sign-in gets to notice it was cancelled and drop its
/// listener. It is a `select!` arm away from doing so; this bound only stops a
/// flow wedged in its token exchange from blocking the new sign-in's request
/// forever. Overrun and the bind below fails with a clear message — which is
/// still an answer, where hanging is not.
const RELEASE_GRACE: Duration = Duration::from_secs(5);

/// The subscription sign-ins this build supports: the key a surface passes to
/// [`SignIn::begin`], what a row calls it, and what the subscription is.
///
/// One list, because the sheet offers these rows and `wizard --login` names
/// them on the command line. The browser GUI that this replaced hard-coded its
/// copy in `settings.js` and matched on strings server-side; a provider added
/// to one and not the other was a row that opened nothing.
///
/// ChatGPT's row is gated on its plugin because its whole sign-in lives there;
/// xAI's is not, because its token store is core and signing in is useful to
/// `web_search` and `generate_image` whatever chat backend is configured. The
/// `debug_assert` in [`OauthState::begin`] is what keeps this honest: a row
/// here with no flow behind it fails a debug build immediately.
pub const SUPPORTED: &[(&str, &str, &str)] = &[
    #[cfg(feature = "provider-chatgpt")]
    (
        "chatgpt",
        "Sign in with ChatGPT",
        "Plus / Pro / Team subscription",
    ),
    ("xai", "Sign in with xAI", "SuperGrok subscription"),
];

/// What the sheet polls while the user is off in the provider's tab.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Status {
    /// No sign-in has been started.
    #[default]
    Idle,
    /// Waiting for the user to finish in the provider's tab.
    Pending { provider: String },
    /// Signed in; the provider is configured and active.
    Done { provider: String },
    /// The exchange failed, or the user denied it.
    Failed { provider: String, error: String },
}

/// What the sign-in in flight is doing, and how the last one ended.
#[derive(Default)]
pub struct SignIn {
    inner: Mutex<Inner>,
    /// Serializes the start sequence (cancel the old flow, bind, spawn), so two
    /// clicks racing cannot both believe they are the flow in flight.
    starting: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Inner {
    status: Status,
    /// Bumped by every start. A flow only writes its outcome while this still
    /// names it, so one that was cancelled and finishes late cannot clobber the
    /// sign-in that replaced it.
    generation: u64,
    in_flight: Option<InFlight>,
}

/// The flow in flight: the handle that cancels it, and the task to wait on so
/// its port is known to be free before the next flow binds.
struct InFlight {
    cancel: Canceller,
    task: JoinHandle<()>,
}

impl SignIn {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub fn status(&self) -> Status {
        self.lock().status.clone()
    }

    /// Start an xAI sign-in, replacing any in flight. `store` receives the
    /// provider once the tokens land.
    /// Start the sign-in named by `provider` and hand back the consent URL.
    ///
    /// The one place a provider *name* is turned into a flow, so [`SUPPORTED`]
    /// is the whole list and a row offered in a picker cannot be a row nothing
    /// answers to.
    pub async fn begin(
        self: &Arc<Self>,
        provider: &str,
        store: Arc<ConfigStore>,
    ) -> anyhow::Result<String> {
        match provider {
            "xai" => self.begin_xai(store).await,
            #[cfg(feature = "provider-chatgpt")]
            "chatgpt" => self.begin_chatgpt(store).await,
            other => {
                debug_assert!(
                    !SUPPORTED.iter().any(|(name, _, _)| *name == other),
                    "{other} is offered in SUPPORTED but has no flow here"
                );
                anyhow::bail!("cannot sign in to '{other}'")
            }
        }
    }

    pub async fn begin_xai(self: &Arc<Self>, store: Arc<ConfigStore>) -> anyhow::Result<String> {
        let _starting = self.starting.lock().await;
        self.release_in_flight().await;

        let pending = xai_oauth::begin_browser_login().await?;
        let url = pending.authorize_url.clone();
        let (cancel, cancelled) = oauth_callback::cancellation();
        let generation = self.mark_pending("xai");
        let this = Arc::clone(self);
        let task = tokio::spawn(async move {
            let outcome = xai_oauth::wait_and_complete(pending, cancelled)
                .await
                .map(|_| xai_oauth::provider_config());
            this.settle(generation, "xai", &store, outcome);
        });
        self.set_in_flight(generation, InFlight { cancel, task });
        Ok(url)
    }

    /// Start a ChatGPT sign-in, replacing any in flight. `store` receives the
    /// provider once the tokens land.
    #[cfg(feature = "provider-chatgpt")]
    pub async fn begin_chatgpt(
        self: &Arc<Self>,
        store: Arc<ConfigStore>,
    ) -> anyhow::Result<String> {
        let _starting = self.starting.lock().await;
        self.release_in_flight().await;

        let pending = chatgpt_oauth::begin_login()?;
        let url = pending.authorize_url.clone();
        let (cancel, cancelled) = oauth_callback::cancellation();
        let generation = self.mark_pending("chatgpt");
        let this = Arc::clone(self);
        let task = tokio::spawn(async move {
            let outcome = chatgpt_oauth::wait_and_complete(pending, cancelled)
                .await
                .map(|_| chatgpt_oauth::provider_config());
            this.settle(generation, "chatgpt", &store, outcome);
        });
        self.set_in_flight(generation, InFlight { cancel, task });
        Ok(url)
    }

    /// Cancel the flow in flight and wait for it to let go of its callback
    /// port. The waiting is the point: the next flow binds that very port, and
    /// a bind racing the old listener's close would fail.
    async fn release_in_flight(&self) {
        let Some(previous) = self.lock().in_flight.take() else {
            return;
        };
        previous.cancel.cancel();
        if tokio::time::timeout(RELEASE_GRACE, previous.task)
            .await
            .is_err()
        {
            tracing::warn!("the previous sign-in did not release its callback port in time");
        }
    }

    /// Open a new generation: the flow it names is the one in flight now, and
    /// every older one is muted.
    fn mark_pending(&self, provider: &str) -> u64 {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.status = Status::Pending {
            provider: provider.to_string(),
        };
        inner.generation
    }

    /// Record the flow's cancel handle — unless it has already been replaced,
    /// or has already finished, in which case there is nothing left to cancel.
    fn set_in_flight(&self, generation: u64, flow: InFlight) {
        let mut inner = self.lock();
        if inner.generation == generation {
            inner.in_flight = Some(flow);
        }
    }

    /// Land a finished sign-in: write the provider it earned and make it active
    /// (a sign-in that leaves you without a usable provider was pointless), or
    /// record why it failed.
    ///
    /// A flow from an older `generation` was cancelled and replaced; it says
    /// nothing, because the status belongs to the sign-in that replaced it.
    /// Otherwise every path ends in [`Status::Done`] or [`Status::Failed`]: the
    /// tab that started the flow is polling, and a failure it never hears about
    /// leaves it waiting forever.
    fn settle(
        &self,
        generation: u64,
        provider: &str,
        store: &ConfigStore,
        outcome: anyhow::Result<ProviderConfig>,
    ) {
        // Saving touches the disk, so it happens before the lock is taken.
        let status = match outcome {
            Ok(config) => {
                let name = config.name.clone();
                let saved = store.update(move |on_disk| {
                    settings::upsert_provider(on_disk, config, true);
                    Ok(())
                });
                match saved {
                    Ok(_) => Status::Done { provider: name },
                    Err(err) => Status::Failed {
                        provider: provider.to_string(),
                        error: format!("signed in, but saving failed: {err:#}"),
                    },
                }
            }
            Err(err) => Status::Failed {
                provider: provider.to_string(),
                error: format!("{err:#}"),
            },
        };

        let mut inner = self.lock();
        if inner.generation != generation {
            return;
        }
        inner.status = status;
        inner.in_flight = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn store() -> Arc<ConfigStore> {
        Arc::new(ConfigStore::new(Config::default()))
    }

    /// xAI's OpenID discovery document, served on loopback.
    ///
    /// [`SignIn::begin_xai`] discovers the endpoints before it binds anything,
    /// and a test that reached `auth.x.ai` for them would be a test of the
    /// network. Pointing discovery here is what lets the regression below run
    /// offline and identically every time, rather than skipping itself whenever
    /// xAI is out of reach — which is precisely when a regression test is worth
    /// nothing.
    ///
    /// The endpoints in the document are the real ones (they are pinned to x.ai,
    /// so they have to be), but nothing dials them: the flows are cancelled long
    /// before any token exchange. Returns the stub's URL and its task, which the
    /// caller aborts.
    async fn serve_discovery() -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind the discovery stub");
        let url = format!(
            "http://{}/.well-known/openid-configuration",
            listener.local_addr().expect("stub address")
        );
        let task = tokio::spawn(async move {
            const BODY: &str = r#"{"authorization_endpoint":"https://auth.x.ai/oauth/auth",
                                   "token_endpoint":"https://auth.x.ai/oauth/token"}"#;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                // The stub answers one document, so the request is read only to
                // let the client finish writing it.
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
                    BODY.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (url, task)
    }

    #[test]
    fn a_fresh_sign_in_is_idle() {
        assert_eq!(SignIn::default().status(), Status::Idle);
    }

    #[test]
    fn a_failed_sign_in_is_reported_not_left_pending() {
        // The sheet polls until it sees done or failed; a flow that died
        // without saying so would keep it waiting forever.
        let sign_in = SignIn::default();
        let generation = sign_in.mark_pending("xai");
        sign_in.settle(
            generation,
            "xai",
            &store(),
            Err(anyhow::anyhow!("timed out waiting for the browser sign-in")),
        );
        match sign_in.status() {
            Status::Failed { provider, error } => {
                assert_eq!(provider, "xai");
                assert!(error.contains("timed out"), "{error}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_replaced_flow_cannot_clobber_the_one_that_replaced_it() {
        // The cancelled flow finishes late, carrying the failure its own
        // cancellation caused. The sign-in the user is now waiting on must
        // survive it.
        let sign_in = SignIn::default();
        let stale = sign_in.mark_pending("xai");
        let current = sign_in.mark_pending("xai");
        assert_ne!(stale, current);

        sign_in.settle(
            stale,
            "xai",
            &store(),
            Err(anyhow::anyhow!("the sign-in was replaced by a newer one")),
        );
        assert_eq!(
            sign_in.status(),
            Status::Pending {
                provider: "xai".to_string()
            },
            "a replaced flow must not speak for the one in flight"
        );
    }

    /// The regression: clicking sign-in again while one is pending must work.
    /// The first flow holds xAI's only redirect port for five minutes unless the
    /// second takes it back, so without the cancel-and-wait the second
    /// `begin_xai` cannot bind and the retry is dead until the timeout.
    ///
    /// Hermetic: discovery is the loopback stub, and under `cfg(test)` the flow
    /// binds a private port rather than xAI's registered one. So the test never
    /// touches the network, never competes with a sign-in the user has in
    /// flight, and — the point of it — never has an excuse not to run.
    ///
    /// Not a `#[tokio::test]`: it holds one fixed callback port across the whole
    /// run, and the guard for that is a plain lock.
    #[test]
    fn a_second_sign_in_replaces_the_first_and_rebinds_the_port() {
        let _serial = oauth_callback::serial_callback_port();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        runtime.block_on(async {
            let (discovery, stub) = serve_discovery().await;
            xai_oauth::use_test_discovery_url(&discovery);

            let sign_in = Arc::new(SignIn::default());
            let first = sign_in
                .begin_xai(store())
                .await
                .expect("the first sign-in binds the callback port");
            assert!(matches!(sign_in.status(), Status::Pending { .. }));

            let second = sign_in
                .begin_xai(store())
                .await
                .expect("the retry must rebind the port the first flow was holding");
            assert_ne!(first, second, "a fresh authorize URL, not the stale one");
            assert!(matches!(sign_in.status(), Status::Pending { .. }));

            // Leave no listener behind on the shared port.
            sign_in.release_in_flight().await;
            stub.abort();
        });
    }
}
