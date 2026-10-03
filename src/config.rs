//! Static configuration and the immutable `RuntimePlan` (ADR 0006).
//!
//! Configuration is one static JSON document with an explicit `schema_version`. It is read
//! and validated exactly once at startup into an immutable [`RuntimePlan`]; nothing rereads
//! it later and there is no hot reload. Unknown fields are rejected at every depth, every
//! value is validated, and the plan has three separate authorities so a content-policy
//! change cannot alter deployment authority. Capacities have no numeric defaults: every
//! value must be present in the file. The optional `resources.limits` object carries the
//! per-request limits, whose provisional values are finite and justified in
//! docs/contracts/resource-limits.md.
//!
//! Diagnostics ([`ConfigError`]) carry a fixed kind and a static schema location only.
//! They never echo file content, key names found in the file, values, or paths.

use std::fmt;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::Path;

use crate::admission::{CapacityPlan, RequestLimits};
use crate::core_bridge;
use crate::protocol::json::{self, Json};
use crate::telemetry::SafeCode;
use crate::transport::local_auth::{self, LocalAuth, LocalToken, TokenReference};

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

/// A reviewed upstream provider profile. Selecting one is the only way configuration can
/// influence upstream destinations: the profile fixes the HTTPS origin, the exact route
/// paths, the port, and the TLS rules inside `transport` (ADR 0013). Configuration never
/// carries a hostname, URL, port, or TLS toggle. Adding a variant is a reviewed change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Provider {
    /// OpenAI public API, `POST /v1/chat/completions` (Alpha 1).
    OpenAi,
}

impl Provider {
    /// Exact, case-sensitive configuration name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "openai" => Some(Self::OpenAi),
            _ => None,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
        }
    }
}

/// Upstream authority: which reviewed provider profile this deployment forwards to.
/// Immutable; derived once at startup and never changed by content or resource policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpstreamAuthority {
    provider: Provider,
}

impl UpstreamAuthority {
    #[must_use]
    pub const fn new(provider: Provider) -> Self {
        Self { provider }
    }

    #[must_use]
    pub const fn provider(&self) -> Provider {
        self.provider
    }
}

/// Deployment authority: listener, upstream provider profile, credential requirements.
/// Content and resource policy live elsewhere; nothing outside this struct can change a
/// destination or a TLS rule. Credentials are never configuration values.
#[derive(Debug)]
#[non_exhaustive]
pub struct DeploymentAuthority {
    listener: ListenerAuthority,
    upstream: Option<UpstreamAuthority>,
    local_auth: LocalAuth,
}

impl DeploymentAuthority {
    /// An authority with no upstream configured: no route exists and nothing can be
    /// forwarded (fail closed).
    #[must_use]
    pub const fn new(listener: ListenerAuthority) -> Self {
        Self {
            listener,
            upstream: None,
            local_auth: LocalAuth::Disabled,
        }
    }

    #[must_use]
    pub const fn with_upstream(mut self, upstream: UpstreamAuthority) -> Self {
        self.upstream = Some(upstream);
        self
    }

    /// Set the local caller authentication authority (#63, ADR 0030).
    ///
    /// # Errors
    /// [`ConfigErrorKind::InvalidCombination`] at `deployment.local_auth.mode` when the
    /// listener is the acknowledged non-loopback kind and no token is enforced: an
    /// acknowledgement alone never opens an unauthenticated listener.
    pub fn with_local_auth(mut self, local_auth: LocalAuth) -> Result<Self, ConfigError> {
        if self.listener.non_loopback_acknowledged() && !local_auth.is_enforced() {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidCombination,
                "deployment.local_auth.mode",
            ));
        }
        self.local_auth = local_auth;
        Ok(self)
    }

    /// The immutable local caller authentication authority. Separate from profiles,
    /// actions, and limits: nothing outside `deployment` can change it.
    #[must_use]
    pub const fn local_auth(&self) -> &LocalAuth {
        &self.local_auth
    }

    #[must_use]
    pub const fn upstream(&self) -> Option<&UpstreamAuthority> {
        self.upstream.as_ref()
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
            upstream: None,
            local_auth: LocalAuth::Disabled,
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
    pii: Vec<String>,
    on_warn: OnWarn,
    max_findings: u32,
}

impl ContentPolicy {
    /// Profile only: no PII selection, `on_warn = reject`, provisional finding bound.
    #[must_use]
    pub const fn new(profile: redact_secret::Profile) -> Self {
        Self {
            profile,
            pii: Vec::new(),
            on_warn: OnWarn::Reject,
            max_findings: DEFAULT_MAX_FINDINGS,
        }
    }

    /// Optional PII selectors (core syntax, for example `pii:family:global:email`).
    #[must_use]
    pub fn with_pii(mut self, pii: Vec<String>) -> Self {
        self.pii = pii;
        self
    }

    #[must_use]
    pub const fn with_on_warn(mut self, on_warn: OnWarn) -> Self {
        self.on_warn = on_warn;
        self
    }

