use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use jose_jwk::jose_b64;
use postgres_client::config::SslMode;
use rand::rngs::OsRng;
use rustls::pki_types::{DnsName, ServerName};
use tokio::net::{TcpStream, lookup_host};
use tokio_rustls::TlsConnector;
use tracing::field::display;
use tracing::{debug, info};

use super::AsyncRW;
use super::conn_pool::poll_client;
use super::conn_pool_lib::{Client, ConnInfo, EndpointConnPool, GlobalConnPool};
use super::http_conn_pool::{self, HttpConnPool, Send, poll_http2_client};
use super::local_conn_pool::{self, EXT_NAME, EXT_SCHEMA, EXT_VERSION, LocalConnPool};
use crate::auth::backend::local::StaticAuthRules;
use crate::auth::backend::{ComputeCredentialKeys, ComputeCredentials, ComputeUserInfo};
use crate::auth::{self, AuthError};
use crate::compute_ctl::{
    ComputeCtlError, ExtensionInstallRequest, Privilege, SetRoleGrantsRequest,
};
use crate::config::{ComputeConfig, ProxyConfig};
use crate::context::RequestContext;
use crate::control_plane::CachedNodeInfo;
use crate::control_plane::client::ApiLockError;
use crate::control_plane::errors::{GetAuthInfoError, WakeComputeError};
use crate::control_plane::locks::ApiLocks;
use crate::error::{ErrorKind, ReportableError, UserFacingError};
use crate::intern::EndpointIdInt;
use crate::proxy::connect_compute::ConnectMechanism;
use crate::proxy::retry::{CouldRetry, ShouldRetryWakeCompute};
use crate::rate_limiter::EndpointRateLimiter;
use crate::types::{EndpointId, Host, LOCAL_PROXY_SUFFIX};

pub(crate) struct PoolingBackend {
    pub(crate) http_conn_pool: Arc<GlobalConnPool<Send, HttpConnPool<Send>>>,
    pub(crate) local_pool: Arc<LocalConnPool<postgres_client::Client>>,
    pub(crate) pool:
        Arc<GlobalConnPool<postgres_client::Client, EndpointConnPool<postgres_client::Client>>>,

    pub(crate) config: &'static ProxyConfig,
    pub(crate) auth_backend: &'static crate::auth::Backend<'static, ()>,
    pub(crate) endpoint_rate_limiter: Arc<EndpointRateLimiter>,
}

impl PoolingBackend {
    pub(crate) async fn authenticate_with_password(
        &self,
        ctx: &RequestContext,
        user_info: &ComputeUserInfo,
        password: &[u8],
    ) -> Result<ComputeCredentials, AuthError> {
        ctx.set_auth_method(crate::context::AuthMethod::Cleartext);

        let user_info = user_info.clone();
        let backend = self.auth_backend.as_ref().map(|()| user_info.clone());
        let access_control = backend.get_endpoint_access_control(ctx).await?;
        access_control.check(
            ctx,
            self.config.authentication_config.ip_allowlist_check_enabled,
            self.config.authentication_config.is_vpc_acccess_proxy,
        )?;

        access_control.connection_attempt_rate_limit(
            ctx,
            &user_info.endpoint,
            &self.endpoint_rate_limiter,
        )?;

        let role_access = backend.get_role_secret(ctx).await?;
        let Some(secret) = role_access.secret else {
            // If we don't have an authentication secret, for the http flow we can just return an error.
            info!("authentication info not found");
            return Err(AuthError::password_failed(&*user_info.user));
        };

        let ep = EndpointIdInt::from(&user_info.endpoint);
        let auth_outcome = crate::auth::validate_password_and_exchange(
            &self.config.authentication_config.thread_pool,
            ep,
            password,
            secret,
        )
        .await?;
        let res = match auth_outcome {
            crate::sasl::Outcome::Success(key) => {
                info!("user successfully authenticated");
                Ok(key)
            }
            crate::sasl::Outcome::Failure(reason) => {
                info!("auth backend failed with an error: {reason}");
                Err(AuthError::password_failed(&*user_info.user))
            }
        };
        res.map(|key| ComputeCredentials {
            info: user_info,
            keys: key,
        })
    }

