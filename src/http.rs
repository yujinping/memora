//! 交接 CLI 的 HTTP 客户端（P4.3）。
//!
//! @author yujinping
//! @intent 见 docs/handoff-integration-design.md §5.4：基于 ureq（blocking + rustls）
//!         的通用 REST 客户端，支持 http/https、自定义 host、路径前缀与超时。
//!         `daemon` 的手写本机探测**不被替换**——它只探 127.0.0.1，TLS 无用；
//!         两套客户端职责不同，重复是有界的。
//!
//! 错误约定：所有失败统一返回带上下文的 `String`（含服务端地址）——CLI 的
//! 错误信息是给人看的排障线索，`Result<_, E>` 的类型分层在此没有消费者。

use std::time::Duration;

/// 服务端地址（scheme + host + port + 可选路径前缀）。
///
/// @intent `--server` 允许带路径前缀（如 `https://mem.example.com/memora`），
///         请求时拼到 `/api/v1/...` 之前；端口缺省按 scheme 补齐。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// 协议（http / https）
    pub scheme: Scheme,
    /// 主机名或 IP
    pub host: String,
    /// 端口（http 缺省取配置的 PORT；https 缺省 443）
    pub port: u16,
    /// 路径前缀（如 `/memora`），不含尾斜杠；空串 = 无前缀
    pub prefix: String,
}

/// 协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// 明文 HTTP（本机 / 内网 / SSH 隧道）
    Http,
    /// HTTPS（公网直连；rustls + WebPKI 根证书，不依赖系统 CA 配置）
    Https,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }

    /// 该协议的缺省端口。
    fn default_port(self, default_http_port: u16) -> u16 {
        match self {
            Scheme::Http => default_http_port,
            Scheme::Https => 443,
        }
    }
}

/// 非 2xx 响应（状态码 + 响应体）。
///
/// @intent 300（歧义候选）、400（可修正错误）、404 都携带调用方需要的信息，
///         故作为**正常返回值**而非错误——与 MCP 的错误码分流同源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// HTTP 状态码
    pub status: u16,
    /// 响应体原文（JSON）
    pub body: String,
}

impl Endpoint {
    /// 解析 `--server` 取值；`raw` 为空时取本机缺省地址。
    ///
    /// 语法：`http://host[:port]` 或 `https://host[:port][/prefix]`。
    /// 缺 scheme 视为用法错误（不猜测），避免 `--server mem.example.com`
    /// 被静默当作 http 明文发往公网。
    pub fn parse(raw: &str, default_http_port: u16) -> Result<Endpoint, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(Endpoint {
                scheme: Scheme::Http,
                host: "127.0.0.1".to_string(),
                port: default_http_port,
                prefix: String::new(),
            });
        }

        let (scheme, rest) = if let Some(rest) = raw.strip_prefix("http://") {
            (Scheme::Http, rest)
        } else if let Some(rest) = raw.strip_prefix("https://") {
            (Scheme::Https, rest)
        } else {
            return Err(format!(
                "`--server {raw}`: expected `http://host[:port]` or `https://host[:port][/prefix]`"
            ));
        };

        // host[:port][/prefix]：路径前缀只出现在 host:port 之后
        let (authority, prefix) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return Err(format!("`--server {raw}`: host is empty"));
        }

        let (host, port) = match authority.rsplit_once(':') {
            // IPv6 字面量不在此支持（自托管场景是域名 / IPv4）；含多个冒号时按字面量报错
            Some((host, port_raw)) if !port_raw.is_empty() && !host.contains(':') => {
                let port = port_raw.parse::<u16>().map_err(|_| {
                    format!("`--server {raw}`: invalid port `{port_raw}` (expected 1..=65535)")
                })?;
                if port == 0 {
                    return Err(format!(
                        "`--server {raw}`: port 0 is not a valid server port"
                    ));
                }
                (host.to_string(), port)
            }
            _ => (
                authority.to_string(),
                scheme.default_port(default_http_port),
            ),
        };

        Ok(Endpoint {
            scheme,
            host,
            port,
            prefix: prefix.to_string(),
        })
    }

    /// 无路径前缀的 base（`scheme://host:port`）。
    pub fn base(&self) -> String {
        format!("{}://{}:{}", self.scheme.as_str(), self.host, self.port)
    }

    /// 拼接 API 路径（base + 前缀 + path）。
    pub fn url(&self, path: &str) -> String {
        format!("{}{}{}", self.base(), self.prefix, path)
    }

    /// 是否以明文 HTTP 连向非本机地址（token / 交接数据将明文暴露）。
    ///
    /// @intent 本机（127.0.0.1 / localhost / ::1）或经 SSH 隧道时明文 http 是既定用法
    ///         （docs/handoff-integration-design.md §5.4），无需告警；连向其它地址则
    ///         `ADMIN_TOKEN` 与全部交接内容将以明文在网络上传输，必须醒目提示改用
    ///         https 或隧道。仅供 `handoff_cli` 决策是否弹告警，不改任何连接行为。
    pub fn is_plaintext_to_remote(&self) -> bool {
        match self.scheme {
            Scheme::Https => false,
            Scheme::Http => !is_loopback_host(&self.host),
        }
    }
}

