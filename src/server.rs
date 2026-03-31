use anyhow::Result;
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use serde::Deserialize;
use tokio::{net::TcpListener, signal, time::sleep};

use crate::{
    pan115::{Pan115Client, Pan115Error, Pan115ErrorKind},
    runner::RunOptions,
};

#[derive(Clone)]
struct ServerState {
    pan115: Pan115Client,
    options: RunOptions,
}

#[derive(Debug, Deserialize)]
struct OfflineTask {
    tasks: Vec<String>,
    cid: Option<String>,
    savepath: Option<String>,
}

pub async fn serve(pan115: Pan115Client, options: RunOptions, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/add", post(handle_add_task))
        .with_state(ServerState { pan115, options });

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    println!("server started on port {port}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    println!("server stopped properly");
    Ok(())
}

async fn handle_add_task(
    State(state): State<ServerState>,
    Json(task): Json<OfflineTask>,
) -> impl IntoResponse {
    if task.tasks.is_empty() {
        return (StatusCode::BAD_REQUEST, "tasks is empty").into_response();
    }
    if let Err(err) = state.pan115.ensure_logged_in().await {
        return (StatusCode::UNAUTHORIZED, err.to_string()).into_response();
    }
    match submit_links(
        &state.pan115,
        state.options,
        &task.tasks,
        task.cid.as_deref(),
        task.savepath.as_deref(),
    )
    .await
    {
        Ok(()) => (StatusCode::OK, "message success").into_response(),
        Err(err) => (StatusCode::BAD_GATEWAY, err.to_string()).into_response(),
    }
}

async fn submit_links(
    pan115: &Pan115Client,
    options: RunOptions,
    links: &[String],
    cid: Option<&str>,
    savepath: Option<&str>,
) -> Result<()> {
    let target_dir = pan115.resolve_target_dir(cid, savepath).await?;
    for (index, chunk) in links.chunks(options.chunk_size).enumerate() {
        let chunk_links = chunk.to_vec();
        match pan115
            .add_offline_urls(&chunk_links, target_dir.as_deref())
            .await
        {
            Ok(_) => {
                log::info!("[server] add {} tasks", chunk.len());
            }
            Err(err) => match add_error_kind(&err) {
                Some(Pan115ErrorKind::TaskExisted) => {
                    log::warn!("[server] task exist");
                }
                Some(Pan115ErrorKind::InvalidLink) => {
                    log::warn!("[server] wrong links");
                }
                _ => return Err(err),
            },
        }
        if index + 1 < chunk_count(links.len(), options.chunk_size) {
            sleep(options.chunk_delay).await;
        }
    }
    Ok(())
}

fn add_error_kind(err: &anyhow::Error) -> Option<Pan115ErrorKind> {
    err.downcast_ref::<Pan115Error>().map(Pan115Error::kind)
}

fn chunk_count(len: usize, size: usize) -> usize {
    if len == 0 {
        0
    } else {
        (len - 1) / size + 1
    }
}

async fn shutdown_signal() {
    let _ = signal::ctrl_c().await;
}
