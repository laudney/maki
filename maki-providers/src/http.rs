pub use maki_config::http::{
    DNS_ATTEMPTS, HTTP_SCHEME, HTTPS_SCHEME, Lookup, NO_ADDRESSES, ProxyEnv, Unresolved,
    error_chain, extract_host_port, is_connect_failure, proxy_env, resolve,
};

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::{Client, Error as ReqwestError};
    use std::sync::Arc;
    use test_case::test_case;

    use crate::test_support::transport::{
        LOOPBACK_HOST, QUERY_SECRET, UNRESOLVABLE_HOST, URL_MARKER, closed_port, plain_http_port,
    };

    /// The shell's proxy is left out, so the request goes where the test points it.
    async fn failed_request(url: &str) -> ReqwestError {
        let client = Client::builder()
            .no_proxy()
            .dns_resolver(Arc::new(Lookup))
            .build()
            .unwrap();
        client
            .get(url)
            .send()
            .await
            .map(|r| r.status())
            .unwrap_err()
    }

    fn closed_port_url() -> String {
        format!("http://{LOOPBACK_HOST}:{}/", closed_port())
    }

    fn unresolvable_url() -> String {
        format!("https://{UNRESOLVABLE_HOST}/")
    }

    fn plain_tcp_behind_https_url() -> String {
        format!("https://{LOOPBACK_HOST}:{}/", plain_http_port())
    }

    #[test_case(closed_port_url, true ; "a_closed_port_draws_on_the_connect_budget")]
    #[test_case(unresolvable_url, true ; "an_unresolvable_name_draws_on_the_connect_budget")]
    #[test_case(plain_tcp_behind_https_url, false ; "a_failed_handshake_retries_as_transient")]
    #[tokio::test]
    async fn transport_failures_classify_like_a_providers(url: fn() -> String, expected: bool) {
        let error = failed_request(&url()).await;
        assert_eq!(
            is_connect_failure(&error),
            expected,
            "{}",
            error_chain(&error)
        );
    }

    #[tokio::test]
    async fn error_chain_leaves_out_the_url() {
        let error = failed_request(&format!("{}?key={QUERY_SECRET}", closed_port_url())).await;
        let chain = error_chain(&error);
        assert!(!chain.contains(QUERY_SECRET), "{chain}");
        assert!(!chain.contains(URL_MARKER), "{chain}");
    }
}
