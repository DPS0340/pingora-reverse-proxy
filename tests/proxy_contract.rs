use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use http::header::{CONNECTION, HOST, LOCATION, UPGRADE};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode, Uri};
use pingora::protocols::l4::socket::SocketAddr;
use proptest::prelude::*;
use url::Url;

use pingora_reverse_proxy::config::ProxyOptions;
use pingora_reverse_proxy::route::RouteKey;
use pingora_reverse_proxy::upstream::{
    apply_forwarded_headers, apply_request_headers, build_upstream_uri, rewrite_location,
    ForwardedContext, Target, TargetError, TlsClientConfig, UpstreamRoute,
};

fn options(include_prefix: bool, prepend_path: bool) -> ProxyOptions {
    ProxyOptions {
        x_forward: true,
        prepend_path,
        include_prefix,
        auto_rewrite: false,
        change_origin: false,
        protocol_rewrite: None,
        custom_headers: BTreeMap::new(),
        verify_upstream_tls: true,
        host_routing: false,
        timeout_ms: None,
        proxy_timeout_ms: None,
        keep_alive_timeout_ms: Some(5_000),
    }
}

fn route(prefix: &str, target: &str) -> UpstreamRoute {
    UpstreamRoute::new(
        RouteKey::parse(prefix).unwrap(),
        Target::parse(&Url::parse(target).unwrap()).unwrap(),
    )
}

#[test]
fn uri_matrix_matches_chp_path_joining() {
    let cases = [
        (true, true, "/base/user/alice/tree?a=%2F&b=1+2"),
        (false, true, "/base/tree?a=%2F&b=1+2"),
        (true, false, "/user/alice/tree?a=%2F&b=1+2"),
        (false, false, "/tree?a=%2F&b=1+2"),
    ];

    for (include_prefix, prepend_path, expected) in cases {
        let output = build_upstream_uri(
            &route("/user/alice", "http://upstream.example/base/"),
            &Uri::from_static("/user/alice/tree?a=%2F&b=1+2"),
            &options(include_prefix, prepend_path),
        )
        .unwrap();
        assert_eq!(
            output, expected,
            "include={include_prefix} prepend={prepend_path}"
        );
    }
}

#[test]
fn uri_join_preserves_literal_duplicate_slashes_like_http_proxy() {
    let output = build_upstream_uri(
        &route("/user/alice", "http://upstream.example//base//"),
        &Uri::from_static("/user/alice//tree///leaf?q=//preserved"),
        &options(false, true),
    )
    .unwrap();
    assert_eq!(output, "//base///tree///leaf?q=//preserved");
}

#[test]
fn uri_root_route_and_target_root_keep_one_leading_slash() {
    for include_prefix in [false, true] {
        for prepend_path in [false, true] {
            let output = build_upstream_uri(
                &route("/", "http://upstream.example/"),
                &Uri::from_static("/escaped%2Fsegment?q=%25FF"),
                &options(include_prefix, prepend_path),
            )
            .unwrap();
            assert_eq!(output, "/escaped%2Fsegment?q=%25FF");
        }
    }
}

#[test]
fn uri_host_routing_prefix_is_removed_at_a_decoded_path_boundary() {
    let mut opts = options(false, true);
    opts.host_routing = true;
    let output = build_upstream_uri(
        &route("/example.test/user/alice", "http://upstream.example/base"),
        &Uri::from_static("/user/alice/tree?q=%2F"),
        &opts,
    )
    .unwrap();
    // CHP slices the raw request URL by the decoded route prefix's JS string
    // length, including the host-routing component. The pinned CHP 5.3.0
    // process therefore forwards only the target path in this case.
    assert_eq!(output, "/base");
}

#[test]
fn uri_no_include_prefix_reproduces_chp_raw_encoded_slice() {
    let output = build_upstream_uri(
        &route("/b@r/b r", "http://upstream.example/foo"),
        &Uri::from_static("/b%40r/b%20r/rest/of/it"),
        &options(false, true),
    )
    .unwrap();
    assert_eq!(output, "/foo/%20r/rest/of/it");
}

