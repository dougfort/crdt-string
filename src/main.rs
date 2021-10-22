use anyhow::Error;
use axum::{
    extract::Extension,
    handler::{get, post},
    Json, Router,
};
use crdts::list;
use hyper::{Body, Client, Method, Request};
use rand::Rng;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Notify;
use tower::ServiceBuilder;
use tower_http::{add_extension::AddExtensionLayer, trace::TraceLayer};

mod genome;
use genome::{Actor, Gene};

mod config;

type SharedState = Arc<RwLock<State>>;

#[derive(Default)]
struct State {
    genome: genome::Genome,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Set the RUST_LOG, if it hasn't been explicitly defined
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "crdt_genome=debug,tower_http=debug")
    }
    tracing_subscriber::fmt::init();

    let config = config::load_configuration()?;

    tracing::info!(
        "actor = {}; count = {}, base port = {}",
        config.actor_id,
        config.actor_count,
        config.base_port_number
    );

    let state = SharedState::default();

    let mutator_notify = Arc::new(Notify::new());
    let mutator_notify2 = Arc::clone(&mutator_notify);

    let mutator_state = state.clone();
    let mutator_handle = tokio::spawn(async move {
        mutator(mutator_state, config, mutator_notify2).await;
    });

    // build our application with a single route
    let app = Router::new()
        .route("/", get(say_hello))
        .route("/genome", post(update_genome))
        .layer(TraceLayer::new_for_http())
        .layer(ServiceBuilder::new().layer(AddExtensionLayer::new(state)));

    // run it with hyper
    let port_number = config.base_port_number + config.actor_id;
    let addr = format!("0.0.0.0:{}", port_number).parse()?;
    tracing::debug!("Listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    mutator_notify.notify_one();
    let join_result = mutator_handle.await?;
    tracing::debug!("mutator join result = {:?}", join_result);

    Ok(())
}

async fn say_hello() -> String {
    "Hello, World!".to_string()
}

async fn update_genome(
    Json(op): Json<list::Op<Gene, Actor>>,
    Extension(state): Extension<SharedState>,
) {
    tracing::debug!("server received op: {:?}", op);
    state.write().unwrap().genome.apply(op);
}

async fn mutator(
    state: Arc<RwLock<State>>,
    config: config::Config,
    mutator_notify: Arc<tokio::sync::Notify>,
) {
    // wait for the server to start
    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut more = true;
    while more {
        let op = {
            let mut lock = state.write().unwrap();
            lock.genome.generate(config.actor_id)
        };
        for i in 0..config.actor_count {
            if i != config.actor_id {
                let op_string = serde_json::to_string(&op).unwrap();
                let port_number = config.base_port_number + i;
                let uri = format!("http://127.0.0.1:{}/genome", port_number);
                let req = Request::builder()
                    .method(Method::POST)
                    .uri(uri.clone())
                    .header("content-type", "application/json")
                    .body(Body::from(op_string))
                    .unwrap();
                let client = Client::new();
                let resp = client.request(req).await.unwrap();
                tracing::debug!("POST {}; Response: {}", uri, resp.status());
            }
        }
        let sleep_interval = {
            let mut rng = rand::thread_rng();
            Duration::from_secs(rng.gen_range(0..20))
        };
        tokio::select! {
            _ = tokio::time::sleep(sleep_interval) => {}
            _ = mutator_notify.notified() => {more = false}
        }
    }
}

#[cfg(unix)]
async fn shutdown_signal() {
    use std::io;
    use tokio::signal::unix::SignalKind;

    async fn terminate() -> io::Result<()> {
        tokio::signal::unix::signal(SignalKind::terminate())?
            .recv()
            .await;
        Ok(())
    }

    tokio::select! {
        _ = terminate() => {},
        _ = tokio::signal::ctrl_c() => {},
    }
    tracing::info!("signal received, starting graceful shutdown")
}

#[cfg(windows)]
async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("faild to install CTRL+C handler");
    tracing::info!("signal received, starting graceful shutdown")
}
