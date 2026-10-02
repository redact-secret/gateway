//! Startup sequence and loopback server with graceful shutdown (ADR 0006).
//!
//! Order: validated plan -> required initialization ([`Services`]) -> bind -> mark ready
//! -> accept. Any failure before the accept loop returns a typed [`StartupError`] and no
//! traffic is ever accepted. The plan is shared by `Arc` and never reread; shared services
//! (the upstream client holder) are built once here, not per request.

use std::fmt;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::admission::Admission;
use crate::boundary::Inspection;
use crate::chat_route::{self, ChatRoute};
use crate::config::{ConfigError, RouteId, RuntimePlan};
use crate::health::{self, HealthState};
use crate::telemetry::{Metrics, SafeCode};
use crate::transport::Upstream;
use crate::transport::destination;
use crate::write_stall::StallListener;

/// How long [`BoundServer::serve`] waits for connections to close after cancelling
/// in-flight requests at the drain deadline.
const CANCEL_GRACE: Duration = Duration::from_secs(1);

/// Safe startup failure. Fixed stage only; no paths, addresses, or OS error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StartupError {
    Config(ConfigError),
    /// Core or transport initialization failed.
    Init,
    /// Signal handlers could not be installed.
    Signals,
    /// The listener could not be bound.
    Bind,
    /// The server stopped abnormally.
    Serve,
}

impl StartupError {
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::Config(_) => SafeCode::InvalidConfig,
            Self::Init | Self::Signals => SafeCode::NotReady,
            Self::Bind | Self::Serve => SafeCode::TransportFailure,
        }
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(e) => write!(f, "{e}"),
            Self::Init => write!(f, "{}: initialization", self.code().as_str()),
            Self::Signals => write!(f, "{}: signal handlers", self.code().as_str()),
            Self::Bind => write!(f, "{}: bind", self.code().as_str()),
            Self::Serve => write!(f, "{}: serve", self.code().as_str()),
        }
    }
}

impl std::error::Error for StartupError {}

impl From<ConfigError> for StartupError {
    fn from(e: ConfigError) -> Self {
        Self::Config(e)
    }
}

/// Shared, startup-built resources. Built once; requests never construct or reconfigure
/// them. Holds no credentials (ADR 0009).
#[derive(Debug)]
pub struct Services {
    pub(crate) admission: Arc<Admission>,
    pub(crate) chat: Arc<ChatRoute>,
    /// How long shutdown drains in-flight requests before cancelling them (ADR 0017).
    pub(crate) drain: Duration,
}

impl Services {
    /// Required initialization: the shared transport client and the core inspection
    /// workers (one registry per worker, built from the plan's profile, optional PII
    /// selection, and limits). Readiness depends on both succeeding.
    ///
    /// # Errors
    /// [`StartupError::Init`] when the transport client or core inspection cannot be built.
    pub fn init(plan: &RuntimePlan) -> Result<Self, StartupError> {
        let metrics = Arc::new(Metrics::new());
        let upstream = Arc::new(
            Upstream::from_plan(plan)
                .map_err(|_| StartupError::Init)?
                .with_metrics(Arc::clone(&metrics)),
        );
        let admission = Arc::new(Admission::new(plan.resources().capacity()));
        let inspection = Arc::new(
            Inspection::start(
                Arc::clone(&admission),
                plan.content(),
                plan.resources().limits(),
                plan.resources().capacity(),
            )
            .map_err(|_| StartupError::Init)?
            .with_metrics(Arc::clone(&metrics)),
        );
        // The route id is the reviewed one from the static table, never from a request.
        let chat = Arc::new(
            ChatRoute::new(
                Arc::clone(&admission),
                *plan.resources().limits(),
                RouteId::new(destination::OPENAI_CHAT_COMPLETIONS_ROUTE),
            )
            .with_inspection(inspection)
            .with_upstream(upstream)
            .with_metrics(metrics),
        );
        Ok(Self {
            admission,
            chat,
            drain: plan.resources().limits().shutdown_drain(),
        })
    }

    /// The shared capacity owner (receipt, memory, inspection, upstream, stream).
    #[must_use]
    pub fn admission(&self) -> Arc<Admission> {
        Arc::clone(&self.admission)
    }

    /// The `POST /v1/chat/completions` admission route.
    #[must_use]
    pub fn chat(&self) -> Arc<ChatRoute> {
        Arc::clone(&self.chat)
    }
}

