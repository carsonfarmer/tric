//! Outbound HTTP: the allow list, then name lookup and TCP by hand, so only an address that passed the block check is dialled.
use crate::guest::{Fut, Shared};
use http_body_util::BodyExt;
use hyper::{Uri, client::conn::http1};
use std::net::IpAddr;
use std::sync::{Arc, LazyLock};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, lookup_host};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto::aws_lc_rs::default_provider, pki_types::ServerName};
use wasmtime_wasi_http::handler::{Request, Response};
use wasmtime_wasi_http::{Error, RequestOptions, WasiHttpHooks, io::TokioIo};

const ANY: &str = "*://*:*";

/// A URI's `scheme://host:port`, lowercase and with the port filled in, if it is http or https with a host.
fn origin(uri: &Uri) -> Option<String> {
    let default = match uri.scheme_str()? {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    Some(format!("{}://{}:{}", uri.scheme_str()?, uri.host()?, uri.port_u16().unwrap_or(default)).to_ascii_lowercase())
}

/// One allow-list item, `scheme://host[:port]`, kept as its origin. A host may start with `*.`, and `*://*:*` allows any.
pub(crate) struct Allow(String);

impl Allow {
    pub(crate) fn parse(item: &str) -> Result<Self, String> {
        let item = item.to_ascii_lowercase();
        // The item must be the start of its own origin, which a path, a user name or a port that is no number is not.
        let ok = |o: &String| o.starts_with(&item) && !o.contains(".:") && !o.replacen("//*.", "//", 1).contains('*');
        match item.parse().ok().as_ref().and_then(origin).filter(ok) {
            _ if item == ANY => Ok(Self(item)),
            Some(o) => Ok(Self(o)),
            None => Err(format!("bad allowed host {item:?}")),
        }
    }

    fn allows(&self, uri: &Uri) -> bool {
        let Some(o) = origin(uri) else { return false };
        self.0 == ANY
            || self.0.split_once('*').map_or(o == self.0, |(head, tail)| o.starts_with(head) && o.ends_with(tail))
    }
}

/// True for everything that is not a public address: private, loopback, link-local (the metadata endpoints), shared,
/// reserved and multicast IPv4, and IPv6 outside 2000::/3. An IPv4-mapped IPv6 address is judged as its IPv4 address.
fn blocked(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(a) => {
            let o = a.octets();
            a.is_private() || a.is_loopback() || a.is_link_local() || a.is_multicast() || o[0] == 0 || o[0] >= 240
                || (o[0] == 100 && o[1] & 0xc0 == 64) // 100.64.0.0/10
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
                || (o[0] == 198 && o[1] & 0xfe == 18) // 198.18.0.0/15
        }
        IpAddr::V6(a) => a.segments()[0] & 0xe000 != 0x2000 || a.segments()[0] == 0x2002, // 2002::/16 embeds an IPv4 address
    }
}

static TLS: LazyLock<TlsConnector> = LazyLock::new(|| {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.into() };
    let provider = Arc::new(default_provider()); // named, because a second provider in the build would make the default ambiguous
    let config = ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap();
    Arc::new(config.with_root_certificates(roots).with_no_client_auth()).into()
});

/// The hooks of one store.
pub(crate) struct Outbound(pub(crate) Arc<Shared>);

impl WasiHttpHooks for Outbound {
    /// Timeouts are the store's 10 s deadline, so `RequestOptions` is ignored.
    fn send_request(&mut self, req: Request, _: Option<RequestOptions>, _: Fut<()>) -> Fut<(Response, Fut<()>)> {
        Box::new(send(self.0.clone(), req))
    }
}

async fn send(app: Arc<Shared>, req: Request) -> Result<(Response, Fut<()>), Error> {
    let uri = req.uri();
    // A user name before an `@` is guest text that would reach the `Host` header. Credentials go in `Authorization`.
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        return Err(Error::HttpRequestUriInvalid);
    }
    if !app.allow.iter().any(|a| a.allows(uri)) {
        return Err(Error::HttpRequestDenied);
    }
    let tls = uri.scheme_str() == Some("https");
    let host = uri.host().unwrap_or_default().trim_matches(['[', ']']); // `[::1]` to `::1`, which `lookup_host` takes as it is
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    // Resolve here, judge every address, and dial only those addresses: there is no second lookup to rebind.
    let found = lookup_host((host, port)).await.map_err(|_| Error::DnsError { rcode: None, info_code: None })?;
    let addrs: Vec<_> = found.collect();
    if addrs.iter().any(|a| blocked(a.ip())) {
        return Err(Error::DestinationIpProhibited);
    }
    let tcp = TcpStream::connect(&addrs[..]).await.map_err(Error::Connect)?;
    if !tls {
        return exchange(tcp, req).await;
    }
    let name = ServerName::try_from(host.to_owned()).map_err(|_| Error::HttpRequestUriInvalid)?;
    exchange(TLS.connect(name, tcp).await.map_err(Error::Tls)?, req).await
}

