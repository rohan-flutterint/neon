//! Postgres protocol proxy/router.
//!
//! This service listens psql port and can check auth via external service
//! (control plane API in our case) and can create new databases and accounts
//! in somewhat transparent manner (again via communication with control plane API).

mod auth;
mod cancellation;
mod compute;
mod config;
mod console;
mod error;
mod http;
mod metrics;
mod mgmt;
mod parse;
mod proxy;
mod sasl;
mod scram;
mod stream;
mod url;
mod waiters;

use ::metrics::set_build_info_metric;
use anyhow::{bail, Context};
use clap::{self, Arg};
use config::ProxyConfig;
use futures::FutureExt;
use std::{borrow::Cow, future::Future, net::SocketAddr};
use tokio::{net::TcpListener, task::JoinError};
use tracing::{info, info_span, Instrument};
use utils::project_git_version;
use utils::sentry_init::{init_sentry, release_name};

project_git_version!(GIT_VERSION);

/// Flattens `Result<Result<T>>` into `Result<T>`.
async fn flatten_err(
    f: impl Future<Output = Result<anyhow::Result<()>, JoinError>>,
) -> anyhow::Result<()> {
    f.map(|r| r.context("join error").and_then(|x| x)).await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(atty::is(atty::Stream::Stdout))
        .with_target(false)
        .init();

    // initialize sentry if SENTRY_DSN is provided
    let _sentry_guard = init_sentry(release_name!(), &[]);

    let arg_matches = cli().get_matches();

    let tls_config = match (
        arg_matches.get_one::<String>("tls-key"),
        arg_matches.get_one::<String>("tls-cert"),
    ) {
        (Some(key_path), Some(cert_path)) => Some(config::configure_tls(key_path, cert_path)?),
        (None, None) => None,
        _ => bail!("either both or neither tls-key and tls-cert must be specified"),
    };

    let proxy_address: SocketAddr = arg_matches.get_one::<String>("proxy").unwrap().parse()?;
    let mgmt_address_str = arg_matches.get_one::<String>("mgmt").unwrap();
    let mgmt_address: Option<SocketAddr> = if !mgmt_address_str.is_empty() {
        Some(mgmt_address_str.parse()?)
    } else {
        None
    };
    let http_address: SocketAddr = arg_matches.get_one::<String>("http").unwrap().parse()?;

    let metric_collection_config = match
    (
        arg_matches.get_one::<String>("metric-collection-endpoint"),
        arg_matches.get_one::<String>("metric-collection-interval"),
    ) {

        (Some(endpoint), Some(interval)) => {
            Some(config::MetricCollectionConfig {
                endpoint: endpoint.parse()?,
                interval: humantime::parse_duration(interval)?,
            })
        }
        (None, None) => None,
        _ => bail!("either both or neither metric-collection-endpoint and metric-collection-interval must be specified"),
    };

    let auth_backend = match arg_matches
        .get_one::<String>("auth-backend")
        .unwrap()
        .as_str()
    {
        "console" => {
            let url = arg_matches
                .get_one::<String>("auth-endpoint")
                .unwrap()
                .parse()?;
            let endpoint = http::Endpoint::new(url, reqwest::Client::new());
            auth::BackendType::Console(Cow::Owned(endpoint), ())
        }
        "postgres" => {
            let url = arg_matches
                .get_one::<String>("auth-endpoint")
                .unwrap()
                .parse()?;
            auth::BackendType::Postgres(Cow::Owned(url), ())
        }
        "link" => {
            let url = arg_matches.get_one::<String>("uri").unwrap().parse()?;
            auth::BackendType::Link(Cow::Owned(url))
        }
        other => bail!("unsupported auth backend: {other}"),
    };

    let config: &ProxyConfig = Box::leak(Box::new(ProxyConfig {
        tls_config,
        auth_backend,
        metric_collection_config,
    }));

    info!("Version: {GIT_VERSION}");
    info!("Authentication backend: {}", config.auth_backend);

    // Check that we can bind to address before further initialization
    info!("Starting http on {http_address}");
    let http_listener = TcpListener::bind(http_address).await?.into_std()?;

    let mgmt_listener = if let Some(mgmt_address) = mgmt_address {
        info!("Starting mgmt on {mgmt_address}");
        Some(TcpListener::bind(mgmt_address).await?.into_std()?)
    } else {
        None
    };

    info!("Starting proxy on {proxy_address}");
    let proxy_listener = TcpListener::bind(proxy_address).await?;

    let mut tasks = vec![
        tokio::spawn(http::server::task_main(http_listener)),
        tokio::spawn(proxy::task_main(config, proxy_listener)),
    ];

    if let Some(mgmt_listener) = mgmt_listener {
        tasks.push(tokio::task::spawn_blocking(move || {
            mgmt::thread_main(mgmt_listener)
        }));
    }

    if let Some(wss_address) = arg_matches.get_one::<String>("wss") {
        let wss_address: SocketAddr = wss_address.parse()?;
        info!("Starting wss on {}", wss_address);
        let wss_listener = TcpListener::bind(wss_address).await?;
        tasks.push(tokio::spawn(http::websocket::task_main(
            wss_listener,
            config,
        )));
    }

    if let Some(metric_collection_config) = &config.metric_collection_config {
        let hostname = hostname::get()?
            .into_string()
            .map_err(|e| anyhow::anyhow!("failed to get hostname {e:?}"))?;

        tasks.push(tokio::spawn(
            metrics::collect_metrics(
                &metric_collection_config.endpoint,
                metric_collection_config.interval,
                hostname,
            )
            .instrument(info_span!("collect_metrics")),
        ));
    }

    let tasks = tasks.into_iter().map(flatten_err);

    set_build_info_metric(GIT_VERSION);
    // This will block until all tasks have completed.
    // Furthermore, the first one to fail will cancel the rest.
    let _: Vec<()> = futures::future::try_join_all(tasks).await?;

    Ok(())
}

