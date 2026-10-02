//! Static configuration and the immutable `RuntimePlan` (ADR 0006).
//!
//! Scaffold: no parsing or validation exists yet (#4 owns it). The plan has three separate
//! authorities so a content-policy change cannot alter deployment authority. Fields have
//! no numeric defaults: callers must supply every value explicitly.

use crate::admission::CapacityPlan;

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

/// Deployment authority: listener, upstream origins, TLS rules, credential requirements.
/// Placeholder until #4/#23-#25.
#[derive(Debug)]
#[non_exhaustive]
pub struct DeploymentAuthority {}

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

impl DeploymentAuthority {
    /// Placeholder constructor; #4 replaces it with validated construction.
    #[must_use]
    pub const fn placeholder() -> Self {
        Self {}
    }
}
