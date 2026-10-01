use axum::{
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

// Debug builds read web/dist from disk; release builds embed it.
#[derive(RustEmbed)]
#[folder = "web/dist"]
#[allow_missing = true]
struct Assets;

pub fn available() -> bool {
    Assets::get("index.html").is_some()
}

/// Serves a file, or the app itself for any other path so client-side routes work.
pub async fn serve(uri: Uri) -> Response {
    match Assets::get(uri.path().trim_start_matches('/')).or_else(|| Assets::get("index.html")) {
        Some(file) => ([(header::CONTENT_TYPE, file.metadata.mimetype().to_owned())], file.data).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_paths_get_the_app() {
        let response = serve(Uri::from_static("/settings")).await;
        let expected = if available() { StatusCode::OK } else { StatusCode::NOT_FOUND };
        assert_eq!(response.status(), expected);
    }
}