    #[must_use]
    pub const fn with_max_findings(mut self, max_findings: u32) -> Self {
        self.max_findings = max_findings;
        self
    }

    #[must_use]
    pub const fn profile(&self) -> redact_secret::Profile {
        self.profile
    }

    #[must_use]
    pub fn pii(&self) -> &[String] {
        &self.pii
    }

    #[must_use]
    pub const fn on_warn(&self) -> OnWarn {
        self.on_warn
    }

    /// Request-wide bound on findings; exceeding it rejects the request.
    #[must_use]
    pub const fn max_findings(&self) -> u32 {
        self.max_findings
    }
}

/// What to do when the core reports a `Warn` finding: the default policy leaves such text
/// in place (medium confidence, for example `password=...`). Rejecting is the default
/// (fail closed); forwarding is a deliberate operator choice (ADR 0015).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnWarn {
    /// Reject the request when any inspected text has a `Warn` finding.
    #[default]
    Reject,
    /// Forward the text unchanged (the core leaves `Warn` findings in place).
    Forward,
}

/// Most entries accepted in `content.pii`. Selectors are validated by the pinned core.
pub const MAX_PII_SELECTORS: usize = 32;
const MAX_PII_SELECTOR_BYTES: usize = 64;
/// Provisional request-wide finding bound, pending ADR 0008 measurement.
pub const DEFAULT_MAX_FINDINGS: u32 = 1024;
/// Ceiling for `content.max_findings` (the core's own default bound).
pub const MAX_FINDINGS_CEILING: u32 = 50_000;

/// Resource policy: limits, deadlines, capacities.
#[derive(Debug)]
pub struct ResourcePolicy {
    capacity: CapacityPlan,
    limits: RequestLimits,
}

impl ResourcePolicy {
    /// Capacities with the provisional per-request limits.
    #[must_use]
    pub const fn new(capacity: CapacityPlan) -> Self {
        Self {
            capacity,
            limits: RequestLimits::provisional(),
        }
    }

    /// Replace the per-request limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: RequestLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub const fn limits(&self) -> &RequestLimits {
        &self.limits
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
    load_from_path_with(path, &local_auth::resolve_reference)
}

/// Like [`load_from_path`] with the token-reference resolver supplied. Production resolves
/// the real environment or file ([`local_auth::resolve_reference`]); the schema and fixture
/// tests supply a synthetic resolver because a committed fixture cannot carry the required
/// file mode or a secret.
///
/// # Errors
/// [`ConfigError`] for any read, size, syntax, schema, or value problem.
pub fn load_from_path_with(
    path: &Path,
    resolve: &TokenResolver<'_>,
) -> Result<RuntimePlan, ConfigError> {
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
    parse_with(&bytes, resolve)
}

/// Resolves a validated `deployment.local_auth.token` reference into the token.
pub type TokenResolver<'a> = dyn Fn(&TokenReference) -> Result<LocalToken, ConfigError> + 'a;

