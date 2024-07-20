use std::time::{Duration, Instant};

async fn proxy_latency_check(proxy_port: u16) -> Result<Duration, Box<dyn std::error::Error>> {
    let connection_test_url = "https://1.1.1.1/cdn-cgi/trace";
    let start = Instant::now();
    let socks5_url = reqwest::Url::parse(&*format!("socks5://127.0.0.1:{:?}", proxy_port).to_string()).unwrap();
    let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(socks5_url).unwrap())
            .build()
            .unwrap();
    let _ = client.get(connection_test_url).send().await.unwrap();
    let duration = start.elapsed();
    return Ok(duration);
}