#[test]
fn uri_join_preserves_repeated_slashes_inside_each_segment() {
    let output = build_upstream_uri(
        &route("/user", "http://upstream.example/base//keep/"),
        &Uri::from_static("/user///tail"),
        &options(true, true),
    )
    .unwrap();
    assert_eq!(output, "/base//keep/user///tail");
}

proptest! {
    #[test]
    fn uri_query_bytes_are_preserved(
        include_prefix in any::<bool>(),
        prepend_path in any::<bool>(),
        query in prop::collection::vec(prop_oneof![Just(b'a'), Just(b'Z'), Just(b'0'), Just(b'%'), Just(b'2'), Just(b'F'), Just(b'+'), Just(b'='), Just(b'&'), Just(b';'), Just(b':')], 1..80),
    ) {
        let query = String::from_utf8(query).unwrap();
        let request: Uri = format!("/user/alice/a%2Fb?{query}").parse().unwrap();
        let output = build_upstream_uri(
            &route("/user/alice", "http://upstream.example/base"),
            &request,
            &options(include_prefix, prepend_path),
        ).unwrap();
        prop_assert_eq!(output.query(), request.query());
    }
}

#[test]
fn uri_empty_query_delimiter_is_dropped_like_http_proxy_url_join() {
    let request: Uri = "/user/alice/tree?".parse().unwrap();
    let output = build_upstream_uri(
        &route("/user/alice", "http://upstream.example/base"),
        &request,
        &options(true, true),
    )
    .unwrap();
    assert_eq!(output, "/base/user/alice/tree");
    assert_eq!(output.query(), None);
}

#[test]
fn uri_request_headers_apply_custom_origin_and_forwarded_host_policy() {
    let target = Target::parse(&Url::parse("https://upstream.example:8443/base").unwrap()).unwrap();
    let mut opts = options(true, true);
    opts.change_origin = true;
    opts.custom_headers
        .insert("x-custom".into(), "configured".into());

    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("public.example:9000"));
    headers.insert("x-custom", HeaderValue::from_static("request"));
    apply_request_headers(&mut headers, &target, &opts).unwrap();

    assert_eq!(headers[HOST], "upstream.example:8443");
    assert_eq!(headers["x-forwarded-host"], "public.example:9000");
    assert_eq!(headers["x-custom"], "configured");
}

#[test]
fn uri_request_headers_remove_connection_named_hop_headers() {
    let target = Target::parse(&Url::parse("http://upstream.example").unwrap()).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive, X-Remove"));
    headers.insert("x-remove", HeaderValue::from_static("secret"));
    headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
    headers.insert("proxy-connection", HeaderValue::from_static("keep-alive"));
    headers.insert("te", HeaderValue::from_static("trailers"));
    headers.insert("trailer", HeaderValue::from_static("x-checksum"));
    headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));

    apply_request_headers(&mut headers, &target, &options(true, true)).unwrap();

    for removed in [
        "connection",
        "x-remove",
        "keep-alive",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        assert!(!headers.contains_key(removed), "{removed} survived");
    }
}

#[test]
fn uri_request_headers_keep_websocket_upgrade_pair_only() {
    let target = Target::parse(&Url::parse("http://upstream.example").unwrap()).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        CONNECTION,
        HeaderValue::from_static("keep-alive, Upgrade, X-Remove"),
    );
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert("x-remove", HeaderValue::from_static("secret"));

    apply_request_headers(&mut headers, &target, &options(true, true)).unwrap();

    assert_eq!(headers[CONNECTION], "upgrade");
    assert_eq!(headers[UPGRADE], "websocket");
    assert!(!headers.contains_key("x-remove"));
}

