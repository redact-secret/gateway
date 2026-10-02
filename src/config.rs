//! Static configuration and the immutable `RuntimePlan` (ADR 0006).
//!
//! Configuration is one static JSON document with an explicit `schema_version`. It is read
//! and validated exactly once at startup into an immutable [`RuntimePlan`]; nothing rereads
//! it later and there is no hot reload. Unknown fields are rejected at every depth, every
//! value is validated, and the plan has three separate authorities so a content-policy
//! change cannot alter deployment authority. Resource limits have no numeric defaults:
//! every value must be present in the file (docs/contracts/resource-limits.md).
//!
//! Diagnostics ([`ConfigError`]) carry a fixed kind and a static schema location only.
//! They never echo file content, key names found in the file, values, or paths.

use std::fmt;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::Path;

use crate::admission::CapacityPlan;
use crate::core_bridge;
use crate::protocol::json::{self, Json};
use crate::telemetry::SafeCode;

/// The only schema version this build accepts.
pub const SCHEMA_VERSION: u64 = 1;

/// Upper bound on the configuration file size. A file-format guard, not a request limit.
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// Operator-defined route identifier. Bounded label used for telemetry and for binding a
/// sanitized body to an approved route. Never derived from client payload or headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteId(Box<str>);

impl RouteId {
    #[must_use]
    pub fn new(label: &str) -> Self {
        Self(label.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Listener authority. Loopback unless the operator explicitly acknowledges otherwise.
/// Loopback is an address restriction, not caller authentication (ADR 0009). The
/// invariant is enforced by the constructor, so an unacknowledged non-loopback listener
/// cannot exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenerAuthority {
    addr: SocketAddr,
    non_loopback_acknowledged: bool,
}

impl ListenerAuthority {
    /// # Errors
    /// [`ConfigErrorKind::InvalidCombination`] when `addr` is not loopback and the
    /// operator did not set `allow_non_loopback`, or when the flag is set for a loopback
    /// address (an acknowledgement with nothing to acknowledge is a config mistake).
    pub fn new(addr: SocketAddr, allow_non_loopback: bool) -> Result<Self, ConfigError> {
        if addr.ip().is_loopback() == allow_non_loopback {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidCombination,
                "deployment.listener",
            ));
        }
        Ok(Self {
            addr,
            non_loopback_acknowledged: allow_non_loopback,
        })
    }

    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    #[must_use]
    pub const fn non_loopback_acknowledged(&self) -> bool {
        self.non_loopback_acknowledged
    }
}

/// Deployment authority: listener, upstream origins, TLS rules, credential requirements.
/// Only the listener exists in the skeleton. Origins, TLS, and credential rules arrive
/// with #23-#25 and will live here, never in content or resource policy. Credentials are
/// never configuration values.
#[derive(Debug)]
#[non_exhaustive]
pub struct DeploymentAuthority {
    listener: ListenerAuthority,
}

impl DeploymentAuthority {
    #[must_use]
    pub const fn new(listener: ListenerAuthority) -> Self {
        Self { listener }
    }

    /// Loopback listener on an OS-assigned port, for tests and scaffolding. Not a
    /// configuration default: the real binary only serves a validated file.
    #[must_use]
    pub const fn placeholder() -> Self {
        Self {
            listener: ListenerAuthority {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                non_loopback_acknowledged: false,
            },
        }
    }

    #[must_use]
    pub const fn listener(&self) -> &ListenerAuthority {
        &self.listener
    }
}

/// Content policy: core profile and actions. Selects only supported core public APIs
/// (`Policy::compile()` is not assumed to exist; #5 verifies).
#[derive(Debug)]
pub struct ContentPolicy {
    profile: redact_secret::Profile,
}

impl ContentPolicy {
    #[must_use]
    pub const fn new(profile: redact_secret::Profile) -> Self {
        Self { profile }
    }

    #[must_use]
    pub const fn profile(&self) -> redact_secret::Profile {
        self.profile
    }
}

/// Resource policy: limits, deadlines, capacities.
#[derive(Debug)]
pub struct ResourcePolicy {
    capacity: CapacityPlan,
}

impl ResourcePolicy {
    #[must_use]
    pub const fn new(capacity: CapacityPlan) -> Self {
        Self { capacity }
    }

    #[must_use]
    pub const fn capacity(&self) -> &CapacityPlan {
        &self.capacity
    }
}

/// Immutable startup plan shared by reference. No setters, no `Default`, no interior
/// mutability. It deliberately does not hold the core `DetectorRegistry`, which is
/// `!Send + !Sync` in the pinned core; #5 decides how inspection owners create theirs.
#[derive(Debug)]
pub struct RuntimePlan {
    deployment: DeploymentAuthority,
    content: ContentPolicy,
    resources: ResourcePolicy,
}

