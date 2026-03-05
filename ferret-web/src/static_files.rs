use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::Embed)]
#[folder = "static/"]
struct Assets;

pub async fn static_handler(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    match Assets::get(&path) {
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, {
                        let mime_str = mime.as_ref();
                        if mime_str.starts_with("text/") {
                            format!("{mime_str}; charset=utf-8")
                        } else {
                            mime_str.to_string()
                        }
                    }),
                    (
                        header::CACHE_CONTROL,
                        "public, max-age=3600, must-revalidate".to_string(),
                    ),
                ],
                file.data.to_vec(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