#[cfg(test)]
thread_local! {
    /// Configuration parses on this thread; lets tests prove none happens per request.
    static PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Number of [`parse`] calls on this thread (unit tests only).
#[cfg(test)]
pub(crate) fn parses_on_this_thread() -> usize {
    PARSES.with(std::cell::Cell::get)
}

/// Parse and validate configuration bytes into the immutable plan.
///
/// # Errors
/// [`ConfigError`] for any syntax, schema, or value problem.
pub fn parse(bytes: &[u8]) -> Result<RuntimePlan, ConfigError> {
    parse_with(bytes, &local_auth::resolve_reference)
}

/// Like [`parse`] with the token-reference resolver supplied (see [`load_from_path_with`]).
/// The resolver runs only for a fully validated `mode: "token"` reference, so every static
/// structural failure is reported before any environment or file access.
///
/// # Errors
/// [`ConfigError`] for any syntax, schema, or value problem.
pub fn parse_with(bytes: &[u8], resolve: &TokenResolver<'_>) -> Result<RuntimePlan, ConfigError> {
    #[cfg(test)]
    PARSES.with(|c| c.set(c.get().saturating_add(1)));
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

    let deployment = parse_deployment(
        &Obj::new(root.require("deployment", "deployment")?, "deployment")?,
        resolve,
    )?;
    let content = parse_content(&Obj::new(root.require("content", "content")?, "content")?)?;
    let resources = parse_resources(&Obj::new(
        root.require("resources", "resources")?,
        "resources",
    )?)?;
    Ok(RuntimePlan::new(deployment, content, resources))
}

fn parse_deployment(
    obj: &Obj<'_>,
    resolve: &TokenResolver<'_>,
) -> Result<DeploymentAuthority, ConfigError> {
    obj.only(&["listener", "upstream", "local_auth"])?;
    let upstream = match obj.get("upstream") {
        None => None,
        Some(v) => Some(parse_upstream(&Obj::new(v, "deployment.upstream")?)?),
    };
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
    let authority = DeploymentAuthority::new(ListenerAuthority::new(addr, allow)?);
    let authority = match upstream {
        Some(u) => authority.with_upstream(u),
        None => authority,
    };
    let local_auth = match obj.get("local_auth") {
        None => None,
        Some(v) => Some(parse_local_auth(
            &Obj::new(v, "deployment.local_auth")?,
            resolve,
        )?),
    };
    // Absent means `disabled`, which an acknowledged non-loopback listener rejects.
    authority.with_local_auth(local_auth.unwrap_or(LocalAuth::Disabled))
}

/// `deployment.local_auth` (#63, ADR 0030). The token is a reference (`env` or `file`),
/// never a value; the reference is resolved here, once, so a bad source stops startup.
fn parse_local_auth(obj: &Obj<'_>, resolve: &TokenResolver<'_>) -> Result<LocalAuth, ConfigError> {
    const MODE: &str = "deployment.local_auth.mode";
    const TOKEN: &str = "deployment.local_auth.token";
    obj.only(&["mode", "token"])?;
    let token = obj.get("token");
    match obj.require("mode", MODE)?.as_str(MODE)? {
        "disabled" => {
            if token.is_some() {
                return Err(ConfigError::new(ConfigErrorKind::InvalidCombination, TOKEN));
            }
            Ok(LocalAuth::Disabled)
        }
        "token" => {
            let source = Obj::new(obj.require("token", TOKEN)?, TOKEN)?;
            source.only(&["env", "file"])?;
            let reference = match (source.get("env"), source.get("file")) {
                (Some(env), None) => {
                    let name = env.as_str(TOKEN)?;
                    if !is_env_name(name) {
                        return Err(ConfigError::new(ConfigErrorKind::InvalidValue, TOKEN));
                    }
                    TokenReference::Env(name.to_owned())
                }
                (None, Some(file)) => {
                    let path = file.as_str(TOKEN)?;
                    if !is_absolute_path(path) {
                        return Err(ConfigError::new(ConfigErrorKind::InvalidValue, TOKEN));
                    }
                    TokenReference::File(path.into())
                }
                // Both or neither source.
                _ => {
                    return Err(ConfigError::new(ConfigErrorKind::InvalidCombination, TOKEN));
                }
            };
            Ok(LocalAuth::Token(std::sync::Arc::new(resolve(&reference)?)))
        }
        _ => Err(ConfigError::new(ConfigErrorKind::InvalidValue, MODE)),
    }
}

/// `[A-Z_][A-Z0-9_]{0,63}`.
fn is_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| b.is_ascii_uppercase() || *b == b'_' || (i > 0 && b.is_ascii_digit()))
}

/// An absolute path: leading `/`, no NUL, bounded. No `~`, variable, or URL expansion
/// exists, so the string is used verbatim.
fn is_absolute_path(path: &str) -> bool {
    path.starts_with('/') && path.len() <= 4096 && !path.contains('\0')
}

fn parse_upstream(obj: &Obj<'_>) -> Result<UpstreamAuthority, ConfigError> {
    obj.only(&["provider"])?;
    let name = obj
        .require("provider", "deployment.upstream.provider")?
        .as_str("deployment.upstream.provider")?;
    let provider = Provider::from_name(name).ok_or(ConfigError::new(
        ConfigErrorKind::InvalidValue,
        "deployment.upstream.provider",
    ))?;
    Ok(UpstreamAuthority::new(provider))
}

fn parse_content(obj: &Obj<'_>) -> Result<ContentPolicy, ConfigError> {
    obj.only(&["profile", "pii", "on_warn", "max_findings"])?;
    let name = obj
        .require("profile", "content.profile")?
        .as_str("content.profile")?;
    let profile = core_bridge::parse_profile(name)
        .map_err(|_| ConfigError::new(ConfigErrorKind::InvalidValue, "content.profile"))?;
    let mut policy = ContentPolicy::new(profile);
    if let Some(value) = obj.get("pii") {
        let Json::Array(items) = value else {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidType,
                "content.pii",
            ));
        };
        if items.len() > MAX_PII_SELECTORS {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                "content.pii",
            ));
        }
        let mut selectors = Vec::with_capacity(items.len());
        for item in items {
            let selector = item.as_str("content.pii")?;
            if selector.is_empty() || selector.len() > MAX_PII_SELECTOR_BYTES {
                return Err(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    "content.pii",
                ));
            }
            selectors.push(selector.to_owned());
        }
        // The pinned core's own parser decides what is supported (`pii:kr` is not).
        let refs: Vec<&str> = selectors.iter().map(String::as_str).collect();
        core_bridge::parse_pii(&refs)
            .map_err(|_| ConfigError::new(ConfigErrorKind::InvalidValue, "content.pii"))?;
        policy = policy.with_pii(selectors);
    }
    if let Some(value) = obj.get("on_warn") {
        let on_warn = match value.as_str("content.on_warn")? {
            "reject" => OnWarn::Reject,
            "forward" => OnWarn::Forward,
            _ => {
                return Err(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    "content.on_warn",
                ));
            }
        };
        policy = policy.with_on_warn(on_warn);
    }
    if let Some(value) = obj.get("max_findings") {
        policy = policy.with_max_findings(value.as_u32_in(
            "content.max_findings",
            1,
            MAX_FINDINGS_CEILING,
        )?);
    }
    // The whole profile + PII activation must build in the pinned core now, so an
    // unbuildable combination stops the process at startup instead of failing requests.
    let refs: Vec<&str> = policy.pii().iter().map(String::as_str).collect();
    core_bridge::validate_activation(policy.profile(), &refs)
        .map_err(|_| ConfigError::new(ConfigErrorKind::InvalidValue, "content"))?;
    Ok(policy)
}