async fn exchange<T: AsyncRead + AsyncWrite + Send + Unpin + 'static>(
    io: T,
    mut req: Request,
) -> Result<(Response, Fut<()>), Error> {
    let (mut sender, conn) = http1::handshake(TokioIo::new(io)).await?;
    // The driver feeds the body, so it lives as long as the body does: Wasmtime drops the io future early when a p3 guest
    // drops its transmit result. Dropping the body, or this future before there is one, aborts it.
    let driver = wasmtime_wasi::runtime::spawn(conn);
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str()); // the wire wants the path only
    *req.uri_mut() = path.parse().map_err(|_| Error::HttpRequestUriInvalid)?;
    let res = sender.send_request(req).await?;
    let keep = move |e| {
        let _ = &driver;
        Error::from(e)
    };
    Ok((res.map(|b| b.map_err(keep).boxed_unsync()), Box::new(std::future::ready(Ok(())))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Empty;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn blocked_table() {
        let bad = [
            "127.0.0.1",
            "127.255.255.254",
            "::1",
            "::ffff:127.0.0.1",
            "169.254.169.254",
            "169.254.170.2",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "::",
            "255.255.255.255",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "64:ff9b::7f00:1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "::ffff:10.0.0.1",
            "ff02::1",
            "2002:7f00:1::1",
            "198.18.0.1",
            "192.0.0.170",
            "::ffff:0:127.0.0.1",
        ];
        let ok = [
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "100.63.255.255",
            "192.169.0.1",
            "198.20.0.1",
            "223.255.255.255",
            "2a00:1450:4001::1",
        ];
        for s in bad {
            assert!(blocked(s.parse().unwrap()), "{s} must be blocked");
        }
        for s in ok {
            assert!(!blocked(s.parse().unwrap()), "{s} must be allowed");
        }
    }

    #[test]
    fn allow_matcher() {
        let t = |item: &str, yes: &[&str], no: &[&str]| {
            let a = Allow::parse(item).unwrap();
            for u in yes {
                assert!(a.allows(&u.parse().unwrap()), "{item} should allow {u}");
            }
            for u in no {
                assert!(!a.allows(&u.parse().unwrap()), "{item} should deny {u}");
            }
        };
        let https = "https://example.com";
        t(
            https,
            &["https://example.com/x", "https://example.com:443/", "https://EXAMPLE.com/", "https://example.com?q=1"],
            &[
                "http://example.com/",
                "https://example.com:8443/",
                "https://sub.example.com/",
                "https://example.com./",
                "https://example.com.evil.com/",
                "https://example.com@evil.com/",
                "https://evilexample.com/",
            ],
        );
        t(https, &["https://evil.com@example.com/"], &[]); // the host is what follows the `@`
        t(
            "https://*.example.com",
            &["https://a.example.com/", "https://a.b.example.com/", "https://A.Example.COM/"],
            &[
                "https://example.com/",
                "https://evilexample.com/",
                "https://example.com.evil.com/",
                "http://a.example.com/",
            ],
        );
        t("*://*:*", &["http://1.2.3.4:81/", "https://[::1]/", "http://localhost/"], &["ftp://x/"]);
        t(
            "http://localhost:3000",
            &["http://localhost:3000/a"],
            &["http://localhost/", "http://localhost:3001/", "https://localhost:3000/"],
        );
        t("https://api.example.com:8443", &["https://api.example.com:8443/"], &["https://api.example.com/"]);
        t("https://*.example.com:8443", &["https://a.example.com:8443/"], &["https://a.example.com/"]);
        t("http://[2606:4700::1111]:80", &["http://[2606:4700::1111]/"], &["http://[2606:4700::1112]/"]);
        t("HTTPS://Example.COM", &["https://example.com/"], &[]);
        t(
            "https://example.com:443",
            &["https://example.com/", "https://example.com:443/"],
            &["https://example.com:80/"],
        );
        for bad in [
            "example.com",
            "*",
            "",
            "https://",
            "https://*",  // dropped: no wildcard host on its own
            "https://*.", // would match every name with a trailing dot
            "https://a*.example.com",
            "https://*.*.com",
            "https://example.com/", // no trailing slash
            "https://example.com/path",
            "https://example.com?q=1",
            "https://example.com:99999",
            "https://example.com:*", // dropped: no wildcard port
            "*://example.com",       // dropped: no wildcard scheme
            "https://*:8443",        // dropped
            "http://*:*",            // dropped: spell it `*://*:*`
            "https://user@example.com",
            "redis://example.com:6379",
            "ftp://example.com",
        ] {
            assert!(Allow::parse(bad).is_err(), "{bad:?} should not parse");
        }
        // an address parses, and the address rule still applies when it is dialled
        for ok in ["https://127.0.0.1:8080", "https://[::1]"] {
            assert!(Allow::parse(ok).is_ok(), "{ok:?} should parse");
        }
    }

    /// The one path the suite cannot reach through `send`, as every local address is blocked: a request goes out in
    /// origin form, and the response comes back whole, even when the io future is dropped at once (Wasmtime does that
    /// when a p3 guest drops its transmit result).
    #[tokio::test]
    async fn exchange_speaks_http1() {
        const BODY: usize = 64 << 10; // far more than the pipe holds
        let (client, mut server) = tokio::io::duplex(1024);
        let body = Empty::new().map_err(|n| match n {}).boxed_unsync();
        let req = hyper::Request::get("http://host/p?q=1").body(body).unwrap();
        let peer = tokio::spawn(async move {
            let mut head = [0; 64];
            let n = server.read(&mut head).await.unwrap();
            server.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {BODY}\r\n\r\n").as_bytes()).await.unwrap();
            server.write_all(&vec![b'x'; BODY]).await.unwrap();
            head[..n].starts_with(b"GET /p?q=1 HTTP/1.1\r\n")
        });
        let (res, io) = exchange(client, req).await.unwrap();
        drop(io);
        assert_eq!(res.status(), 200);
        assert_eq!(res.into_body().collect().await.unwrap().to_bytes().len(), BODY);
        assert!(peer.await.unwrap(), "the request line");
    }
}
