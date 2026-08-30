pub mod http;
pub mod issue;
pub mod keyring;
pub mod request;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::configs::{Config, secrets};
use crate::core::agent::Agent;
use crate::core::scheduler::Notifier;
use crate::error::{JumabekError, JumabekResult};
use keyring::Keyring;
use request::{Accepted, Kind};

const BIND: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct Inbox {
    agent: Arc<Agent>,
    notifier: Arc<dyn Notifier>,
    keyring: std::sync::RwLock<Arc<Keyring>>,
    port: u16,
    ask_timeout: std::time::Duration,
}

impl Inbox {
    pub fn build(
        config: &Config,
        agent: Arc<Agent>,
        notifier: Arc<dyn Notifier>,
    ) -> Option<(Inbox, Vec<String>)> {
        if !config.inbox.enabled {
            return None;
        }

        let keyring = Keyring::build(&secrets::inbox_tokens(), &config.inbox.grants);
        let mut problems = keyring.problems();

        if keyring.is_empty() {
            problems.push(
                "inbox is enabled but no usable token is configured — nothing can knock"
                    .to_string(),
            );
        }

        Some((
            Inbox {
                agent,
                notifier,
                keyring: std::sync::RwLock::new(Arc::new(keyring)),
                port: config.inbox.port,
                ask_timeout: std::time::Duration::from_secs(config.inbox.ask_timeout_sec),
            },
            problems,
        ))
    }

    pub fn callers(&self) -> Vec<String> {
        self.current()
            .names()
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn current(&self) -> Arc<Keyring> {
        Arc::clone(&self.keyring.read().expect("keyring lock"))
    }

    pub fn reload_keyring(
        &self,
        grants: &std::collections::BTreeMap<String, crate::core::task::Grant>,
    ) -> Result<Vec<String>, Vec<String>> {
        let rebuilt = Keyring::build(&secrets::inbox_tokens(), grants);
        let problems = rebuilt.problems();
        let names: Vec<String> = rebuilt.names().into_iter().map(str::to_string).collect();

        *self.keyring.write().expect("keyring lock") = Arc::new(rebuilt);

        if problems.is_empty() {
            Ok(names)
        } else {
            Err(problems)
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub async fn serve(self: Arc<Self>) -> JumabekResult<()> {
        let address = SocketAddr::new(BIND, self.port);
        let listener = TcpListener::bind(address).await.map_err(|e| {
            JumabekError::ConfigError(format!(
                "cannot listen on {} — is something else using that port? {}",
                address, e
            ))
        })?;

        let (queue, mut waiting) = unbounded_channel::<Accepted>();
        let worker = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(accepted) = waiting.recv().await {
                worker.handle_notify(accepted).await;
            }
        });

        loop {
            let (stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    self.notifier.notify(format!("  x inbox: {}", e));
                    continue;
                }
            };

            let served = Arc::clone(&self);
            let queue = queue.clone();
            tokio::spawn(async move { served.serve_one(stream, queue).await });
        }
    }

    async fn serve_one(&self, mut stream: TcpStream, queue: UnboundedSender<Accepted>) {
        let (status, body) = match self.read_request(&mut stream, queue).await {
            Ok(pair) => pair,
            Err((status, why)) => (status, http::json_message("error", why.as_str())),
        };

        let _ = stream
            .write_all(http::response(status, &body).as_bytes())
            .await;
        let _ = stream.shutdown().await;
    }

