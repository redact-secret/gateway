//! Structural controls: `model`, `stream`, `stream_options` and the numeric parameters.
//! Owner: nobody in Alpha 2 (unchanged).

use serde_json::Number;

use super::{Checked, MAX_MODEL_BYTES, boolean, string, unsupported};
use crate::protocol::json::Json;

/// `stream_options`: only `include_usage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamOptions {
    pub include_usage: Option<bool>,
}

/// Validated numeric/control parameters. Each present value passed its range check; the
/// original JSON number literal is kept.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Params {
    pub temperature: Option<Number>,
    pub top_p: Option<Number>,
    pub max_tokens: Option<Number>,
    pub max_completion_tokens: Option<Number>,
    pub presence_penalty: Option<Number>,
    pub frequency_penalty: Option<Number>,
    /// Only the value `1` is accepted.
    pub n: Option<Number>,
    pub seed: Option<Number>,
}

pub(in crate::protocol) fn parse_model(value: Json) -> Checked<String> {
    let model = string(value)?;
    let well_formed = !model.is_empty()
        && model.len() <= MAX_MODEL_BYTES
        && model.chars().enumerate().all(|(i, c)| {
            c.is_ascii_alphanumeric()
                || (i > 0 && matches!(c, '.' | '_' | ':' | '/' | '@' | '+' | '-'))
        });
    if well_formed {
        Ok(model)
    } else {
        Err(unsupported())
    }
}

pub(super) fn parse_stream_options(value: Json) -> Checked<StreamOptions> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut include_usage = None;
    for (key, value) in entries {
        match key.as_str() {
            "include_usage" => include_usage = Some(boolean(value)?),
            _ => return Err(unsupported()),
        }
    }
    Ok(StreamOptions { include_usage })
}
