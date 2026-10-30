//! Token and cost accounting for `--max-budget-usd`.

use crate::agent::CancelHandle;
use crate::usage::{PriceInputs, TurnTokens, estimate_cost};

use super::Price;

/// Adds up the tokens a run is billed for, prices them, and cancels the turn
/// in flight once the estimate passes the limit.
#[derive(Debug, Default)]
pub struct Meter {
    model: String,
    price: Price,
    limit: Option<f64>,
    cancel: Option<CancelHandle>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    exceeded: bool,
}

impl Meter {
    pub fn new(model: &str, price: Price, limit: Option<f64>) -> Self {
        Self {
            model: model.to_string(),
            price,
            limit,
            ..Self::default()
        }
    }

    pub fn set_cancel(&mut self, cancel: CancelHandle) {
        self.cancel = Some(cancel);
    }

    pub fn limit(&self) -> Option<f64> {
        self.limit
    }

    /// Whether the estimate has passed the limit.
    pub fn exceeded(&self) -> bool {
        self.exceeded
    }

    /// Estimated cost so far, in USD.
    pub fn cost_usd(&self) -> f64 {
        estimate_cost(
            TurnTokens {
                prompt: self.prompt_tokens,
                completion: self.completion_tokens,
                ..TurnTokens::default()
            },
            &PriceInputs {
                model: &self.model,
                endpoint: &self.price.endpoint,
                usd_per_mtok_in: self.price.usd_per_mtok_in,
                usd_per_mtok_out: self.price.usd_per_mtok_out,
                self_hosted: self.price.self_hosted,
            },
        )
        .usd
    }

    /// Count one model call. Returns true the moment the limit is crossed.
    pub fn add(&mut self, prompt: u64, completion: u64) -> bool {
        self.prompt_tokens += prompt;
        self.completion_tokens += completion;
        let crossed = !self.exceeded && self.limit.is_some_and(|limit| self.cost_usd() >= limit);
        if crossed {
            self.exceeded = true;
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
        }
        crossed
    }
}