fn parse_resources(obj: &Obj<'_>) -> Result<ResourcePolicy, ConfigError> {
    obj.only(&["capacity", "limits"])?;
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
    let capacity = CapacityPlan::new(
        n("receipt", "resources.capacity.receipt")?,
        n("memory_units", "resources.capacity.memory_units")?,
        n("inspection", "resources.capacity.inspection")?,
        n("upstream", "resources.capacity.upstream")?,
        n("stream", "resources.capacity.stream")?,
    );
    let limits = match obj.get("limits") {
        None => RequestLimits::provisional(),
        Some(value) => parse_limits(&Obj::new(value, "resources.limits")?)?,
    };
    // The aggregate stream buffer bound is the product of the two configured numbers; it
    // must itself be finite and reviewable.
    let stream_total = u64::from(capacity.stream_permits().get())
        .saturating_mul(u64::from(limits.stream_buffer_bytes));
    if stream_total > MAX_STREAM_BUFFER_TOTAL {
        return Err(ConfigError::new(
            ConfigErrorKind::InvalidCombination,
            "resources.limits.stream_buffer_bytes",
        ));
    }
    Ok(ResourcePolicy::new(capacity).with_limits(limits))
}

/// Ceiling that keeps every body-derived product computable in `u32`/`usize`.
const MAX_BODY_CEILING: u32 = 16 * 1024 * 1024;
/// Ceiling for the buffered provider response body (#20).
const MAX_RESPONSE_CEILING: u32 = 64 * 1024 * 1024;
/// Ceiling for the stream idle and lifetime deadlines (#21): one hour. Infinite streams
/// are a non-goal.
const MAX_STREAM_LIFETIME_MS: u32 = 3_600_000;
/// Ceiling for the accepted-connection bound (#40): the size of the per-process file
/// descriptor space a deployment can reasonably be given.
const MAX_CONNECTIONS_CEILING: u32 = 65_536;
/// Ceiling for the per-stream relay buffer (#21).
const MAX_STREAM_BUFFER_CEILING: u32 = 16 * 1024 * 1024;
/// Ceiling for `resources.capacity.stream` times `stream_buffer_bytes` (#21): the most
/// memory streamed provider bytes may be configured to hold in the relay in aggregate.
const MAX_STREAM_BUFFER_TOTAL: u64 = 4 * 1024 * 1024 * 1024;