    pub(crate) async fn authenticate_with_jwt(
        &self,
        ctx: &RequestContext,
        user_info: &ComputeUserInfo,
        jwt: String,
    ) -> Result<ComputeCredentials, AuthError> {
        ctx.set_auth_method(crate::context::AuthMethod::Jwt);

        match &self.auth_backend {
            crate::auth::Backend::ControlPlane(console, ()) => {
                self.config
                    .authentication_config
                    .jwks_cache
                    .check_jwt(
                        ctx,
                        user_info.endpoint.clone(),
                        &user_info.user,
                        &**console,
                        &jwt,
                    )
                    .await?;

                Ok(ComputeCredentials {
                    info: user_info.clone(),
                    keys: crate::auth::backend::ComputeCredentialKeys::None,
                })
            }
            crate::auth::Backend::Local(_) => {
                let keys = self
                    .config
                    .authentication_config
                    .jwks_cache
                    .check_jwt(
                        ctx,
                        user_info.endpoint.clone(),
                        &user_info.user,
                        &StaticAuthRules,
                        &jwt,
                    )
                    .await?;

                Ok(ComputeCredentials {
                    info: user_info.clone(),
                    keys,
                })
            }
        }
    }

    // Wake up the destination if needed. Code here is a bit involved because
    // we reuse the code from the usual proxy and we need to prepare few structures
    // that this code expects.
    #[tracing::instrument(skip_all, fields(
        pid = tracing::field::Empty,
        compute_id = tracing::field::Empty,
        conn_id = tracing::field::Empty,
    ))]
    pub(crate) async fn connect_to_compute(
        &self,
        ctx: &RequestContext,
        conn_info: ConnInfo,
        keys: ComputeCredentials,
        force_new: bool,
    ) -> Result<Client<postgres_client::Client>, HttpConnError> {
        let maybe_client = if force_new {
            debug!("pool: pool is disabled");
            None
        } else {
            debug!("pool: looking for an existing connection");
            self.pool.get(ctx, &conn_info)?
        };

        if let Some(client) = maybe_client {
            return Ok(client);
        }
        let conn_id = uuid::Uuid::new_v4();
        tracing::Span::current().record("conn_id", display(conn_id));
        info!(%conn_id, "pool: opening a new connection '{conn_info}'");
        let backend = self.auth_backend.as_ref().map(|()| keys.info);
        crate::proxy::connect_compute::connect_to_compute(
            ctx,
            &TokioMechanism {
                conn_id,
                conn_info,
                pool: self.pool.clone(),
                locks: &self.config.connect_compute_locks,
                keys: keys.keys,
            },
            &backend,
            self.config.wake_compute_retry_config,
            &self.config.connect_to_compute,
        )
        .await
    }

    // Wake up the destination if needed
    #[tracing::instrument(skip_all, fields(
        compute_id = tracing::field::Empty,
        conn_id = tracing::field::Empty,
    ))]
    pub(crate) async fn connect_to_local_proxy(
        &self,
        ctx: &RequestContext,
        conn_info: ConnInfo,
    ) -> Result<http_conn_pool::Client<Send>, HttpConnError> {
        debug!("pool: looking for an existing connection");
        if let Ok(Some(client)) = self.http_conn_pool.get(ctx, &conn_info) {
            return Ok(client);
        }

        let conn_id = uuid::Uuid::new_v4();
        tracing::Span::current().record("conn_id", display(conn_id));
        debug!(%conn_id, "pool: opening a new connection '{conn_info}'");
        let backend = self.auth_backend.as_ref().map(|()| ComputeUserInfo {
            user: conn_info.user_info.user.clone(),
            endpoint: EndpointId::from(format!(
                "{}{LOCAL_PROXY_SUFFIX}",
                conn_info.user_info.endpoint.normalize()
            )),
            options: conn_info.user_info.options.clone(),
        });
        crate::proxy::connect_compute::connect_to_compute(
            ctx,
            &HyperMechanism {
                conn_id,
                conn_info,
                pool: self.http_conn_pool.clone(),
                locks: &self.config.connect_compute_locks,
            },
            &backend,
            self.config.wake_compute_retry_config,
            &self.config.connect_to_compute,
        )
        .await
    }

    /// Connect to postgres over localhost.
    ///
    /// We expect postgres to be started here, so we won't do any retries.
    ///
    /// # Panics
    ///
    /// Panics if called with a non-local_proxy backend.
    #[tracing::instrument(skip_all, fields(
        pid = tracing::field::Empty,
        conn_id = tracing::field::Empty,
    ))]
    pub(crate) async fn connect_to_local_postgres(
        &self,
        ctx: &RequestContext,
        conn_info: ConnInfo,
    ) -> Result<Client<postgres_client::Client>, HttpConnError> {
        if let Some(client) = self.local_pool.get(ctx, &conn_info)? {
            return Ok(client);
        }

        let local_backend = match &self.auth_backend {
            auth::Backend::ControlPlane(_, ()) => {
                unreachable!("only local_proxy can connect to local postgres")
            }
            auth::Backend::Local(local) => local,
        };

        if !self.local_pool.initialized(&conn_info) {
            // only install and grant usage one at a time.
            let _permit = local_backend
                .initialize
                .acquire()
                .await
                .expect("semaphore should never be closed");

            // check again for race
            if !self.local_pool.initialized(&conn_info) {
                local_backend
                    .compute_ctl
                    .install_extension(&ExtensionInstallRequest {
                        extension: EXT_NAME,
                        database: conn_info.dbname.clone(),
                        version: EXT_VERSION,
                    })
                    .await?;

                local_backend
                    .compute_ctl
                    .grant_role(&SetRoleGrantsRequest {
                        schema: EXT_SCHEMA,
                        privileges: vec![Privilege::Usage],
                        database: conn_info.dbname.clone(),
                        role: conn_info.user_info.user.clone(),
                    })
                    .await?;

                self.local_pool.set_initialized(&conn_info);
            }
        }

        let conn_id = uuid::Uuid::new_v4();
        tracing::Span::current().record("conn_id", display(conn_id));
        info!(%conn_id, "local_pool: opening a new connection '{conn_info}'");

        let (key, jwk) = create_random_jwk();

        let mut config = local_backend
            .node_info
            .conn_info
            .to_postgres_client_config();
        config
            .user(&conn_info.user_info.user)
            .dbname(&conn_info.dbname)
            .set_param(
                "options",
                &format!(
                    "-c pg_session_jwt.jwk={}",
                    serde_json::to_string(&jwk).expect("serializing jwk to json should not fail")
                ),
            );

        let pause = ctx.latency_timer_pause(crate::metrics::Waiting::Compute);
        let (client, connection) = config.connect(&postgres_client::NoTls).await?;
        drop(pause);

        let pid = client.get_process_id();
        tracing::Span::current().record("pid", pid);

        let mut handle = local_conn_pool::poll_client(
            self.local_pool.clone(),
            ctx,
            conn_info,
            client,
            connection,
            key,
            conn_id,
            local_backend.node_info.aux.clone(),
        );

        {
            let (client, mut discard) = handle.inner();
            debug!("setting up backend session state");

            // initiates the auth session
            if let Err(e) = client.batch_execute("select auth.init();").await {
                discard.discard();
                return Err(e.into());
            }

            info!("backend session state initialized");
        }

        Ok(handle)
    }
}