/// host 是否为本机回环地址（连向这些地址的明文 http 视为安全，无需告警）。
fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host == "::1" || host == "127.0.0.1" || host.starts_with("127.")
}

/// 阻塞式 REST 客户端（持有 `ADMIN_TOKEN`，全部请求带 Bearer 头）。
pub struct Client {
    endpoint: Endpoint,
    token: String,
    timeout: Duration,
}

impl Client {
    /// 构造客户端。
    pub fn new(endpoint: Endpoint, token: String, timeout: Duration) -> Client {
        Client {
            endpoint,
            token,
            timeout,
        }
    }

    /// 发起一次请求；非 2xx 以 [`Response`] 返回，网络层故障以 `Err` 返回。
    ///
    /// @intent `http_status_as_error(false)` 是关键开关：300（歧义候选）与 404
    ///         都携带调用方需要的信息，必须在正常返回值里透传给上层分流。
    ///         ureq 3 的带体 / 无体请求是不同类型状态，故在 `body` 分支内各自收口。
    pub fn send(&self, method: &str, path: &str, body: Option<&str>) -> Result<Response, String> {
        let url = self.endpoint.url(path);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // 300 Multiple Choices（歧义候选）携带调用方需要的信息，
            // 必须原样透传，绝不能被当作重定向去跟随
            .max_redirects(0)
            .timeout_global(Some(self.timeout))
            .build()
            .new_agent();
        let auth = format!("Bearer {}", self.token);

        let mut response = match body {
            Some(json) => agent
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(json.as_bytes())
                .map_err(|err| format!("request to {} failed: {err}", self.endpoint.base()))?,
            None => {
                let request = match method {
                    "GET" => agent.get(&url).header("Authorization", &auth),
                    "DELETE" => agent.delete(&url).header("Authorization", &auth),
                    other => return Err(format!("unsupported HTTP method: {other}")),
                };
                request
                    .call()
                    .map_err(|err| format!("request to {} failed: {err}", self.endpoint.base()))?
            }
        };

        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().map_err(|err| {
            format!(
                "reading response from {} failed: {err}",
                self.endpoint.base()
            )
        })?;
        Ok(Response { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------ Endpoint::parse

    #[test]
    fn http_url_with_explicit_port() {
        let ep = Endpoint::parse("http://127.0.0.1:6790", 6789).unwrap();
        assert_eq!(ep.scheme, Scheme::Http);
        assert_eq!(ep.host, "127.0.0.1");
        assert_eq!(ep.port, 6790);
        assert_eq!(ep.prefix, "");
        assert_eq!(ep.base(), "http://127.0.0.1:6790");
    }

    #[test]
    fn http_default_port_comes_from_config() {
        let ep = Endpoint::parse("http://127.0.0.1", 6789).unwrap();
        assert_eq!(ep.port, 6789, "http 缺省端口应取配置的 PORT");
    }

    #[test]
    fn https_defaults_to_443() {
        let ep = Endpoint::parse("https://mem.example.com", 6789).unwrap();
        assert_eq!(ep.scheme, Scheme::Https);
        assert_eq!(ep.port, 443);
        assert_eq!(ep.base(), "https://mem.example.com:443");
    }

    #[test]
    fn https_with_explicit_port_and_path_prefix() {
        let ep = Endpoint::parse("https://mem.example.com:8443/memora/", 6789).unwrap();
        assert_eq!(ep.port, 8443);
        assert_eq!(ep.prefix, "/memora", "尾斜杠应被剥掉");
        assert_eq!(
            ep.url("/api/v1/projects"),
            "https://mem.example.com:8443/memora/api/v1/projects"
        );
    }

    #[test]
    fn empty_server_means_local_default() {
        let ep = Endpoint::parse("", 6789).unwrap();
        assert_eq!(ep, Endpoint::parse("http://127.0.0.1:6789", 6789).unwrap());
    }

    #[test]
    fn missing_or_unknown_scheme_is_rejected() {
        // 缺 scheme 不猜：`--server mem.example.com` 被静默当 http 明文发公网是最坏结果
        assert!(Endpoint::parse("mem.example.com", 6789).is_err());
        assert!(Endpoint::parse("ftp://mem.example.com", 6789).is_err());
        assert!(
            Endpoint::parse("https://", 6789).is_err(),
            "空 host 必须报错"
        );
    }

    // ------------------------------------------------------ is_plaintext_to_remote

    #[test]
    fn loopback_http_is_not_flagged_as_plaintext_remote() {
        for host in ["http://127.0.0.1:6789", "http://localhost:6789"] {
            let ep = Endpoint::parse(host, 6789).unwrap();
            assert!(
                !ep.is_plaintext_to_remote(),
                "{host} 是本机地址，明文 http 属既定用法，不应告警"
            );
        }
    }

    #[test]
    fn remote_http_is_flagged_as_plaintext_remote() {
        for host in [
            "http://memora.example.com:6789",
            "http://10.0.0.5:6789",
            "http://192.168.1.10:6789",
        ] {
            let ep = Endpoint::parse(host, 6789).unwrap();
            assert!(
                ep.is_plaintext_to_remote(),
                "{host} 非本机，明文 http 会暴露 token，必须告警"
            );
        }
    }

    #[test]
    fn https_is_never_flagged_as_plaintext_remote() {
        let ep = Endpoint::parse("https://memora.example.com", 6789).unwrap();
        assert!(!ep.is_plaintext_to_remote());
    }

    // ------------------------------------------------------ Client（真 socket）

    /// 极简 HTTP 服务桩：顺序应答固定响应，并回传「实际收到的请求行与头」。
    ///
    /// @intent 用真 socket 而不是 mock：被测的是「请求确实带上了 Authorization /
    ///         Content-Type、状态码与响应体原样透传」这一整条链路。
    fn serve_once(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = std::io::Read::read(&mut stream, &mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                // 请求完整性以「头结束 + Content-Length 字节」为准：
                // GET 无 body，头读完即完整；POST 还须收满 Content-Length。
                if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                    let content_length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
            }
            let resp = format!(
                "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            std::io::Write::write_all(&mut stream, resp.as_bytes()).ok();
            buf
        });
        (format!("http://{}", addr), handle)
    }

    #[test]
    fn get_carries_bearer_header_and_returns_status_and_body() {
        let (server, handle) = serve_once("HTTP/1.1 200 OK", r#"{"handoffs":[]}"#);
        let ep = Endpoint::parse(&server, 0).unwrap();
        let client = Client::new(ep.clone(), "tok-123".into(), Duration::from_secs(5));
        let resp = client
            .send("GET", "/api/v1/projects/demo/handoffs", None)
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, r#"{"handoffs":[]}"#);

        let raw = String::from_utf8(handle.join().unwrap()).unwrap();
        assert!(
            raw.starts_with("GET /api/v1/projects/demo/handoffs HTTP/1.1"),
            "请求行：{raw}"
        );
        // HTTP 头名不区分大小写（ureq 发小写头），断言须按小写比对
        assert!(
            raw.to_lowercase().contains("authorization: bearer tok-123"),
            "请求头：{raw}"
        );
    }

    #[test]
    fn post_carries_json_body_and_content_type() {
        let (server, handle) = serve_once("HTTP/1.1 201 Created", r#"{"task_id":"current@zed"}"#);
        let ep = Endpoint::parse(&server, 0).unwrap();
        let client = Client::new(ep, "tok".into(), Duration::from_secs(5));
        let resp = client
            .send(
                "POST",
                "/api/v1/projects/demo/handoffs",
                Some(r#"{"goal":"g"}"#),
            )
            .unwrap();
        assert_eq!(resp.status, 201, "201 也属于正常返回");

        let raw = String::from_utf8(handle.join().unwrap()).unwrap();
        assert!(
            raw.starts_with("POST /api/v1/projects/demo/handoffs HTTP/1.1"),
            "请求行：{raw}"
        );
        assert!(
            raw.to_lowercase()
                .contains("content-type: application/json"),
            "请求头：{raw}"
        );
        assert!(raw.contains(r#"{"goal":"g"}"#), "请求体：{raw}");
    }

    /// 非 2xx（300 歧义候选 / 404 / 400）是**正常返回值**，状态码与响应体原样透传。
    #[test]
    fn non_2xx_responses_are_returned_not_errors() {
        let (server, _handle) = serve_once(
            "HTTP/1.1 300 Multiple Choices",
            r#"{"status":"ambiguous","candidates":[]}"#,
        );
        let ep = Endpoint::parse(&server, 0).unwrap();
        let client = Client::new(ep, "tok".into(), Duration::from_secs(5));
        let resp = client
            .send("GET", "/api/v1/projects/demo/handoffs/pending", None)
            .unwrap();
        assert_eq!(resp.status, 300);
        assert!(resp.body.contains("ambiguous"));
    }

    /// 连接失败（端口无人监听）必须报错，且错误信息包含服务端地址。
    #[test]
    fn connection_failure_includes_server_address() {
        // 占用一个端口后立刻释放：无人监听，连接必然被拒
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let ep = Endpoint::parse(&format!("http://{addr}"), 0).unwrap();
        let client = Client::new(ep.clone(), "tok".into(), Duration::from_secs(2));
        let err = client.send("GET", "/api/v1/projects", None).unwrap_err();
        assert!(err.contains(&ep.base()), "错误信息应含服务端地址：{err}");
    }

    /// 超时也是连接类故障（服务端收下连接但不响应）。
    #[test]
    fn unresponsive_server_times_out() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // 只 accept 不回话
        std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(5));
        });

        let ep = Endpoint::parse(&format!("http://{addr}"), 0).unwrap();
        let client = Client::new(ep, "tok".into(), Duration::from_secs(1));
        let started = std::time::Instant::now();
        let err = client.send("GET", "/api/v1/projects", None).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "超时应大致按配置生效，实际耗时 {:?}",
            started.elapsed()
        );
        assert!(!err.is_empty());
    }
}
