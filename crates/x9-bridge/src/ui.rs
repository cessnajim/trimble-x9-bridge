//! The phone web UI: static files built into the binary, served at `/ui/`.
//! They need no token to load; every API call they make sends one.

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
};

const INDEX: &str = include_str!("../ui/index.html");
const FILES: [(&str, &str, &str); 3] = [
    ("app.js", "text/javascript; charset=utf-8", include_str!("../ui/app.js")),
    ("app.css", "text/css; charset=utf-8", include_str!("../ui/app.css")),
    ("icon.svg", "image/svg+xml", include_str!("../ui/icon.svg")),
];

fn page(content_type: &'static str, body: &'static str) -> Response {
    // Small files, changed only by a new bridge build: always revalidate.
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

pub async fn to_ui() -> Redirect {
    Redirect::to("/ui/")
}

pub async fn index() -> Response {
    page("text/html; charset=utf-8", INDEX)
}

pub async fn asset(Path(file): Path<String>) -> Response {
    match FILES.iter().find(|(name, _, _)| *name == file) {
        Some((_, ct, body)) => page(ct, body),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn index_loads_its_assets() {
        for (name, _, body) in super::FILES {
            assert!(!body.is_empty(), "{name} is empty");
            if name != "icon.svg" {
                assert!(super::INDEX.contains(name), "index.html does not load {name}");
            }
        }
    }
}