fn create_random_jwk() -> (SigningKey, jose_jwk::Key) {
    let key = SigningKey::generate(&mut OsRng);

    let jwk = jose_jwk::Key::Okp(jose_jwk::Okp {
        crv: jose_jwk::OkpCurves::Ed25519,
        x: jose_b64::serde::Bytes::from(key.verifying_key().to_bytes().to_vec()),
        d: None,
    });

    (key, jwk)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HttpConnError {
    #[error("pooled connection closed at inconsistent state")]
    ConnectionClosedAbruptly(#[from] tokio::sync::watch::error::SendError<uuid::Uuid>),
    #[error("could not connect to postgres in compute")]
    PostgresConnectionError(#[from] postgres_client::Error),
    #[error("could not connect to local-proxy in compute")]
    LocalProxyConnectionError(#[from] LocalProxyConnError),
    #[error("could not parse JWT payload")]
    JwtPayloadError(serde_json::Error),

    #[error("could not install extension: {0}")]
    ComputeCtl(#[from] ComputeCtlError),
    #[error("could not get auth info")]
    GetAuthInfo(#[from] GetAuthInfoError),
    #[error("user not authenticated")]
    AuthError(#[from] AuthError),
    #[error("wake_compute returned error")]
    WakeCompute(#[from] WakeComputeError),
    #[error("error acquiring resource permit: {0}")]
    TooManyConnectionAttempts(#[from] ApiLockError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LocalProxyConnError {
    #[error("error with connection to local-proxy")]
    Io(#[source] std::io::Error),
    #[error("could not establish h2 connection")]
    H2(#[from] hyper::Error),
}

impl ReportableError for HttpConnError {
    fn get_error_kind(&self) -> ErrorKind {
        match self {
            HttpConnError::ConnectionClosedAbruptly(_) => ErrorKind::Compute,
            HttpConnError::PostgresConnectionError(p) => {
                if p.as_db_error().is_some() {
                    // postgres rejected the connection
                    ErrorKind::Postgres
                } else {
                    // couldn't even reach postgres
                    ErrorKind::Compute
                }
            }
            HttpConnError::LocalProxyConnectionError(_) => ErrorKind::Compute,
            HttpConnError::ComputeCtl(_) => ErrorKind::Service,
            HttpConnError::JwtPayloadError(_) => ErrorKind::User,
            HttpConnError::GetAuthInfo(a) => a.get_error_kind(),
            HttpConnError::AuthError(a) => a.get_error_kind(),
            HttpConnError::WakeCompute(w) => w.get_error_kind(),
            HttpConnError::TooManyConnectionAttempts(w) => w.get_error_kind(),
        }
    }
}

impl UserFacingError for HttpConnError {
    fn to_string_client(&self) -> String {
        match self {
            HttpConnError::ConnectionClosedAbruptly(_) => self.to_string(),
            HttpConnError::PostgresConnectionError(p) => p.to_string(),
            HttpConnError::LocalProxyConnectionError(p) => p.to_string(),
            HttpConnError::ComputeCtl(_) => "could not set up the JWT authorization database extension".to_string(),
            HttpConnError::JwtPayloadError(p) => p.to_string(),
            HttpConnError::GetAuthInfo(c) => c.to_string_client(),
            HttpConnError::AuthError(c) => c.to_string_client(),
            HttpConnError::WakeCompute(c) => c.to_string_client(),
            HttpConnError::TooManyConnectionAttempts(_) => {
                "Failed to acquire permit to connect to the database. Too many database connection attempts are currently ongoing.".to_owned()
            }
        }
    }
}

impl CouldRetry for HttpConnError {
    fn could_retry(&self) -> bool {
        match self {
            HttpConnError::PostgresConnectionError(e) => e.could_retry(),
            HttpConnError::LocalProxyConnectionError(e) => e.could_retry(),
            HttpConnError::ComputeCtl(_) => false,
            HttpConnError::ConnectionClosedAbruptly(_) => false,
            HttpConnError::JwtPayloadError(_) => false,
            HttpConnError::GetAuthInfo(_) => false,
            HttpConnError::AuthError(_) => false,
            HttpConnError::WakeCompute(_) => false,
            HttpConnError::TooManyConnectionAttempts(_) => false,
        }
    }
}
impl ShouldRetryWakeCompute for HttpConnError {
    fn should_retry_wake_compute(&self) -> bool {
        match self {
            HttpConnError::PostgresConnectionError(e) => e.should_retry_wake_compute(),
            // we never checked cache validity
            HttpConnError::TooManyConnectionAttempts(_) => false,
            _ => true,
        }
    }
}

impl ReportableError for LocalProxyConnError {
    fn get_error_kind(&self) -> ErrorKind {
        match self {
            LocalProxyConnError::Io(_) => ErrorKind::Compute,
            LocalProxyConnError::H2(_) => ErrorKind::Compute,
        }
    }
}

impl UserFacingError for LocalProxyConnError {
    fn to_string_client(&self) -> String {
        "Could not establish HTTP connection to the database".to_string()
    }
}

impl CouldRetry for LocalProxyConnError {
    fn could_retry(&self) -> bool {
        match self {
            LocalProxyConnError::Io(_) => false,
            LocalProxyConnError::H2(_) => false,
        }
    }
}
impl ShouldRetryWakeCompute for LocalProxyConnError {
    fn should_retry_wake_compute(&self) -> bool {
        match self {
            LocalProxyConnError::Io(_) => false,
            LocalProxyConnError::H2(_) => false,
        }
    }
}

struct TokioMechanism {
    pool: Arc<GlobalConnPool<postgres_client::Client, EndpointConnPool<postgres_client::Client>>>,
    conn_info: ConnInfo,
    conn_id: uuid::Uuid,
    keys: ComputeCredentialKeys,

    /// connect_to_compute concurrency lock
    locks: &'static ApiLocks<Host>,
}

#[async_trait]
impl ConnectMechanism for TokioMechanism {
    type Connection = Client<postgres_client::Client>;
    type ConnectError = HttpConnError;
    type Error = HttpConnError;

    async fn connect_once(
        &self,
        ctx: &RequestContext,
        node_info: &CachedNodeInfo,
        compute_config: &ComputeConfig,
    ) -> Result<Self::Connection, Self::ConnectError> {
        let permit = self.locks.get_permit(&node_info.conn_info.host).await?;

        let mut config = node_info.conn_info.to_postgres_client_config();
        let config = config
            .user(&self.conn_info.user_info.user)
            .dbname(&self.conn_info.dbname)
            .connect_timeout(compute_config.timeout);

        if let ComputeCredentialKeys::AuthKeys(auth_keys) = self.keys {
            config.auth_keys(auth_keys);
        }

        let pause = ctx.latency_timer_pause(crate::metrics::Waiting::Compute);
        let res = config.connect(compute_config).await;
        drop(pause);
        let (client, connection) = permit.release_result(res)?;

        tracing::Span::current().record("pid", tracing::field::display(client.get_process_id()));
        tracing::Span::current().record(
            "compute_id",
            tracing::field::display(&node_info.aux.compute_id),
        );

        if let Some(query_id) = ctx.get_testodrome_id() {
            info!("latency={}, query_id={}", ctx.get_proxy_latency(), query_id);
        }

        Ok(poll_client(
            self.pool.clone(),
            ctx,
            self.conn_info.clone(),
            client,
            connection,
            self.conn_id,
            node_info.aux.clone(),
        ))
    }
}

struct HyperMechanism {
    pool: Arc<GlobalConnPool<Send, HttpConnPool<Send>>>,
    conn_info: ConnInfo,
    conn_id: uuid::Uuid,

    /// connect_to_compute concurrency lock
    locks: &'static ApiLocks<Host>,
}

#[async_trait]
impl ConnectMechanism for HyperMechanism {
    type Connection = http_conn_pool::Client<Send>;
    type ConnectError = HttpConnError;
    type Error = HttpConnError;

    async fn connect_once(
        &self,
        ctx: &RequestContext,
        node_info: &CachedNodeInfo,
        config: &ComputeConfig,
    ) -> Result<Self::Connection, Self::ConnectError> {
        let host_addr = node_info.conn_info.host_addr;
        let host = &node_info.conn_info.host;
        let permit = self.locks.get_permit(host).await?;

        let pause = ctx.latency_timer_pause(crate::metrics::Waiting::Compute);

        let tls = if node_info.conn_info.ssl_mode == SslMode::Disable {
            None
        } else {
            Some(&config.tls)
        };

        let port = node_info.conn_info.port;
        let res = connect_http2(host_addr, host, port, config.timeout, tls).await;
        drop(pause);
        let (client, connection) = permit.release_result(res)?;

        tracing::Span::current().record(
            "compute_id",
            tracing::field::display(&node_info.aux.compute_id),
        );

        if let Some(query_id) = ctx.get_testodrome_id() {
            info!("latency={}, query_id={}", ctx.get_proxy_latency(), query_id);
        }

        Ok(poll_http2_client(
            self.pool.clone(),
            ctx,
            &self.conn_info,
            client,
            connection,
            self.conn_id,
            node_info.aux.clone(),
        ))
    }
}

async fn connect_http2(
    host_addr: Option<IpAddr>,
    host: &str,
    port: u16,
    timeout: Duration,
    tls: Option<&Arc<rustls::ClientConfig>>,
) -> Result<(http_conn_pool::Send, http_conn_pool::Connect), LocalProxyConnError> {
    let addrs = match host_addr {
        Some(addr) => vec![SocketAddr::new(addr, port)],
        None => lookup_host((host, port))
            .await
            .map_err(LocalProxyConnError::Io)?
            .collect(),
    };
    let mut last_err = None;

    let mut addrs = addrs.into_iter();
    let stream = loop {
        let Some(addr) = addrs.next() else {
            return Err(last_err.unwrap_or_else(|| {
                LocalProxyConnError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "could not resolve any addresses",
                ))
            }));
        };

        match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                stream.set_nodelay(true).map_err(LocalProxyConnError::Io)?;
                break stream;
            }
            Ok(Err(e)) => {
                last_err = Some(LocalProxyConnError::Io(e));
            }
            Err(e) => {
                last_err = Some(LocalProxyConnError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    e,
                )));
            }
        }
    };

    let stream = if let Some(tls) = tls {
        let host = DnsName::try_from(host)
            .map_err(io::Error::other)
            .map_err(LocalProxyConnError::Io)?
            .to_owned();
        let stream = TlsConnector::from(tls.clone())
            .connect(ServerName::DnsName(host), stream)
            .await
            .map_err(LocalProxyConnError::Io)?;
        Box::pin(stream) as AsyncRW
    } else {
        Box::pin(stream) as AsyncRW
    };

    let (client, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .keep_alive_interval(Duration::from_secs(20))
        .keep_alive_while_idle(true)
        .keep_alive_timeout(Duration::from_secs(5))
        .handshake(TokioIo::new(stream))
        .await?;

    Ok((client, connection))
}
