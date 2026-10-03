//! `stop` and `user` (inspected text). Owner: nobody in Alpha 2 (unchanged); #55 audits.

use std::fmt;
use std::io::{self, Write};

use super::serialize::{Bounded, json_str};
use super::slots::TextSlot;
use super::{Checked, MAX_STOP_SEQUENCES, MAX_USER_BYTES, string, unsupported};
use crate::protocol::json::Json;

/// `stop`: one string or up to [`MAX_STOP_SEQUENCES`] strings. Inspected text.
pub enum Stop {
    One(String),
    Many(Vec<String>),
}

impl fmt::Debug for Stop {
    /// Prints only the form and sizes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::One(_) => f.write_str("Stop::One"),
            Self::Many(items) => write!(f, "Stop::Many({})", items.len()),
        }
    }
}

pub(super) fn parse_user(value: Json) -> Checked<String> {
    let user = string(value)?;
    if user.len() <= MAX_USER_BYTES {
        Ok(user)
    } else {
        Err(unsupported())
    }
}

pub(super) fn parse_stop(value: Json) -> Checked<Stop> {
    match value {
        Json::String(s) => Ok(Stop::One(s)),
        Json::Array(items) if !items.is_empty() && items.len() <= MAX_STOP_SEQUENCES => items
            .into_iter()
            .map(string)
            .collect::<Checked<Vec<_>>>()
            .map(Stop::Many),
        _ => Err(unsupported()),
    }
}

/// Visit `stop` texts in order.
pub(super) fn visit_stop(stop: Option<&Stop>, f: &mut impl FnMut(TextSlot, &str)) {
    match stop {
        Some(Stop::One(text)) => f(TextSlot::Stop { index: 0 }, text),
        Some(Stop::Many(items)) => {
            for (index, text) in items.iter().enumerate() {
                f(TextSlot::Stop { index }, text);
            }
        }
        None => {}
    }
}

/// Mutable twin of [`visit_stop`].
pub(super) fn visit_stop_mut(stop: Option<&mut Stop>, f: &mut impl FnMut(TextSlot, &mut String)) {
    match stop {
        Some(Stop::One(text)) => f(TextSlot::Stop { index: 0 }, text),
        Some(Stop::Many(items)) => {
            for (index, text) in items.iter_mut().enumerate() {
                f(TextSlot::Stop { index }, text);
            }
        }
        None => {}
    }
}

/// Write `,"stop":...` when present.
pub(super) fn write_stop(stop: Option<&Stop>, w: &mut Bounded) -> io::Result<()> {
    match stop {
        Some(Stop::One(text)) => {
            w.write_all(b",\"stop\":")?;
            json_str(w, text)?;
        }
        Some(Stop::Many(items)) => {
            w.write_all(b",\"stop\":[")?;
            for (index, text) in items.iter().enumerate() {
                if index > 0 {
                    w.write_all(b",")?;
                }
                json_str(w, text)?;
            }
            w.write_all(b"]")?;
        }
        None => {}
    }
    Ok(())
}

/// Write `,"user":...` when present.
pub(super) fn write_user(user: Option<&str>, w: &mut Bounded) -> io::Result<()> {
    if let Some(user) = user {
        w.write_all(b",\"user\":")?;
        json_str(w, user)?;
    }
    Ok(())
}
