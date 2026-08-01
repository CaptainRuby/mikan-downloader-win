use crate::models::ProxyMode;
use std::{io::Read, time::Duration};

pub const RSS_MAX_BYTES: usize = 5 * 1024 * 1024;
pub const TORRENT_MAX_BYTES: usize = 20 * 1024 * 1024;

pub struct HttpClient {
    inner: reqwest::blocking::Client,
}

impl HttpClient {
    pub fn new(proxy_mode: &ProxyMode) -> Result<Self, String> {
        let mut builder = reqwest::blocking::Client::builder()
            .user_agent(format!("MikanRssDownloader/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30));
        if matches!(proxy_mode, ProxyMode::NoProxy) {
            builder = builder.no_proxy();
        }
        let inner = builder
            .build()
            .map_err(|error| format!("HTTP client could not be created: {error}"))?;
        Ok(Self { inner })
    }

    pub fn get_bytes(&self, url: &str, label: &str, limit: usize) -> Result<Vec<u8>, String> {
        let response = self
            .inner
            .get(url)
            .send()
            .map_err(|error| format!("{label} request failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "{label} request failed: HTTP {}",
                response.status().as_u16()
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(format!("{label} response exceeds {limit} bytes"));
        }

        let capacity = response
            .content_length()
            .unwrap_or_default()
            .min(limit as u64) as usize;
        let mut bytes = Vec::with_capacity(capacity);
        response
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("{label} response could not be read: {error}"))?;
        if bytes.len() > limit {
            return Err(format!("{label} response exceeds {limit} bytes"));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::HttpClient;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    #[test]
    fn rejects_chunked_response_over_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n123456")
                .unwrap();
        });

        let client = HttpClient {
            inner: reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .unwrap(),
        };
        let error = client
            .get_bytes(&format!("http://{address}"), "Test", 5)
            .unwrap_err();
        assert!(error.contains("exceeds 5 bytes"));
        server.join().unwrap();
    }

    #[test]
    fn times_out_stalled_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_millis(200));
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        });

        let client = HttpClient {
            inner: reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_millis(50))
                .build()
                .unwrap(),
        };
        let error = client
            .get_bytes(&format!("http://{address}"), "Test", 5)
            .unwrap_err();
        assert!(error.contains("request failed"));
        server.join().unwrap();
    }
}