#[test]
fn uri_invalid_custom_header_is_a_typed_error_not_a_panic() {
    let target = Target::parse(&Url::parse("http://upstream.example").unwrap()).unwrap();
    let mut opts = options(true, true);
    opts.custom_headers
        .insert("bad header".into(), "value".into());
    let error = apply_request_headers(&mut HeaderMap::new(), &target, &opts).unwrap_err();
    assert!(error.to_string().contains("custom header"));
}

#[test]
fn uri_invalid_custom_header_leaves_request_headers_unchanged() {
    let target = Target::parse(&Url::parse("https://upstream.example").unwrap()).unwrap();
    let mut opts = options(true, true);
    opts.change_origin = true;
    opts.custom_headers
        .insert("x-invalid".into(), "value\r\ninjected: yes".into());
    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("public.example"));
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    let before = headers.clone();

    assert!(apply_request_headers(&mut headers, &target, &opts).is_err());
    assert_eq!(headers, before);
}

#[test]
fn uri_x_forwarded_values_append_exactly_like_http_proxy() {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("public.example:9000"));
    headers.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.1"));
    headers.insert("x-forwarded-port", HeaderValue::from_static("443"));
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    headers.insert("x-forwarded-host", HeaderValue::from_static("edge.example"));
    let context = ForwardedContext {
        client_address: "203.0.113.7",
        port: 9000,
        protocol: "http",
    };

    apply_forwarded_headers(&mut headers, &context, &options(true, true)).unwrap();

    assert_eq!(headers["x-forwarded-for"], "10.0.0.1,203.0.113.7");
    assert_eq!(headers["x-forwarded-port"], "443,9000");
    assert_eq!(headers["x-forwarded-proto"], "https,http");
    assert_eq!(headers["x-forwarded-host"], "edge.example");
}

#[test]
fn uri_x_forwarded_disabled_is_a_noop() {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static("public.example"));
    let before = headers.clone();
    let mut opts = options(true, true);
    opts.x_forward = false;
    apply_forwarded_headers(
        &mut headers,
        &ForwardedContext {
            client_address: "203.0.113.7",
            port: 80,
            protocol: "http",
        },
        &opts,
    )
    .unwrap();
    assert_eq!(headers, before);
}

#[test]
fn uri_redirect_rewrite_matrix_matches_http_proxy() {
    for auto_rewrite in [false, true] {
        for protocol_rewrite in [None, Some("https".to_owned())] {
            let target =
                Target::parse(&Url::parse("http://upstream.example:8080/base").unwrap()).unwrap();
            let mut opts = options(true, true);
            opts.auto_rewrite = auto_rewrite;
            opts.protocol_rewrite = protocol_rewrite.clone();
            let request = Request::builder()
                .uri("/from")
                .header(HOST, "public.example:9443")
                .body(())
                .unwrap();
            let mut response = Response::builder()
                .status(StatusCode::FOUND)
                .header(LOCATION, "http://upstream.example:8080/next?q=%2F")
                .body(())
                .unwrap();

            rewrite_location(&mut response, &request, &target, &opts).unwrap();

            let expected_host = if auto_rewrite {
                "public.example:9443"
            } else {
                "upstream.example:8080"
            };
            let expected_scheme = protocol_rewrite.as_deref().unwrap_or("http");
            assert_eq!(
                response.headers()[LOCATION],
                format!("{expected_scheme}://{expected_host}/next?q=%2F")
            );
        }
    }
}

#[test]
fn uri_redirect_rewrite_requires_matching_target_host_and_redirect_status() {
    let target = Target::parse(&Url::parse("http://upstream.example:8080").unwrap()).unwrap();
    let mut opts = options(true, true);
    opts.auto_rewrite = true;
    for (status, location) in [
        (StatusCode::OK, "http://upstream.example:8080/next"),
        (StatusCode::FOUND, "http://other.example/next"),
    ] {
        let request = Request::builder()
            .header(HOST, "public.example")
            .body(())
            .unwrap();
        let mut response = Response::builder()
            .status(status)
            .header(LOCATION, location)
            .body(())
            .unwrap();
        rewrite_location(&mut response, &request, &target, &opts).unwrap();
        assert_eq!(response.headers()[LOCATION], location);
    }
}