    async fn read_request(
        &self,
        stream: &mut TcpStream,
        queue: UnboundedSender<Accepted>,
    ) -> Result<(u16, String), (u16, String)> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 2048];

        let head_end = loop {
            if let Some(at) = find_double_crlf(&buffer) {
                break at;
            }
            if buffer.len() > http::MAX_HEADER_BYTES {
                return Err((413, "headers are too large".to_string()));
            }

            let read = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk))
                .await
                .map_err(|_| (408, "took too long to send a request".to_string()))?
                .map_err(|e| (400, e.to_string()))?;

            if read == 0 {
                return Err((
                    400,
                    "connection closed before a request arrived".to_string(),
                ));
            }
            buffer.extend_from_slice(&chunk[..read]);
        };

        let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
        let parsed =
            http::parse_head(&head).map_err(|bad| (bad.status(), bad.why().to_string()))?;

        if parsed.method == "GET" && parsed.path == "/health" {
            return Ok((200, http::json_message("status", "listening")));
        }

        if parsed.method == "GET" && parsed.path == "/agents" {
            let running = self.agent.agents().snapshot().await;
            return Ok((200, crate::core::agents::as_json(&running)));
        }

        if parsed.method != "POST" {
            return Err((405, "only POST is accepted".to_string()));
        }

        let route = match parsed.path.as_str() {
            "/notify" => Kind::Notify,
            "/ask" => Kind::Ask,
            other => return Err((404, format!("no route {}", other))),
        };

        let keyring = self.current();
        let caller = keyring
            .admit(parsed.token())
            .map_err(|e| (401, e.to_string()))?;

        let mut body = buffer[head_end + 4..].to_vec();
        while body.len() < parsed.content_length {
            let read = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk))
                .await
                .map_err(|_| (408, "took too long to send the body".to_string()))?
                .map_err(|e| (400, e.to_string()))?;

            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }

        let text = String::from_utf8_lossy(&body[..body.len().min(parsed.content_length)]);
        let mut accepted = request::accept(&text, caller.grant.clone())
            .map_err(|refusal| (400, refusal.to_string()))?;

        accepted.kind = route;

        match route {
            Kind::Notify => {
                let queued = format!("queued from {}", accepted.source);
                queue
                    .send(accepted)
                    .map_err(|_| (500, "the queue is gone".to_string()))?;
                Ok((200, http::json_message("status", &queued)))
            }
            Kind::Ask => self.answer(accepted).await,
        }
    }

    async fn answer(&self, accepted: Accepted) -> Result<(u16, String), (u16, String)> {
        self.notifier.notify(format!(
            "  · inbox · {} asks: {}",
            accepted.source,
            first_line(&accepted.text)
        ));

        let running = self.agent.run_detached(
            accepted.as_task(),
            accepted.grant.clone(),
            accepted.origin(),
        );

        match tokio::time::timeout(self.ask_timeout, running).await {
            Ok(Ok(reply)) => Ok((200, http::json_message("reply", &reply))),
            Ok(Err(e)) => Err((500, e.to_string())),
            Err(_) => Err((
                408,
                "the task is still running; it was not abandoned".to_string(),
            )),
        }
    }

    async fn handle_notify(&self, accepted: Accepted) {
        self.notifier.notify(format!(
            "  · inbox · {} · {}",
            accepted.source,
            first_line(&accepted.text)
        ));

        match self
            .agent
            .run_detached(
                accepted.as_task(),
                accepted.grant.clone(),
                accepted.origin(),
            )
            .await
        {
            Ok(reply) if !reply.trim().is_empty() => {
                for line in reply.lines() {
                    self.notifier.notify(format!("  | {}", line));
                }
            }
            Ok(_) => {}
            Err(e) => self
                .notifier
                .notify(format!("  x inbox · {} · {}", accepted.source, e)),
        }
    }
}

fn find_double_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(90)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bind_address_is_loopback_and_cannot_be_configured() {
        assert!(BIND.is_loopback(), "the inbox was opened to the network");
    }

    #[test]
    fn the_end_of_the_headers_is_found() {
        assert_eq!(find_double_crlf(b"GET / HTTP/1.1\r\n\r\nbody"), Some(14));
        assert_eq!(find_double_crlf(b"no end here"), None);
    }

    #[test]
    fn a_long_first_line_is_cut_for_the_log() {
        let line = first_line(&format!("{}\nsecond", "x".repeat(200)));
        assert_eq!(line.chars().count(), 90);
        assert!(!line.contains("second"));
    }
}
