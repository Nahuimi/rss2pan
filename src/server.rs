use anyhow::Result;
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use serde::Deserialize;
use tokio::{net::TcpListener, signal};

use crate::{
    pan115::{Pan115Client, Pan115ErrorKind},
    runner::{submit_error_kind, submit_links_with_options, RunOptions},
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
    match submit_links_with_options(
        &state.pan115,
        state.options,
        "[server]",
        &task.tasks,
        task.cid.as_deref(),
        task.savepath.as_deref(),
    )
    .await
    {
        Ok(()) => (StatusCode::OK, "message success").into_response(),
        Err(err) => (add_task_error_status(&err), err.to_string()).into_response(),
    }
}


fn add_task_error_status(err: &anyhow::Error) -> StatusCode {
    match submit_error_kind(err) {
        Some(Pan115ErrorKind::InvalidLink) => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    }
}

async fn shutdown_signal() {
    let _ = signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pan115::Pan115Error;

    #[test]
    fn test_invalid_link_maps_to_bad_request() {
        let err: anyhow::Error = Pan115Error::new(10004, None).into();
        assert_eq!(add_task_error_status(&err), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_unexpected_error_maps_to_bad_gateway() {
        let err: anyhow::Error = Pan115Error::new(99999, None).into();
        assert_eq!(add_task_error_status(&err), StatusCode::BAD_GATEWAY);
    }
}
