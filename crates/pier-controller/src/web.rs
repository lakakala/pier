use axum::{
    body::Body,
    extract::Request,
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
};
include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

pub(crate) async fn serve(request: Request) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = request.uri().path();
    let page = matches!(
        path,
        "/" | "/init"
            | "/login"
            | "/repository"
            | "/apps"
            | "/blueprints"
            | "/agents"
            | "/deployments"
            | "/settings"
            | "/settings/controller"
            | "/agent/init"
    ) || ["/agents/", "/deployments/"].iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(pier_protocol::safe_id)
    });
    let Some(bytes) = asset(if page { "/index.html" } else { path }) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = if page {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    };
    let nonce = pier_protocol::new_token();
    let body = if request.method() == Method::HEAD {
        Body::empty()
    } else if page {
        Body::from(String::from_utf8_lossy(bytes).replace("__PIER_NONCE__", &nonce))
    } else {
        Body::from(bytes)
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, content_type),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        body,
    )
        .into_response();
    if page {
        response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, format!("default-src 'none'; script-src 'self'; style-src 'self' 'nonce-{nonce}'; style-src-attr 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'").parse().unwrap());
    }
    response
}