/// Optional `resources.limits`. Absent fields keep the provisional value
/// ([`RequestLimits::provisional`]); present fields must be within their ceilings.
fn parse_limits(obj: &Obj<'_>) -> Result<RequestLimits, ConfigError> {
    obj.only(&[
        "max_body_bytes",
        "max_depth",
        "max_nodes",
        "max_string_bytes",
        "max_messages",
        "admission_wait_ms",
        "admission_queue",
        "body_deadline_ms",
        "upstream_connect_ms",
        "upstream_header_ms",
        "upstream_total_ms",
        "max_response_header_bytes",
        "max_response_body_bytes",
        "shutdown_drain_ms",
        "stream_idle_ms",
        "stream_lifetime_ms",
        "stream_write_stall_ms",
        "stream_buffer_bytes",
        "max_connections",
    ])?;
    let mut limits = RequestLimits::provisional();
    let read = |key: &str, loc: &'static str, lo: u32, hi: u32, slot: &mut u32| {
        if let Some(value) = obj.get(key) {
            *slot = value.as_u32_in(loc, lo, hi)?;
        }
        Ok::<(), ConfigError>(())
    };
    read(
        "max_body_bytes",
        "resources.limits.max_body_bytes",
        1,
        MAX_BODY_CEILING,
        &mut limits.max_body_bytes,
    )?;
    read(
        "max_depth",
        "resources.limits.max_depth",
        1,
        64,
        &mut limits.max_depth,
    )?;
    read(
        "max_nodes",
        "resources.limits.max_nodes",
        1,
        1_048_576,
        &mut limits.max_nodes,
    )?;
    read(
        "max_string_bytes",
        "resources.limits.max_string_bytes",
        1,
        MAX_BODY_CEILING,
        &mut limits.max_string_bytes,
    )?;
    read(
        "max_messages",
        "resources.limits.max_messages",
        1,
        4096,
        &mut limits.max_messages,
    )?;
    read(
        "admission_wait_ms",
        "resources.limits.admission_wait_ms",
        0,
        60_000,
        &mut limits.admission_wait_ms,
    )?;
    read(
        "admission_queue",
        "resources.limits.admission_queue",
        0,
        1024,
        &mut limits.admission_queue,
    )?;
    read(
        "body_deadline_ms",
        "resources.limits.body_deadline_ms",
        1,
        300_000,
        &mut limits.body_deadline_ms,
    )?;
    read(
        "upstream_connect_ms",
        "resources.limits.upstream_connect_ms",
        1,
        60_000,
        &mut limits.upstream_connect_ms,
    )?;
    read(
        "upstream_header_ms",
        "resources.limits.upstream_header_ms",
        1,
        3_600_000,
        &mut limits.upstream_header_ms,
    )?;
    read(
        "upstream_total_ms",
        "resources.limits.upstream_total_ms",
        1,
        3_600_000,
        &mut limits.upstream_total_ms,
    )?;
    read(
        "max_response_header_bytes",
        "resources.limits.max_response_header_bytes",
        1,
        262_144,
        &mut limits.max_response_header_bytes,
    )?;
    read(
        "max_response_body_bytes",
        "resources.limits.max_response_body_bytes",
        1,
        MAX_RESPONSE_CEILING,
        &mut limits.max_response_body_bytes,
    )?;
    read(
        "shutdown_drain_ms",
        "resources.limits.shutdown_drain_ms",
        0,
        600_000,
        &mut limits.shutdown_drain_ms,
    )?;
    read(
        "stream_idle_ms",
        "resources.limits.stream_idle_ms",
        1,
        MAX_STREAM_LIFETIME_MS,
        &mut limits.stream_idle_ms,
    )?;
    read(
        "stream_lifetime_ms",
        "resources.limits.stream_lifetime_ms",
        1,
        MAX_STREAM_LIFETIME_MS,
        &mut limits.stream_lifetime_ms,
    )?;
    read(
        "stream_write_stall_ms",
        "resources.limits.stream_write_stall_ms",
        1,
        600_000,
        &mut limits.stream_write_stall_ms,
    )?;
    read(
        "stream_buffer_bytes",
        "resources.limits.stream_buffer_bytes",
        1,
        MAX_STREAM_BUFFER_CEILING,
        &mut limits.stream_buffer_bytes,
    )?;
    read(
        "max_connections",
        "resources.limits.max_connections",
        1,
        MAX_CONNECTIONS_CEILING,
        &mut limits.max_connections,
    )?;
    // The idle deadline is part of the stream's lifetime: an idle deadline beyond the
    // lifetime could never fire.
    if limits.stream_idle_ms > limits.stream_lifetime_ms {
        return Err(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            "resources.limits.stream_idle_ms",
        ));
    }
    // The response-header deadline is part of the total: a header deadline beyond the total
    // could never fire.
    if limits.upstream_header_ms > limits.upstream_total_ms {
        return Err(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            "resources.limits.upstream_header_ms",
        ));
    }
    // A string cannot exceed the body it came from, so `max_string_bytes` above
    // `max_body_bytes` is harmless and needs no cross-field rule.
    Ok(limits)
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
    fn as_u32_in(&self, loc: &'static str, lo: u32, hi: u32) -> Result<u32, ConfigError>;
}

impl JsonExt for Json {
    fn as_str(&self, loc: &'static str) -> Result<&str, ConfigError> {
        match self {
            Self::String(s) => Ok(s),
            _ => Err(ConfigError::new(ConfigErrorKind::InvalidType, loc)),
        }
    }