fn cli() -> clap::Command {
    clap::Command::new("Neon proxy/router")
        .disable_help_flag(true)
        .version(GIT_VERSION)
        .arg(
            Arg::new("proxy")
                .short('p')
                .long("proxy")
                .help("listen for incoming client connections on ip:port")
                .default_value("127.0.0.1:4432"),
        )
        .arg(
            Arg::new("auth-backend")
                .long("auth-backend")
                .value_parser(["console", "postgres", "link"])
                .default_value("link"),
        )
        .arg(
            Arg::new("mgmt")
                .short('m')
                .long("mgmt")
                .help("listen for management callback connection on ip:port (disabled by default)")
                .default_value(""),
        )
        .arg(
            Arg::new("http")
                .long("http")
                .help("listen for incoming http connections (control plane callbacks, metrics, etc) on ip:port")
                .default_value("127.0.0.1:7001"),
        )
        .arg(
            Arg::new("wss")
                .long("wss")
                .help("listen for incoming wss connections on ip:port"),
        )
        .arg(
            Arg::new("uri")
                .short('u')
                .long("uri")
                .help("redirect unauthenticated users to the given uri in case of link auth")
                .default_value("http://localhost:3000/psql_session/"),
        )
        .arg(
            Arg::new("auth-endpoint")
                .short('a')
                .long("auth-endpoint")
                .help("cloud API endpoint for authenticating users")
                .default_value("http://localhost:3000/authenticate_proxy_request/"),
        )
        .arg(
            Arg::new("tls-key")
                .short('k')
                .long("tls-key")
                .alias("ssl-key") // backwards compatibility
                .help("path to TLS key for client postgres connections"),
        )
        .arg(
            Arg::new("tls-cert")
                .short('c')
                .long("tls-cert")
                .alias("ssl-cert") // backwards compatibility
                .help("path to TLS cert for client postgres connections"),
        )
        .arg(
            Arg::new("metric-collection-endpoint")
                .long("metric-collection-endpoint")
                .help("metric collection HTTP endpoint"),
        )
        .arg(
            Arg::new("metric-collection-interval")
                .long("metric-collection-interval")
                .help("metric collection interval"),
        )
}

#[test]
fn verify_cli() {
    cli().debug_assert();
}
