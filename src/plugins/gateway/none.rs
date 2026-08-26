//! The no-op gateway: stands in when no messaging gateway is configured. Any
//! attempt to use it returns an actionable error.

use anyhow::{Result, bail};
use async_trait::async_trait;

use super::{Gateway, Inbound};

/// A gateway that does nothing but report that none is configured.
pub struct NoneGateway;

#[async_trait]
impl Gateway for NoneGateway {
    fn label(&self) -> &str {
        "none"
    }

    async fn poll(&mut self) -> Result<Vec<Inbound>> {
        bail!(
            "no messaging gateway configured — set [gateway] kind = \"telegram\" in \
             ~/.wizard/config.toml (or re-run `wizard --onboard` and pick Telegram), \
             store the bot token under [keys] telegram in ~/.wizard/credentials.toml, \
             then run `wizard --gateway` in your project"
        )
    }

    async fn send(&self, _chat_id: i64, _text: &str) -> Result<()> {
        bail!(
            "no messaging gateway configured — set [gateway] kind = \"telegram\" in \
             ~/.wizard/config.toml (or re-run `wizard --onboard` and pick Telegram), \
             store the bot token under [keys] telegram in ~/.wizard/credentials.toml, \
             then run `wizard --gateway` in your project"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_use_reports_how_to_configure_a_gateway() {
        let mut gateway = NoneGateway;
        let poll_err = gateway.poll().await.expect_err("nothing to poll");
        let message = format!("{poll_err:#}");
        assert!(message.contains("config.toml"), "{message}");
        assert!(message.contains("telegram"), "{message}");

        let send_err = gateway.send(1, "hi").await.expect_err("nowhere to send");
        assert!(format!("{send_err:#}").contains("config.toml"));
    }

    #[tokio::test]
    async fn typing_is_a_harmless_no_op() {
        // serve() fires the typing hint before every turn; a transport
        // without one must not fail the turn over it.
        NoneGateway.typing(1).await.expect("default typing is Ok");
    }
}