    fn as_u32_in(&self, loc: &'static str, lo: u32, hi: u32) -> Result<u32, ConfigError> {
        match self {
            Self::Number(n) => n
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v >= lo && *v <= hi)
                .ok_or(ConfigError::new(ConfigErrorKind::InvalidValue, loc)),
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

    fn with_content(content: &str) -> String {
        VALID.replace(r#"{"profile": "common"}"#, content)
    }

    #[test]
    fn content_policy_defaults_fail_closed() {
        let plan = parse(VALID.as_bytes()).expect("valid");
        assert_eq!(plan.content().on_warn(), OnWarn::Reject);
        assert!(plan.content().pii().is_empty());
        assert_eq!(plan.content().max_findings(), DEFAULT_MAX_FINDINGS);
    }

    #[test]
    fn content_policy_fields_are_validated_strictly() {
        let plan = parse(
            with_content(
                r#"{"profile":"full","pii":["pii:family:global:email"],"on_warn":"forward","max_findings":7}"#,
            )
            .as_bytes(),
        )
        .expect("valid");
        assert_eq!(plan.content().on_warn(), OnWarn::Forward);
        assert_eq!(plan.content().pii(), ["pii:family:global:email"]);
        assert_eq!(plan.content().max_findings(), 7);
        for bad in [
            r#"{"profile":"full","on_warn":"allow"}"#,
            r#"{"profile":"full","on_warn":true}"#,
            r#"{"profile":"full","on_warn":"Reject"}"#,
            r#"{"profile":"full","pii":"pii"}"#,
            r#"{"profile":"full","pii":[1]}"#,
            r#"{"profile":"full","pii":[""]}"#,
            r#"{"profile":"full","pii":["pii:kr"]}"#,
            r#"{"profile":"full","pii":["no-such-selector"]}"#,
            r#"{"profile":"full","max_findings":0}"#,
            r#"{"profile":"full","max_findings":50001}"#,
            r#"{"profile":"full","max_findings":"7"}"#,
            r#"{"profile":"full","unknown":1}"#,
        ] {
            assert!(parse(with_content(bad).as_bytes()).is_err(), "{bad}");
        }
        let many = format!(
            r#"{{"profile":"full","pii":[{}]}}"#,
            vec![r#""pii:family:global:email""#; MAX_PII_SELECTORS + 1].join(",")
        );
        assert!(parse(with_content(&many).as_bytes()).is_err());
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
    fn limits_are_optional_provisional_and_bounded() {
        let plan = parse(VALID.as_bytes()).expect("valid");
        assert_eq!(*plan.resources().limits(), RequestLimits::provisional());

        let with = |limits: &str| {
            VALID.replace(
                "\"stream\": 1\n        }}",
                &format!("\"stream\": 1\n        }}, \"limits\": {limits}}}"),
            )
        };
        let plan = parse(with(r#"{"max_body_bytes": 2048, "admission_wait_ms": 0}"#).as_bytes())
            .expect("valid limits");
        assert_eq!(plan.resources().limits().max_body_bytes, 2048);
        // The connection bound (#40) is finite by default and bounded when set.
        assert_eq!(RequestLimits::provisional().max_connections, 256);
        let capped = parse(with(r#"{"max_connections": 65536}"#).as_bytes()).expect("ceiling");
        assert_eq!(capped.resources().limits().max_connections, 65_536);
        assert_eq!(plan.resources().limits().admission_wait_ms, 0);
        let plan = parse(
            with(r#"{"upstream_header_ms": 1000, "upstream_total_ms": 1000, "shutdown_drain_ms": 0}"#)
                .as_bytes(),
        )
        .expect("equal header and total deadlines are allowed");
        assert_eq!(plan.resources().limits().shutdown_drain_ms, 0);
        assert_eq!(
            plan.resources().limits().max_response_body_bytes,
            RequestLimits::provisional().max_response_body_bytes
        );
        assert_eq!(
            plan.resources().limits().max_depth,
            RequestLimits::provisional().max_depth
        );

        for bad in [
            r#"{"max_body_bytes": 0}"#,
            r#"{"max_body_bytes": 16777217}"#,
            r#"{"max_depth": 65}"#,
            r#"{"max_nodes": 0}"#,
            r#"{"max_messages": 4097}"#,
            r#"{"body_deadline_ms": 0}"#,
            r#"{"admission_wait_ms": 60001}"#,
            r#"{"admission_queue": -1}"#,
            r#"{"upstream_connect_ms": 0}"#,
            r#"{"upstream_connect_ms": 60001}"#,
            r#"{"upstream_total_ms": 3600001}"#,
            r#"{"max_response_header_bytes": 0}"#,
            r#"{"max_response_body_bytes": 67108865}"#,
            r#"{"shutdown_drain_ms": 600001}"#,
            r#"{"stream_idle_ms": 0}"#,
            r#"{"stream_idle_ms": 3600001}"#,
            r#"{"stream_lifetime_ms": 3600001}"#,
            r#"{"stream_write_stall_ms": 0}"#,
            r#"{"stream_write_stall_ms": 600001}"#,
            r#"{"stream_buffer_bytes": 0}"#,
            r#"{"stream_buffer_bytes": 16777217}"#,
            r#"{"max_connections": 0}"#,
            r#"{"max_connections": 65537}"#,
            r#"{"max_connections": "8"}"#,
            // The idle deadline is part of the lifetime.
            r#"{"stream_idle_ms": 2000, "stream_lifetime_ms": 1000}"#,
            // The header deadline is part of the total.
            r#"{"upstream_header_ms": 2000, "upstream_total_ms": 1000}"#,
            r#"{"max_depth": "8"}"#,
            r#"{"SYNTH_KEY": 1}"#,
            "[]",
        ] {
            assert!(parse(with(bad).as_bytes()).is_err(), "{bad}");
        }
        assert_eq!(
            err(&with(r#"{"SYNTH_KEY": 1}"#)).kind(),
            ConfigErrorKind::UnknownField
        );
    }

    #[test]
    fn stream_limits_are_provisional_bounded_and_composed_with_the_stream_capacity() {
        let defaults = RequestLimits::provisional();
        assert_eq!(
            (
                defaults.stream_idle_ms,
                defaults.stream_lifetime_ms,
                defaults.stream_write_stall_ms,
                defaults.stream_buffer_bytes
            ),
            (120_000, 900_000, 30_000, 1_048_576)
        );
        let with = |doc: &str, limits: &str| {
            doc.replacen(
                "\n        }}",
                &format!("\n        }}, \"limits\": {limits}}}"),
                1,
            )
        };
        let plan = parse(
            with(
                VALID,
                r#"{"stream_idle_ms": 50, "stream_lifetime_ms": 50, "stream_write_stall_ms": 7, "stream_buffer_bytes": 4096}"#,
            )
            .as_bytes(),
        )
        .expect("valid stream limits");
        let l = plan.resources().limits();
        assert_eq!(
            (
                l.stream_idle_ms,
                l.stream_lifetime_ms,
                l.stream_write_stall_ms,
                l.stream_buffer_bytes
            ),
            (50, 50, 7, 4096)
        );
        // Capacity times buffer is itself bounded: 5000 streams of 1 MiB is over 4 GiB.
        let big = VALID.replace("\"stream\": 1", "\"stream\": 5000");
        assert_eq!(err(&big).kind(), ConfigErrorKind::InvalidCombination);
        assert!(
            parse(with(&big, r#"{"stream_buffer_bytes": 65536}"#).as_bytes()).is_ok(),
            "a smaller buffer brings the product under the ceiling"
        );
    }

    #[test]
    fn non_loopback_requires_explicit_acknowledgement() {
        let open = VALID.replace("127.0.0.1:0", "0.0.0.0:0");
        assert_eq!(err(&open).kind(), ConfigErrorKind::InvalidCombination);
        let acked = open.replace(
            "\"address\": \"0.0.0.0:0\"",
            "\"address\": \"0.0.0.0:0\", \"allow_non_loopback\": true",
        );
        // An acknowledgement alone is not enough: a token is required too (#63).
        let e = err(&acked);
        assert_eq!(e.kind(), ConfigErrorKind::InvalidCombination);
        assert_eq!(e.location(), "deployment.local_auth.mode");
        let with_token = acked.replace(
            "\"listener\":",
            "\"local_auth\": {\"mode\": \"token\", \"token\": {\"env\": \"SYNTH_TOKEN\"}}, \"listener\":",
        );
        let plan = parse_with(with_token.as_bytes(), &synthetic_resolver).expect("acknowledged");
        assert!(plan.deployment().listener().non_loopback_acknowledged());
        assert!(plan.deployment().local_auth().is_enforced());
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
    fn upstream_is_optional_and_selects_only_a_reviewed_provider() {
        let plan = parse(VALID.as_bytes()).expect("valid");
        assert!(plan.deployment().upstream().is_none());
        let with = VALID.replace(
            "\"listener\":",
            "\"upstream\": {\"provider\": \"openai\"}, \"listener\":",
        );
        let plan = parse(with.as_bytes()).expect("valid");
        assert_eq!(
            plan.deployment()
                .upstream()
                .map(UpstreamAuthority::provider),
            Some(Provider::OpenAi)
        );
    }

    #[test]
    fn upstream_rejects_destination_and_tls_fields_and_unknown_providers() {
        let base = VALID.replace("\"listener\":", "\"upstream\": {UP}, \"listener\":");
        for up in [
            r#"{"provider": "openai", "origin": "https://example.test"}"#,
            r#"{"provider": "openai", "url": "https://example.test"}"#,
            r#"{"provider": "openai", "host": "example.test"}"#,
            r#"{"provider": "openai", "port": 443}"#,
            r#"{"provider": "openai", "insecure": true}"#,
            r#"{"provider": "openai", "allow_http": true}"#,
            r#"{"provider": "openai", "proxy": "http://127.0.0.1:1"}"#,
            r#"{"provider": "openai", "test_upstream": "http://127.0.0.1:1"}"#,
            r#"{"provider": "openai", "follow_redirects": true}"#,
        ] {
            let e = err(&base.replace("{UP}", up));
            assert_eq!(e.kind(), ConfigErrorKind::UnknownField, "{up}");
            assert_eq!(e.location(), "deployment.upstream");
        }
        for (up, kind) in [
            (r"{}", ConfigErrorKind::MissingField),
            (r#"{"provider": "OpenAI"}"#, ConfigErrorKind::InvalidValue),
            (
                r#"{"provider": "https://api.openai.com"}"#,
                ConfigErrorKind::InvalidValue,
            ),
            (r#"{"provider": "evil"}"#, ConfigErrorKind::InvalidValue),
            (r#"{"provider": 1}"#, ConfigErrorKind::InvalidType),
            (r#""openai""#, ConfigErrorKind::InvalidType),
        ] {
            assert_eq!(err(&base.replace("{UP}", up)).kind(), kind, "{up}");
        }
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

    const SYNTHETIC_TOKEN: &str = "SYNTH-local-token-0123456789-abcdefghij";

    fn synthetic_resolver(_: &TokenReference) -> Result<LocalToken, ConfigError> {
        LocalToken::from_bytes(SYNTHETIC_TOKEN.as_bytes())
    }

    fn with_local_auth(object: &str) -> String {
        VALID.replace(
            "\"listener\":",
            &format!("\"local_auth\": {object}, \"listener\":"),
        )
    }

    #[test]
    fn local_auth_absent_means_disabled_and_explicit_disabled_is_accepted() {
        let plan = parse(VALID.as_bytes()).expect("valid");
        assert!(!plan.deployment().local_auth().is_enforced());
        let plan = parse(with_local_auth(r#"{"mode": "disabled"}"#).as_bytes()).expect("valid");
        assert!(!plan.deployment().local_auth().is_enforced());
    }

    #[test]
    fn local_auth_token_references_are_validated_before_any_resolution() {
        let resolved = std::cell::Cell::new(0_u32);
        let counting = |r: &TokenReference| {
            resolved.set(resolved.get() + 1);
            synthetic_resolver(r)
        };
        let run = |obj: &str| parse_with(with_local_auth(obj).as_bytes(), &counting);
        for (obj, kind, loc) in [
            (
                r#"{"mode": "token"}"#,
                ConfigErrorKind::MissingField,
                "deployment.local_auth.token",
            ),
            (
                r#"{"token": {"env": "SYNTH_T"}}"#,
                ConfigErrorKind::MissingField,
                "deployment.local_auth.mode",
            ),
            (
                r#"{"mode": "disabled", "token": {"env": "SYNTH_T"}}"#,
                ConfigErrorKind::InvalidCombination,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "token", "token": {}}"#,
                ConfigErrorKind::InvalidCombination,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "token", "token": {"env": "SYNTH_T", "file": "/x"}}"#,
                ConfigErrorKind::InvalidCombination,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "token", "token": {"value": "SYNTH"}}"#,
                ConfigErrorKind::UnknownField,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "token", "token": {"env": "lower"}}"#,
                ConfigErrorKind::InvalidValue,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "token", "token": {"env": "1BAD"}}"#,
                ConfigErrorKind::InvalidValue,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "token", "token": {"env": "A_VERY_LONG_NAME_OVER_SIXTY_FOUR_CHARACTERS_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#,
                ConfigErrorKind::InvalidValue,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "token", "token": {"file": "relative/path"}}"#,
                ConfigErrorKind::InvalidValue,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "token", "token": {"file": "~/token"}}"#,
                ConfigErrorKind::InvalidValue,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "token", "token": {"file": 1}}"#,
                ConfigErrorKind::InvalidType,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "token", "token": "SYNTH"}"#,
                ConfigErrorKind::InvalidType,
                "deployment.local_auth.token",
            ),
            (
                r#"{"mode": "Token", "token": {"env": "SYNTH_T"}}"#,
                ConfigErrorKind::InvalidValue,
                "deployment.local_auth.mode",
            ),
            (
                r#"{"mode": true}"#,
                ConfigErrorKind::InvalidType,
                "deployment.local_auth.mode",
            ),
            (
                r#"{"mode": "disabled", "extra": 1}"#,
                ConfigErrorKind::UnknownField,
                "deployment.local_auth",
            ),
            ("[]", ConfigErrorKind::InvalidType, "deployment.local_auth"),
        ] {
            let e = run(obj).expect_err(obj);
            assert_eq!((e.kind(), e.location()), (kind, loc), "{obj}");
            assert!(!e.to_string().contains("SYNTH"), "{obj}");
        }
        assert_eq!(resolved.get(), 0, "no reference resolved for a bad shape");
        for ok in [
            r#"{"mode": "token", "token": {"env": "SYNTH_T"}}"#,
            r#"{"mode": "token", "token": {"file": "/run/secrets/synthetic-token"}}"#,
        ] {
            let plan = run(ok).expect(ok);
            assert!(plan.deployment().local_auth().is_enforced());
        }
        assert_eq!(resolved.get(), 2);
    }

    #[test]
    fn local_auth_resolution_failures_are_static_and_do_not_echo_the_reference() {
        let failing = |_: &TokenReference| {
            Err::<LocalToken, _>(ConfigError::new(
                ConfigErrorKind::Unreadable,
                "deployment.local_auth.token",
            ))
        };
        let doc = with_local_auth(
            r#"{"mode": "token", "token": {"file": "/run/secrets/SYNTH-secret-path"}}"#,
        );
        let e = parse_with(doc.as_bytes(), &failing).expect_err("unreadable");
        assert_eq!(
            e.to_string(),
            "invalid_config: unreadable at deployment.local_auth.token"
        );
        // The real resolver fails the same way for a missing file and an unset variable.
        for obj in [
            r#"{"mode": "token", "token": {"file": "/nonexistent/SYNTH-secret-path"}}"#,
            r#"{"mode": "token", "token": {"env": "SYNTH_UNSET_TOKEN_VARIABLE_63"}}"#,
        ] {
            let e = parse(with_local_auth(obj).as_bytes()).expect_err(obj);
            assert_eq!(
                e.to_string(),
                "invalid_config: unreadable at deployment.local_auth.token"
            );
        }
    }

    #[test]
    fn local_auth_cannot_be_set_from_content_or_resources() {
        for doc in [
            VALID.replace(
                "{\"profile\": \"common\"}",
                "{\"profile\": \"common\", \"local_auth\": {\"mode\": \"disabled\"}}",
            ),
            VALID.replace("\"stream\": 1", "\"stream\": 1, \"local_auth\": 1"),
        ] {
            assert_eq!(err(&doc).kind(), ConfigErrorKind::UnknownField);
        }
    }
}