impl RuntimePlan {
    #[must_use]
    pub const fn new(
        deployment: DeploymentAuthority,
        content: ContentPolicy,
        resources: ResourcePolicy,
    ) -> Self {
        Self {
            deployment,
            content,
            resources,
        }
    }

    #[must_use]
    pub const fn deployment(&self) -> &DeploymentAuthority {
        &self.deployment
    }

    #[must_use]
    pub const fn content(&self) -> &ContentPolicy {
        &self.content
    }

    #[must_use]
    pub const fn resources(&self) -> &ResourcePolicy {
        &self.resources
    }
}

/// Class of configuration failure. Fixed set; never carries content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigErrorKind {
    /// The file could not be read.
    Unreadable,
    /// The file exceeds [`MAX_CONFIG_BYTES`].
    TooLarge,
    /// Not a single strict JSON document (syntax, encoding, duplicate keys, depth).
    Malformed,
    /// `schema_version` is not [`SCHEMA_VERSION`].
    UnsupportedSchemaVersion,
    /// A required field is absent.
    MissingField,
    /// A field outside the schema is present.
    UnknownField,
    /// A field has the wrong JSON type.
    InvalidType,
    /// A field has the right type but a disallowed value.
    InvalidValue,
    /// Individually valid values that are not allowed together.
    InvalidCombination,
}

impl ConfigErrorKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unreadable => "unreadable",
            Self::TooLarge => "too_large",
            Self::Malformed => "malformed",
            Self::UnsupportedSchemaVersion => "unsupported_schema_version",
            Self::MissingField => "missing_field",
            Self::UnknownField => "unknown_field",
            Self::InvalidType => "invalid_type",
            Self::InvalidValue => "invalid_value",
            Self::InvalidCombination => "invalid_combination",
        }
    }
}

/// Safe configuration failure: a fixed kind plus a static schema location such as
/// `deployment.listener.address`. For an unknown field the location is the parent object,
/// never the offending key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    location: &'static str,
}

impl ConfigError {
    #[must_use]
    pub const fn new(kind: ConfigErrorKind, location: &'static str) -> Self {
        Self { kind, location }
    }

    #[must_use]
    pub const fn kind(&self) -> ConfigErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn location(&self) -> &'static str {
        self.location
    }

    #[must_use]
    pub const fn code(&self) -> SafeCode {
        SafeCode::InvalidConfig
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} at {}",
            self.code().as_str(),
            self.kind.as_str(),
            self.location
        )
    }
}

impl std::error::Error for ConfigError {}

/// Read, parse, and validate the configuration file into the immutable plan. This is the
/// only place the file is read; call it once at startup.
///
/// # Errors
/// [`ConfigError`] for any read, size, syntax, schema, or value problem.
pub fn load_from_path(path: &Path) -> Result<RuntimePlan, ConfigError> {
    const LOC: &str = "file";
    let file = std::fs::File::open(path)
        .map_err(|_| ConfigError::new(ConfigErrorKind::Unreadable, LOC))?;
    let mut bytes = Vec::new();
    let limit = u64::try_from(MAX_CONFIG_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|_| ConfigError::new(ConfigErrorKind::Unreadable, LOC))?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(ConfigErrorKind::TooLarge, LOC));
    }
    parse(&bytes)
}

/// Parse and validate configuration bytes into the immutable plan.
///
/// # Errors
/// [`ConfigError`] for any syntax, schema, or value problem.
pub fn parse(bytes: &[u8]) -> Result<RuntimePlan, ConfigError> {
    let doc = json::parse_strict(bytes)
        .map_err(|_| ConfigError::new(ConfigErrorKind::Malformed, "file"))?;
    let root = Obj::new(&doc, "root")?;

    // The version gates everything else: a future schema may have different fields.
    match root.require("schema_version", "schema_version")? {
        Json::Number(n) if n.as_u64() == Some(SCHEMA_VERSION) => {}
        Json::Number(_) => {
            return Err(ConfigError::new(
                ConfigErrorKind::UnsupportedSchemaVersion,
                "schema_version",
            ));
        }
        _ => {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidType,
                "schema_version",
            ));
        }
    }
    root.only(&["schema_version", "deployment", "content", "resources"])?;

    let deployment = parse_deployment(&Obj::new(
        root.require("deployment", "deployment")?,
        "deployment",
    )?)?;
    let content = parse_content(&Obj::new(root.require("content", "content")?, "content")?)?;
    let resources = parse_resources(&Obj::new(
        root.require("resources", "resources")?,
        "resources",
    )?)?;
    Ok(RuntimePlan::new(deployment, content, resources))
}

