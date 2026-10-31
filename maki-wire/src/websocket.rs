//! A [`Link`] over a WebSocket, one JSON-RPC message per text frame. Framing,
//! TLS, masking and ping replies stay in `async-tungstenite`.

use futures::{SinkExt, StreamExt};
use thiserror::Error;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Error as SocketError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::task::AbortOnDropHandle;
use url::Url;

use crate::client::{END_OF_INPUT, EVENT_CAPACITY, Link, Pumps, logged};

/// Far above any ACP message Maki sends, low enough that a broken peer
/// cannot make the client buffer without end.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const PUMP_SOCKET_OUT: &str = "websocket out";
const PUMP_SOCKET_IN: &str = "websocket in";

#[derive(Debug, Error)]
pub enum WebSocketError {
    #[error(
        "invalid ACP endpoint: use an http, https, ws or wss URL without credentials or a fragment"
    )]
    Endpoint,
    #[error("the ACP bearer token is not a valid header value")]
    Token,
    #[error("the agent at {endpoint} refused the connection: HTTP {status}")]
    Refused { endpoint: String, status: u16 },
    #[error("cannot connect to the agent at {endpoint}: {reason}")]
    Connect { endpoint: String, reason: String },
}

/// `http` means a WebSocket upgrade at that URL and `https` a secure one.
pub fn endpoint(url: &str) -> Result<Url, WebSocketError> {
    let mut url = Url::parse(url).map_err(|_| WebSocketError::Endpoint)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err(WebSocketError::Endpoint);
    }
    let scheme = match url.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        _ => return Err(WebSocketError::Endpoint),
    };
    url.set_scheme(scheme)
        .map_err(|()| WebSocketError::Endpoint)?;
    Ok(url)
}

/// Where an error says it tried to connect. The query is left out because
/// it can carry a credential.
fn shown(url: &Url) -> String {
    let mut shown = url.clone();
    shown.set_query(None);
    shown.to_string()
}

