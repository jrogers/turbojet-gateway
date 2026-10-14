//! The web console (`web/`), built into the binary with the `console` feature and served at every
//! path outside `/api`: a file of its build if the path names one, else its `index.html`, whose
//! script routes the rest. It needs no authentication to load; the API it calls does.

use axum::http::header::{self, HeaderName, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};

use super::ApiError;

// `ASSETS`: each file of `web/dist`, by its path under `/`, written by `build.rs`.
include!(concat!(env!("OUT_DIR"), "/console_assets.rs"));

/// Sent with every file. Websockets to the page's own host are covered by `connect-src 'self'`
/// in browsers that follow CSP 3, Safari since 2022 (WebKit bug 235873).
const HEADERS: [(HeaderName, &str); 4] = [
    (
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; \
         frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
    ),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "no-referrer"),
    (header::X_FRAME_OPTIONS, "DENY"),
];

/// Answers `GET` or `HEAD` of `path`: its file, or `index.html` for a path the console's own
/// routing handles. Files under `/assets/` have hashed names, so may be kept for ever; one that
/// isn't there is a 404, not the page, so a stale page's script fails plainly.
pub(super) fn serve(method: &Method, path: &str) -> Response {
    if method != Method::GET && method != Method::HEAD {
        let error = ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        return ([(header::ALLOW, "GET, HEAD")], error).into_response();
    }
    let (path, bytes) = match find(path) {
        Some(bytes) => (path, bytes),
        None if path.starts_with("/assets/") => {
            return ApiError::new(StatusCode::NOT_FOUND, format!("no such path: {path}")).into_response();
        }
        None => ("/index.html", find("/index.html").expect("build.rs checks web/dist has index.html")),
    };
    let cache = if path.starts_with("/assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    for (name, value) in HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(path)));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}

fn find(path: &str) -> Option<&'static [u8]> {
    ASSETS.iter().find(|(name, _)| *name == path).map(|(_, bytes)| *bytes)
}

/// The `Content-Type` of a file by its extension.
fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map_or("", |(_, extension)| extension) {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" | "map" => "application/json",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::connect_info::MockConnectInfo;
    use tower::ServiceExt;

    /// `method path` to the API's router, answering its status, headers and body.
    async fn send(method: &str, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let (app, _dir) = super::super::tests::app();
        let app = app.layer(MockConnectInfo("127.0.0.1:50000".parse::<std::net::SocketAddr>().unwrap()));
        let request = axum::http::Request::builder().method(method).uri(path).body(axum::body::Body::empty()).unwrap();
        let (parts, body) = app.oneshot(request).await.unwrap().into_parts();
        (parts.status, parts.headers, axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec())
    }

    fn index() -> &'static [u8] {
        find("/index.html").unwrap()
    }

    #[tokio::test]
    async fn the_root_is_index_html_with_every_header() {
        let (status, headers, body) = send("GET", "/").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, index());
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        for (name, value) in HEADERS {
            assert_eq!(headers[name], value);
        }
    }

    #[tokio::test]
    async fn an_asset_has_its_type_and_is_kept_for_ever() {
        let (path, bytes) =
            ASSETS.iter().find(|(path, _)| path.starts_with("/assets/") && path.ends_with(".js")).unwrap();
        let (status, headers, body) = send("GET", path).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, *bytes);
        assert_eq!(headers[header::CONTENT_TYPE], "text/javascript; charset=utf-8");
        assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
    }

    /// The console's own routes are its `index.html`; a file under `/assets/` it doesn't have is a
    /// 404.
    #[tokio::test]
    async fn other_paths_are_index_html() {
        let (status, headers, body) = send("GET", "/blotter/x").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, index());
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        let (status, headers, body) = send("GET", "/assets/gone.js").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(body, br#"{"error":"no such path: /assets/gone.js"}"#);
    }

    /// `/api` stays the API's, unknown paths and the authentication it needs included, and `//`
    /// paths aren't the console's.
    #[tokio::test]
    async fn the_api_comes_first() {
        let (status, headers, body) = send("GET", "/api/me").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert!(body.starts_with(br#"{"error":"#));
        for path in ["/api/nope", "/api", "//api/me"] {
            let (status, headers, body) = send("GET", path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert_eq!(headers[header::CONTENT_TYPE], "application/json", "{path}");
            assert_eq!(body, format!(r#"{{"error":"no such path: {path}"}}"#).as_bytes());
        }
    }

    #[tokio::test]
    async fn only_get_and_head_are_served() {
        let (_, get_headers, _) = send("GET", "/").await;
        let (status, headers, body) = send("HEAD", "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_empty());
        for name in [header::CONTENT_TYPE, header::CONTENT_SECURITY_POLICY, header::CACHE_CONTROL] {
            assert_eq!(headers[&name], get_headers[&name], "{name}");
        }
        let (status, headers, body) = send("POST", "/").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(headers[header::ALLOW], "GET, HEAD");
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(body, br#"{"error":"method not allowed"}"#);
    }

    /// A file the build adds of a type [`content_type`] doesn't know would be served as bytes.
    #[test]
    fn every_asset_has_a_known_type() {
        for (path, _) in ASSETS {
            assert_ne!(content_type(path), "application/octet-stream", "{path}");
        }
    }
}
