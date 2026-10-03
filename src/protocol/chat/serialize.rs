//! Fresh bounded serialization (ADR 0015 #11, ADR 0025). Owner: the #52 baseline.
//!
//! The outbound body is written from the typed request, never from the original bytes.
//! Canonical key order (Alpha 1 requests are byte-identical to before the split):
//! `model`, `messages`, `tools`/`tool_choice`/`parallel_tool_calls` (#54), `stream`,
//! `stream_options`, the numeric controls, `stop`, `user`, `metadata` (#55),
//! `response_format`. Each module writes only its own keys through its own `write`
//! function, so adding a field is a one-line call here plus the module's function.

use std::io::{self, Write};

use super::{ChatRequest, messages, metadata, response_format, stop_user, tool_defs};

/// Why serialization of the transformed request failed. Carries nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerializeError {
    /// The output would exceed the transformed-output bound.
    Limit,
    /// The writer failed for another reason.
    Invalid,
}

/// Output sink that refuses to grow past its bound.
pub(super) struct Bounded {
    buf: Vec<u8>,
    max: usize,
    overflow: bool,
}

impl Write for Bounded {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len().saturating_add(data.len()) > self.max {
            self.overflow = true;
            return Err(io::Error::other("output bound"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn json_str(w: &mut Bounded, text: &str) -> io::Result<()> {
    serde_json::to_writer(w, text).map_err(io::Error::from)
}

pub(super) fn json_bool(w: &mut Bounded, value: bool) -> io::Result<()> {
    w.write_all(if value { b"true" } else { b"false" })
}

impl ChatRequest {
    /// Serialize a fresh JSON document from the typed request (never from the original
    /// bytes): canonical key order, the same keys, types, and array order, with every
    /// string escaped by the JSON writer. The output is bounded while it is produced, so an
    /// oversized result is refused before it is fully allocated.
    ///
    /// # Errors
    /// [`SerializeError::Limit`] when the output would exceed `max_bytes`;
    /// [`SerializeError::Invalid`] for any other writer failure (not reachable for the
    /// supported matrix, but never ignored).
    pub fn serialize_bounded(&self, max_bytes: usize) -> Result<Vec<u8>, SerializeError> {
        let mut estimate = 256_usize;
        self.for_each_text(|_, text| estimate = estimate.saturating_add(text.len()));
        let mut out = Bounded {
            buf: Vec::with_capacity(estimate.min(max_bytes)),
            max: max_bytes,
            overflow: false,
        };
        match self.write_to(&mut out) {
            Ok(()) => Ok(out.buf),
            Err(_) if out.overflow => Err(SerializeError::Limit),
            Err(_) => Err(SerializeError::Invalid),
        }
    }

    fn write_to(&self, w: &mut Bounded) -> io::Result<()> {
        w.write_all(b"{\"model\":")?;
        json_str(w, &self.model)?;
        messages::write(&self.messages, w)?;
        tool_defs::write(&self.tool_defs, w)?;
        if let Some(stream) = self.stream {
            w.write_all(b",\"stream\":")?;
            json_bool(w, stream)?;
        }
        if let Some(options) = self.stream_options {
            w.write_all(b",\"stream_options\":{")?;
            if let Some(usage) = options.include_usage {
                w.write_all(b"\"include_usage\":")?;
                json_bool(w, usage)?;
            }
            w.write_all(b"}")?;
        }
        let params = &self.params;
        for (key, value) in [
            ("temperature", &params.temperature),
            ("top_p", &params.top_p),
            ("max_tokens", &params.max_tokens),
            ("max_completion_tokens", &params.max_completion_tokens),
            ("presence_penalty", &params.presence_penalty),
            ("frequency_penalty", &params.frequency_penalty),
            ("n", &params.n),
            ("seed", &params.seed),
        ] {
            if let Some(number) = value {
                w.write_all(b",\"")?;
                w.write_all(key.as_bytes())?;
                w.write_all(b"\":")?;
                serde_json::to_writer(&mut *w, number).map_err(io::Error::from)?;
            }
        }
        stop_user::write_stop(self.stop.as_ref(), w)?;
        stop_user::write_user(self.user.as_deref(), w)?;
        metadata::write(&self.metadata, w)?;
        response_format::write(self.response_format, w)?;
        w.write_all(b"}")
    }
}