fn parse_deployment(obj: &Obj<'_>) -> Result<DeploymentAuthority, ConfigError> {
    obj.only(&["listener"])?;
    let listener = Obj::new(
        obj.require("listener", "deployment.listener")?,
        "deployment.listener",
    )?;
    listener.only(&["address", "allow_non_loopback"])?;
    let addr = listener
        .require("address", "deployment.listener.address")?
        .as_str("deployment.listener.address")?
        .parse::<SocketAddr>()
        .map_err(|_| {
            ConfigError::new(ConfigErrorKind::InvalidValue, "deployment.listener.address")
        })?;
    let allow = match listener.get("allow_non_loopback") {
        None => false,
        Some(Json::Bool(b)) => *b,
        Some(_) => {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidType,
                "deployment.listener.allow_non_loopback",
            ));
        }
    };
    Ok(DeploymentAuthority::new(ListenerAuthority::new(
        addr, allow,
    )?))
}

fn parse_content(obj: &Obj<'_>) -> Result<ContentPolicy, ConfigError> {
    obj.only(&["profile"])?;
    let name = obj
        .require("profile", "content.profile")?
        .as_str("content.profile")?;
    let profile = core_bridge::parse_profile(name)
        .map_err(|_| ConfigError::new(ConfigErrorKind::InvalidValue, "content.profile"))?;
    Ok(ContentPolicy::new(profile))
}

fn parse_resources(obj: &Obj<'_>) -> Result<ResourcePolicy, ConfigError> {
    obj.only(&["capacity"])?;
    let cap = Obj::new(
        obj.require("capacity", "resources.capacity")?,
        "resources.capacity",
    )?;
    cap.only(&[
        "receipt",
        "memory_units",
        "inspection",
        "upstream",
        "stream",
    ])?;
    let n = |key: &str, loc: &'static str| -> Result<NonZeroU32, ConfigError> {
        cap.require(key, loc)?.as_nonzero_u32(loc)
    };
    Ok(ResourcePolicy::new(CapacityPlan::new(
        n("receipt", "resources.capacity.receipt")?,
        n("memory_units", "resources.capacity.memory_units")?,
        n("inspection", "resources.capacity.inspection")?,
        n("upstream", "resources.capacity.upstream")?,
        n("stream", "resources.capacity.stream")?,
    )))
}

/// Borrowed view of a JSON object with strict-schema helpers.
struct Obj<'a> {
    entries: &'a [(String, Json)],
    loc: &'static str,
}

impl<'a> Obj<'a> {
    fn new(value: &'a Json, loc: &'static str) -> Result<Self, ConfigError> {
        match value {
            Json::Object(entries) => Ok(Self { entries, loc }),
            _ => Err(ConfigError::new(ConfigErrorKind::InvalidType, loc)),
        }
    }

    fn get(&self, key: &str) -> Option<&'a Json> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn require(&self, key: &str, loc: &'static str) -> Result<&'a Json, ConfigError> {
        self.get(key)
            .ok_or(ConfigError::new(ConfigErrorKind::MissingField, loc))
    }

    /// Reject any key outside `allowed`. Reports the parent location, never the key.
    fn only(&self, allowed: &[&str]) -> Result<(), ConfigError> {
        if self
            .entries
            .iter()
            .all(|(k, _)| allowed.contains(&k.as_str()))
        {
            Ok(())
        } else {
            Err(ConfigError::new(ConfigErrorKind::UnknownField, self.loc))
        }
    }
}