/// The token goes in a sensitive `Authorization` header only. A redirect is
/// not followed: tungstenite reports it as a refusal.
pub async fn connect(url: &str, token: Option<&str>) -> Result<Link, WebSocketError> {
    let url = endpoint(url)?;
    let endpoint = shown(&url);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| WebSocketError::Endpoint)?;
    if let Some(token) = token {
        let mut header =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| WebSocketError::Token)?;
        header.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, header);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let (socket, _) = connect_async_with_config(request, Some(config), false)
        .await
        .map_err(|error| match error {
            SocketError::Http(response) => WebSocketError::Refused {
                endpoint: endpoint.clone(),
                status: response.status().as_u16(),
            },
            other => WebSocketError::Connect {
                endpoint: endpoint.clone(),
                reason: other.to_string(),
            },
        })?;
    let (mut sink, mut stream) = socket.split();
    let (out_tx, out_rx) = flume::unbounded::<String>();
    let (in_tx, in_rx) = flume::bounded::<String>(EVENT_CAPACITY);
    let (writer_alive, writer_gone) = flume::bounded::<()>(0);
    let writer = tokio::spawn(logged(PUMP_SOCKET_OUT, async move {
        let _writer_alive = writer_alive;
        while let Ok(line) = out_rx.recv_async().await {
            if line == END_OF_INPUT {
                break;
            }
            sink.send(Message::Text(line.into()))
                .await
                .map_err(std::io::Error::other)?;
        }
        let _ = sink.close().await;
        Ok(())
    }));
    let reader = tokio::spawn(logged(PUMP_SOCKET_IN, async move {
        loop {
            let gone = async {
                let _ = writer_gone.recv_async().await;
                None
            };
            let read = async { Some(stream.next().await) };
            let text = match futures_lite::future::or(gone, read).await {
                None | Some(None) => return Ok(()),
                Some(Some(Err(error))) => return Err(std::io::Error::other(error)),
                Some(Some(Ok(Message::Text(text)))) => text.to_string(),
                Some(Some(Ok(Message::Close(_)))) => return Ok(()),
                Some(Some(Ok(_))) => continue,
            };
            if in_tx.send_async(text).await.is_err() {
                return Ok(());
            }
        }
    }));
    Ok(Link::with_pumps(
        out_tx,
        in_rx,
        Pumps::new(
            None,
            [
                AbortOnDropHandle::new(writer),
                AbortOnDropHandle::new(reader),
            ],
        ),
    ))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::Implementation;
    use serde_json::{Value, json};
    use test_case::test_case;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::http::StatusCode;

    use super::*;
    use crate::client::{AcpClient, ClientError};
    use crate::methods;

    const TOKEN: &str = "fixture-token";
    const WRONG_TOKEN: &str = "wrong-token";
    const CLIENT_NAME: &str = "maki-test";
    const CLIENT_VERSION: &str = "0";
    const UNAUTHORIZED: u16 = 401;

    #[test_case("http://localhost:7803/acp?workspace=a", "ws://localhost:7803/acp?workspace=a" ; "http_upgrade")]
    #[test_case("https://example.com/acp", "wss://example.com/acp" ; "secure_upgrade")]
    #[test_case("ws://localhost/acp", "ws://localhost/acp" ; "websocket")]
    #[test_case("wss://localhost/acp", "wss://localhost/acp" ; "secure_websocket")]
    fn endpoint_upgrades_the_scheme(input: &str, expected: &str) {
        let _runtime = crate::test_runtime().enter();
        assert_eq!(endpoint(input).unwrap().as_str(), expected);
    }

    #[test_case("file:///acp" ; "unsupported_scheme")]
    #[test_case("http://user:secret@localhost/acp" ; "credentials")]
    #[test_case("http://localhost/acp#fragment" ; "fragment")]
    #[test_case("not a url" ; "not_a_url")]
    fn endpoint_refuses_unsafe_urls(input: &str) {
        let _runtime = crate::test_runtime().enter();
        assert!(matches!(endpoint(input), Err(WebSocketError::Endpoint)));
    }

    #[test]
    fn errors_never_show_the_query() {
        let _runtime = crate::test_runtime().enter();
        let url = endpoint("http://localhost/acp?token=secret").unwrap();
        assert_eq!(shown(&url), "ws://localhost/acp");
    }

    /// Accepts one connection that must carry `TOKEN`, answers the first
    /// request with `initialize_response`, then waits for the client to go.
    async fn fixture(close_after_request: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            #[allow(
                clippy::result_large_err,
                reason = "tungstenite's handshake callback fixes the error type"
            )]
            let check = |request: &Request, response: Response| {
                let expected = format!("Bearer {TOKEN}");
                match request.headers().get(AUTHORIZATION) {
                    Some(value) if value == expected.as_str() => Ok(response),
                    _ => {
                        let mut refusal = ErrorResponse::new(None);
                        *refusal.status_mut() = StatusCode::UNAUTHORIZED;
                        Err(refusal)
                    }
                }
            };
            let Ok(mut socket) = accept_hdr_async(stream, check).await else {
                return;
            };
            let Some(Ok(Message::Text(request))) = socket.next().await else {
                return;
            };
            if close_after_request {
                let _ = socket.close(None).await;
                return;
            }
            let request: Value = serde_json::from_str(&request).unwrap();
            let response = json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": methods::initialize_response(false),
            });
            socket
                .send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
            while socket.next().await.is_some() {}
        });
        format!("http://{address}/acp")
    }

    fn info() -> Implementation {
        Implementation::new(CLIENT_NAME, CLIENT_VERSION)
    }

    #[test]
    fn request_round_trips_over_the_socket() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let url = fixture(false).await;
            let link = connect(&url, Some(TOKEN)).await.unwrap();
            let (client, _events) = AcpClient::new(link);
            let response = client.initialize(info(), false).await.unwrap();
            assert_eq!(response.agent_info.unwrap().name, "maki");
        });
    }

    #[test_case(None ; "no_token")]
    #[test_case(Some(WRONG_TOKEN) ; "wrong_token")]
    fn refusal_reports_the_status_without_the_token(token: Option<&str>) {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let url = fixture(false).await;
            let error = connect(&url, token).await.err().unwrap();
            assert!(matches!(
                error,
                WebSocketError::Refused {
                    status: UNAUTHORIZED,
                    ..
                }
            ));
            assert!(!error.to_string().contains(WRONG_TOKEN));
        });
    }

    #[test]
    fn server_close_fails_the_pending_request() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let url = fixture(true).await;
            let link = connect(&url, Some(TOKEN)).await.unwrap();
            let (client, _events) = AcpClient::new(link);
            assert!(matches!(
                client.initialize(info(), false).await,
                Err(ClientError::Closed)
            ));
        });
    }
}