/// A bound, initialized, not-yet-serving listener.
#[derive(Debug)]
pub struct BoundServer {
    listener: TcpListener,
    state: Arc<HealthState>,
    services: Services,
}

/// Initialize and bind. No request is accepted until [`BoundServer::serve`].
///
/// `init` builds the shared services from the plan; production passes [`Services::init`].
/// Tests may pass a failing or counting initializer.
///
/// # Errors
/// [`StartupError::Init`] / [`StartupError::Bind`].
pub async fn bind(
    plan: Arc<RuntimePlan>,
    init: impl FnOnce(&RuntimePlan) -> Result<Services, StartupError>,
) -> Result<BoundServer, StartupError> {
    let services = init(&plan)?;
    let addr = plan.deployment().listener().addr();
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|_| StartupError::Bind)?;
    let state = Arc::new(HealthState::new(plan));
    state.mark_initialized();
    Ok(BoundServer {
        listener,
        state,
        services,
    })
}

impl BoundServer {
    /// # Errors
    /// [`StartupError::Bind`] if the OS cannot report the bound address.
    pub fn local_addr(&self) -> Result<SocketAddr, StartupError> {
        self.listener.local_addr().map_err(|_| StartupError::Bind)
    }

    /// The chat admission route, for tests that observe capacity.
    #[must_use]
    pub fn chat(&self) -> Arc<ChatRoute> {
        self.services.chat()
    }

    #[must_use]
    pub fn health(&self) -> Arc<HealthState> {
        Arc::clone(&self.state)
    }

    /// Serve until `shutdown` completes, then stop accepting, report not ready, and drain
    /// in-flight connections for at most `resources.limits.shutdown_drain_ms`. When the
    /// drain deadline passes, every in-flight request future is cancelled (it answers a
    /// local `503 not_ready`, releases its permits and buffers, and any upstream exchange is
    /// aborted) and `serve` waits only a short fixed grace for connections to close before
    /// returning (ADR 0004, ADR 0017). Bytes already transmitted to a provider cannot be
    /// retracted.
    ///
    /// # Errors
    /// [`StartupError::Serve`] if the server stops abnormally.
    pub async fn serve(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), StartupError> {
        let Self {
            listener,
            state,
            services,
        } = self;
        let chat = services.chat();
        let app = chat_route::mount(health::router(Arc::clone(&state)), Arc::clone(&chat));
        state.set_accepting(true);
        let on_shutdown = Arc::clone(&state);
        let draining = Arc::new(Notify::new());
        let started = Arc::clone(&draining);
        // Every accepted connection enforces the write-stall deadline (#21).
        let listener = StallListener::new(listener, chat.limits().stream_write_stall());
        let server = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.await;
                on_shutdown.set_accepting(false);
                started.notify_one();
            })
            .into_future();
        tokio::pin!(server);
        let result = tokio::select! {
            result = &mut server => result,
            () = async {
                draining.notified().await;
                tokio::time::sleep(services.drain).await;
            } => {
                chat.cancel_in_flight();
                // Cancelled requests answer and close promptly; do not wait on idle or
                // stuck peers beyond the grace.
                tokio::time::timeout(CANCEL_GRACE, &mut server)
                    .await
                    .unwrap_or(Ok(()))
            }
        }
        .map_err(|_| StartupError::Serve);
        state.set_accepting(false);
        drop(services);
        result
    }
}

/// Installed termination-signal handlers (SIGINT and SIGTERM on Unix, Ctrl-C elsewhere).
/// Install before binding so a signal during startup is not lost.
#[derive(Debug)]
pub struct ShutdownSignal {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
}

impl ShutdownSignal {
    /// # Errors
    /// [`StartupError::Signals`] if handlers cannot be registered.
    #[cfg(unix)]
    pub fn install() -> Result<Self, StartupError> {
        use tokio::signal::unix::{SignalKind, signal};
        let term = signal(SignalKind::terminate()).map_err(|_| StartupError::Signals)?;
        let int = signal(SignalKind::interrupt()).map_err(|_| StartupError::Signals)?;
        Ok(Self { term, int })
    }

    /// # Errors
    /// Never on this platform.
    #[cfg(not(unix))]
    pub fn install() -> Result<Self, StartupError> {
        Ok(Self {})
    }

    /// Resolve when a termination signal arrives.
    #[cfg(unix)]
    pub async fn recv(mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.int.recv() => {}
        }
    }

    /// Resolve when Ctrl-C arrives.
    #[cfg(not(unix))]
    pub async fn recv(self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}