trait JsonExt {
    fn as_str(&self, loc: &'static str) -> Result<&str, ConfigError>;
    fn as_nonzero_u32(&self, loc: &'static str) -> Result<NonZeroU32, ConfigError>;
}

impl JsonExt for Json {
    fn as_str(&self, loc: &'static str) -> Result<&str, ConfigError> {
        match self {
            Self::String(s) => Ok(s),
            _ => Err(ConfigError::new(ConfigErrorKind::InvalidType, loc)),
        }
    }

    fn as_nonzero_u32(&self, loc: &'static str) -> Result<NonZeroU32, ConfigError> {
        match self {
            Self::Number(n) => n
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .and_then(NonZeroU32::new)
                .ok_or(ConfigError::new(ConfigErrorKind::InvalidValue, loc)),
            _ => Err(ConfigError::new(ConfigErrorKind::InvalidType, loc)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{
        "schema_version": 1,
        "deployment": {"listener": {"address": "127.0.0.1:0"}},
        "content": {"profile": "common"},
        "resources": {"capacity": {
            "receipt": 1, "memory_units": 1, "inspection": 1, "upstream": 1, "stream": 1
        }}
    }"#;

    fn err(doc: &str) -> ConfigError {
        parse(doc.as_bytes()).expect_err("must be rejected")
    }

    #[test]
    fn valid_config_builds_plan() {
        let plan = parse(VALID.as_bytes()).expect("valid");
        assert!(plan.deployment().listener().addr().ip().is_loopback());
        assert!(!plan.deployment().listener().non_loopback_acknowledged());
    }

    #[test]
    fn unknown_field_at_any_depth_is_rejected_without_naming_it() {
        for doc in [
            VALID.replace(
                "\"schema_version\": 1,",
                "\"schema_version\": 1, \"SYNTH_KEY\": 1,",
            ),
            VALID.replace("\"common\"", "\"common\", \"SYNTH_KEY\": 1"),
            VALID.replace("127.0.0.1:0\"", "127.0.0.1:0\", \"SYNTH_KEY\": 1"),
            VALID.replace("\"stream\": 1", "\"stream\": 1, \"SYNTH_KEY\": 1"),
        ] {
            let e = err(&doc);
            assert_eq!(e.kind(), ConfigErrorKind::UnknownField);
            assert!(!e.to_string().contains("SYNTH_KEY"));
        }
    }

    #[test]
    fn schema_version_is_explicit_and_checked_first() {
        let e = err(r#"{"deployment": {}}"#);
        assert_eq!(e.kind(), ConfigErrorKind::MissingField);
        let e = err(r#"{"schema_version": 2, "future": true}"#);
        assert_eq!(e.kind(), ConfigErrorKind::UnsupportedSchemaVersion);
        let e = err(r#"{"schema_version": "1"}"#);
        assert_eq!(e.kind(), ConfigErrorKind::InvalidType);
    }

    #[test]
    fn limits_are_required_and_nonzero() {
        let e = err(&VALID.replace("\"receipt\": 1,", ""));
        assert_eq!(e.kind(), ConfigErrorKind::MissingField);
        let e = err(&VALID.replace("\"receipt\": 1", "\"receipt\": 0"));
        assert_eq!(e.kind(), ConfigErrorKind::InvalidValue);
        let e = err(&VALID.replace("\"receipt\": 1", "\"receipt\": 4294967296"));
        assert_eq!(e.kind(), ConfigErrorKind::InvalidValue);
        let e = err(&VALID.replace("\"receipt\": 1", "\"receipt\": -1"));
        assert_eq!(e.kind(), ConfigErrorKind::InvalidValue);
    }

    #[test]
    fn non_loopback_requires_explicit_acknowledgement() {
        let open = VALID.replace("127.0.0.1:0", "0.0.0.0:0");
        assert_eq!(err(&open).kind(), ConfigErrorKind::InvalidCombination);
        let acked = open.replace(
            "\"address\": \"0.0.0.0:0\"",
            "\"address\": \"0.0.0.0:0\", \"allow_non_loopback\": true",
        );
        let plan = parse(acked.as_bytes()).expect("acknowledged");
        assert!(plan.deployment().listener().non_loopback_acknowledged());
        let pointless = VALID.replace(
            "\"address\": \"127.0.0.1:0\"",
            "\"address\": \"127.0.0.1:0\", \"allow_non_loopback\": true",
        );
        assert_eq!(err(&pointless).kind(), ConfigErrorKind::InvalidCombination);
    }

    #[test]
    fn bad_values_and_duplicates_are_rejected() {
        assert_eq!(
            err(&VALID.replace("127.0.0.1:0", "localhost:80")).kind(),
            ConfigErrorKind::InvalidValue
        );
        assert_eq!(
            err(&VALID.replace("common", "SYNTH_PROFILE")).kind(),
            ConfigErrorKind::InvalidValue
        );
        assert_eq!(
            err(&VALID.replace(
                "\"schema_version\": 1,",
                "\"schema_version\": 1, \"schema_version\": 1,"
            ))
            .kind(),
            ConfigErrorKind::Malformed
        );
        assert_eq!(err("not json").kind(), ConfigErrorKind::Malformed);
        assert_eq!(err("[]").kind(), ConfigErrorKind::InvalidType);
    }

    #[test]
    fn credentials_are_not_configuration() {
        let doc = VALID.replace(
            "\"content\":",
            "\"api_key\": \"SYNTH-SECRET-MARKER\", \"content\":",
        );
        let e = err(&doc);
        assert_eq!(e.kind(), ConfigErrorKind::UnknownField);
        assert!(!e.to_string().contains("SYNTH"));
    }
}