#[test]
fn uri_target_parses_tcp_and_unix_variants() {
    let http = Target::parse(&Url::parse("http://example.test/base").unwrap()).unwrap();
    assert_eq!(http.authority(), "example.test");
    assert_eq!(http.path(), "/base");

    let https = Target::parse(&Url::parse("https://example.test:9443/").unwrap()).unwrap();
    assert_eq!(https.authority(), "example.test:9443");
    assert!(https.is_tls());

    let unix = Target::parse(&Url::parse("http+unix://%2Ftmp%2Fchp.sock/base").unwrap()).unwrap();
    assert_eq!(
        unix.unix_path(),
        Some(std::path::Path::new("/tmp/chp.sock"))
    );
    assert_eq!(unix.path(), "/base");
}

#[test]
fn uri_target_rejects_unsupported_or_invalid_unix_targets() {
    assert!(matches!(
        Target::parse(&Url::parse("ftp://example.test/file").unwrap()),
        Err(TargetError::UnsupportedScheme(_))
    ));
    assert!(Target::parse(&Url::parse("http+unix://relative/base").unwrap()).is_err());
    assert!(Target::parse(&Url::parse("http+unix://%00tmp/base").unwrap()).is_err());
}

#[test]
fn uri_http_peer_defaults_to_certificate_and_hostname_verification() {
    let target = Target::parse(&Url::parse("https://127.0.0.1:9443").unwrap()).unwrap();
    let peer = target.http_peer(&TlsClientConfig::default()).unwrap();
    assert!(peer.options.verify_cert);
    assert!(peer.options.verify_hostname);
}

#[test]
fn uri_http_peer_applies_transport_policy_without_panicking() {
    let target = Target::parse(&Url::parse("https://127.0.0.1:9443/base").unwrap()).unwrap();
    let tls = TlsClientConfig {
        verify_cert: false,
        verify_hostname: false,
        connection_timeout: Some(Duration::from_millis(10)),
        total_connection_timeout: Some(Duration::from_millis(15)),
        read_timeout: Some(Duration::from_millis(20)),
        write_timeout: Some(Duration::from_millis(30)),
        idle_timeout: Some(Duration::from_millis(40)),
        ca_file: None,
        client_certificate: None,
        client_key: None,
    };
    let peer = target.http_peer(&tls).unwrap();
    assert!(peer.is_tls());
    assert!(!peer.options.verify_cert);
    assert!(!peer.options.verify_hostname);
    assert_eq!(peer.options.connection_timeout, tls.connection_timeout);
    assert_eq!(
        peer.options.total_connection_timeout,
        tls.total_connection_timeout
    );
    assert_eq!(peer.options.read_timeout, tls.read_timeout);
    assert_eq!(peer.options.write_timeout, tls.write_timeout);
    assert_eq!(peer.options.idle_timeout, tls.idle_timeout);
}

#[test]
fn uri_unix_http_peer_uses_the_decoded_socket_path() {
    let target = Target::parse(&Url::parse("http+unix://%2Ftmp%2Fchp.sock/base").unwrap()).unwrap();
    let peer = target.http_peer(&TlsClientConfig::default()).unwrap();
    assert!(matches!(peer._address, SocketAddr::Unix(_)));
    assert!(!peer.is_tls());
}

#[test]
fn uri_http_peer_reports_unresolvable_addresses_instead_of_panicking() {
    let target = Target::parse(&Url::parse("http://nonexistent.invalid:8080").unwrap()).unwrap();
    assert!(target.http_peer(&TlsClientConfig::default()).is_err());
}

#[test]
fn uri_http_peer_rejects_partial_client_identity() {
    let target = Target::parse(&Url::parse("https://127.0.0.1").unwrap()).unwrap();
    let tls = TlsClientConfig {
        client_certificate: Some(PathBuf::from("cert.pem")),
        ..TlsClientConfig::default()
    };
    assert!(target.http_peer(&tls).is_err());
}
