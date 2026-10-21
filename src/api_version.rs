use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use serde::Serialize;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

static V1_REQUESTS: AtomicU64 = AtomicU64::new(0);
static V2_REQUESTS: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize)]
pub struct VersionUsage {
    pub v1: u64,
    pub v2: u64,
}

pub async fn usage() -> Json<VersionUsage> {
    Json(VersionUsage {
        v1: V1_REQUESTS.load(Ordering::Relaxed),
        v2: V2_REQUESTS.load(Ordering::Relaxed),
    })
}

fn parse_requested_version(value: &str) -> Option<u8> {
    let value = value.trim();
    let value = value.strip_prefix("application/vnd.zenith.v").unwrap_or(value);
    let value = value.strip_suffix("+json").unwrap_or(value);
    let value = value.strip_prefix('v').unwrap_or(value);
    value.parse().ok()
}

fn version_from_path(path: &str) -> Option<u8> {
    path.strip_prefix("/api/v")?
        .split('/')
        .next()?
        .parse()
        .ok()
}

fn websocket_version(headers: &axum::http::HeaderMap) -> Result<Option<u8>, ()> {
    let mut version = None;
    for value in headers.get_all("sec-websocket-protocol") {
        let value = value.to_str().map_err(|_| ())?;
        for protocol in value.split(',').map(str::trim) {
            if let Some(requested) = protocol
                .strip_prefix("zenith.v")
                .and_then(|value| value.parse::<u8>().ok())
            {
                if version.is_some_and(|current| current != requested) {
                    return Err(());
                }
                version = Some(requested);
            }
        }
    }
    Ok(version)
}

fn deprecation_routes() -> Vec<String> {
    std::env::var("ZENITH_DEPRECATED_ROUTES")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|route| !route.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn error_response(status: StatusCode, message: &'static str) -> Response {
    (status, Json(serde_json::json!({"error": message}))).into_response()
}

/// Negotiate an explicit major version against URL versioning, report usage,
/// and attach configured sunset metadata to deprecated paths.
pub async fn negotiate_version(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let path_version = version_from_path(&path);
    let header_version = match request.headers().get("x-api-version") {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(parse_requested_version)
            .map(Some)
            .ok_or(()),
        None => Ok(None),
    };
    let accept_version = match request.headers().get(header::ACCEPT) {
        Some(value) => match value.to_str() {
            Ok(value) if value.starts_with("application/vnd.zenith.v") => {
                parse_requested_version(value).map(Some).ok_or(())
            }
            Ok(_) => Ok(None),
            Err(_) => Err(()),
        },
        None => Ok(None),
    };
    let websocket_version = websocket_version(request.headers());

    let (header_version, accept_version, websocket_version) =
        match (header_version, accept_version, websocket_version) {
        (Ok(header), Ok(accept), Ok(websocket)) => (header, accept, websocket),
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid API version negotiation header",
            )
        }
    };
    let negotiated: Vec<u8> = [header_version, accept_version, websocket_version]
        .into_iter()
        .flatten()
        .collect();
    if negotiated.windows(2).any(|versions| versions[0] != versions[1]) {
        return error_response(StatusCode::BAD_REQUEST, "conflicting API versions requested");
    }
    let requested = negotiated.first().copied();
    if requested.is_some_and(|version| !matches!(version, 1 | 2))
        || path_version.is_some_and(|version| !matches!(version, 1 | 2))
    {
        return error_response(StatusCode::NOT_ACCEPTABLE, "unsupported API version");
    }
    if requested.is_some() && path_version.is_some() && requested != path_version {
        return error_response(
            StatusCode::BAD_REQUEST,
            "requested API version does not match URL version",
        );
    }

    let version = path_version.or(requested);
    if let Some(version) = version {
        match version {
            1 => &V1_REQUESTS,
            2 => &V2_REQUESTS,
            _ => unreachable!("unsupported versions returned above"),
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    let mut response = next.run(request).await;
    if let Some(version) = version {
        response.headers_mut().insert(
            "api-version",
            HeaderValue::from_str(&version.to_string()).expect("numeric version is a valid header"),
        );
    }
    if websocket_version.is_some() {
        if let Some(version) = version {
            response.headers_mut().insert(
                "sec-websocket-protocol",
                HeaderValue::from_str(&format!("zenith.v{version}"))
                    .expect("numeric websocket version is a valid header"),
            );
        }
    }
    if version == Some(1) && deprecation_routes().iter().any(|route| route == &path) {
        if let Ok(value) = std::env::var("ZENITH_DEPRECATION_TIMESTAMP") {
            if let Ok(value) = HeaderValue::from_str(&format!("@{value}")) {
                response.headers_mut().insert("deprecation", value);
            } else {
                tracing::error!("configured deprecation timestamp is not a valid header value");
            }
        }
        if let Ok(sunset) = std::env::var("ZENITH_SUNSET_DATE") {
            match HeaderValue::from_str(&sunset) {
                Ok(value) => {
                    response.headers_mut().insert("sunset", value);
                    response.headers_mut().insert(
                        header::LINK,
                        HeaderValue::from_static(
                            "</docs/api-versioning-policy.md>; rel=\"deprecation\"",
                        ),
                    );
                }
                Err(error) => tracing::error!(%error, "invalid configured Sunset header"),
            }
        }
    }
    response
}

#[derive(Serialize)]
pub struct SpotResponseV2 {
    pub assets: std::collections::HashMap<String, AssetQuoteV2>,
}

#[derive(Serialize)]
pub struct AssetQuoteV2 {
    pub price: f64,
    pub implied_vol: f64,
}

pub async fn get_spot_v2(State(state): State<AppState>) -> Json<SpotResponseV2> {
    let prices = state.spot_prices.lock().unwrap().clone();
    let vols = state.vol_surface.lock().unwrap().clone();
    let assets = prices
        .into_iter()
        .filter_map(|(symbol, price)| {
            vols.get(&symbol).map(|vol| {
                (
                    symbol,
                    AssetQuoteV2 {
                        price,
                        implied_vol: *vol,
                    },
                )
            })
        })
        .collect();
    Json(SpotResponseV2 { assets })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_major_versions_from_url_and_accept_headers() {
        assert_eq!(version_from_path("/api/v1/spot"), Some(1));
        assert_eq!(version_from_path("/api/v2/spot"), Some(2));
        assert_eq!(parse_requested_version("application/vnd.zenith.v2+json"), Some(2));
        assert_eq!(parse_requested_version("1"), Some(1));
        assert_eq!(version_from_path("/health"), None);
    }

    #[test]
    fn reads_websocket_version_subprotocol() {
        let headers = axum::http::HeaderMap::from_iter([(
            "sec-websocket-protocol".parse().unwrap(),
            "chat, zenith.v2".parse().unwrap(),
        )]);
        assert_eq!(websocket_version(&headers).unwrap(), Some(2));
    }
}
