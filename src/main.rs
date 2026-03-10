use std::net::SocketAddr;
use std::time::Duration;
use hyper::header::{HOST, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, body::Bytes};
use hyper_util::rt::TokioIo;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use http_body_util::{BodyExt, Full, StreamBody};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use hyper::body::Frame;
use http_body_util::combinators::BoxBody;
mod lru_cache;
use lru_cache::{CacheKey, CacheMessage, spawn_cache_actor, spawn_periodic_evictor};
mod rate_limiter;
use rate_limiter::{RateLimitMessage, spawn_rate_limiter};

// Shared Client Type for Backend Forwarding via Connection Pool
type HttpClient = Client<HttpConnector, hyper::body::Incoming>;

fn resolve_backend(host: &str) -> &'static str {
    // Basic routing based on host header / hostname
    match host {
        "api.example.com" => "http://127.0.0.1:9001",
        "images.example.com" => "http://127.0.0.1:9002",
        "blog.example.com" => "http://127.0.0.1:9003",
        _ => "http://127.0.0.1:9000", // Default backend
    }
}

async fn proxy_handler(
    req: Request<hyper::body::Incoming>,
    cache_tx: mpsc::Sender<CacheMessage>,
    client: HttpClient,
    client_ip: SocketAddr,
    rate_limit_tx: mpsc::Sender<RateLimitMessage>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    // responses > 10mb won't be cached 
    const MAX_CACHE_SIZE: usize = 10 * 1024 * 1024;

    let hop_headers = [
        "connection", "keep-alive", "proxy-authenticate", 
        "proxy-authorization", "te", "trailers", 
        "transfer-encoding", "upgrade"
    ];

    let (rl_tx, rl_rx) = oneshot::channel();

    let _  = rate_limit_tx.send(RateLimitMessage::Allow {
        ip: client_ip.ip(),
        reply_to: rl_tx
    }).await; 

    if let Ok(permission) = rl_rx.await {
        match permission {
            false => {
                let mut res = Response::new(
                    Full::new(Bytes::from("429 Too many Requests\n"))
                    .map_err(|never| match never {})
                    .boxed(),
                );
                *res.status_mut() = hyper::StatusCode::TOO_MANY_REQUESTS;
                return Ok(res); 
            }
            true => {}
        }
    }

    let method = req.method().as_str().to_owned();
    let host = req.headers().get(HOST).and_then(|v| v.to_str().ok()).unwrap_or("").to_string(); 
    let path = req.uri().path().to_owned();
    let query = req.uri().query().map(|q| q.to_owned());

    let cache_key = CacheKey { method, host: host.clone(), path, query };

    let is_get_or_head = req.method() == hyper::Method::GET || req.method() == hyper::Method::HEAD;
    let has_cookie = req.headers().contains_key(hyper::header::COOKIE);
    let has_no_store = req.headers().get(hyper::header::CACHE_CONTROL)
        .map_or(false, |v| v.to_str().unwrap_or("").to_lowercase().contains("no-store"));

    let can_cache = is_get_or_head && !has_cookie && !has_no_store;

    if can_cache {
        // 1. Send request URL through cache channel 
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = cache_tx.send(CacheMessage::Get {
            key: cache_key.clone(),
            reply_to: reply_tx,
        }).await;

        // 2. Wait for Cache Hit or Miss
        if let Ok(Some(cached_bytes)) = reply_rx.await {
            println!("Cache HIT: {:?}", cache_key);
            return Ok(Response::new(Full::new(cached_bytes).map_err(|never| match never {}).boxed()));
        }

        println!("Cache MISS: {:?}", cache_key);
    } else {
        println!("Cache BYPASS: {:?}", cache_key);
    }

    // 3. Forward to backend on Miss using hyper_util Client
    // Resolve the backend based on the requested host
    
    let backend_base = resolve_backend(&host);
    let backend_uri = format!("{}{}", backend_base, req.uri().path_and_query().map(|x| x.as_str()).unwrap_or(""));
    let (parts, body) = req.into_parts();

    let mut backend_req = Request::builder()
        .method(parts.method)
        .uri(backend_uri)
        .body(body)
        .unwrap();

    
    // Copy original headers and remove Host, hop-by-hop headers(the backend expects its own host)
    *backend_req.headers_mut() = parts.headers;
    // remove hop-by-hop headers 
    for header in &hop_headers {
        backend_req.headers_mut().remove(*header);
    }

    backend_req.headers_mut().remove(hyper::header::HOST);

    backend_req.headers_mut().insert(
        "x-forwarded-for",
        HeaderValue::from_str(&client_ip.ip().to_string()).unwrap()
    );

    // Forward the request via pooled client
    let backend_res = match client.request(backend_req).await {
        Ok(res) => res,
        Err(err) => {
            eprintln!("Error connecting to backend: {}", err);
            let mut res = Response::new(Full::new(Bytes::from("502 Bad Gateway\n")).map_err(|never| match never {}).boxed());
            *res.status_mut() = hyper::StatusCode::BAD_GATEWAY;
            return Ok(res);
        }
    };

    let (parts, mut body) = backend_res.into_parts();
    let (tx, rx) = mpsc::channel(16);
    // this is the stream that's passed to hyper which sends the bytes to client's socket 
    let body_stream = ReceiverStream::new(rx).map(Ok::<_, hyper::Error>);
    let response_body = StreamBody::new(body_stream).boxed();

    // Clone things needed for the background task
    let cache_tx_clone = cache_tx.clone();
    let cache_key_clone = cache_key.clone();
    let mut can_cache = can_cache;

    // Detect size early if Content-Length exists
    if let Some(cl) = parts.headers.get(hyper::header::CONTENT_LENGTH) {
        if let Ok(len_str) = cl.to_str() {
            if let Ok(len) = len_str.parse::<usize>() {
                if len > MAX_CACHE_SIZE { can_cache = false; }
            }
        }
    }
    // Read the response body & spawn a new background task to send response to client 
    tokio::spawn(async move {
        let mut buffer = Vec::new();
        let mut total_size = 0;
        // here we are the acting as the client to the server. 
        // we pull the data, forward it to the reciever channel and hyper automatically handles pulling that data through the response body that we have returned
        while let Some(frame_result) = body.frame().await {
            match frame_result {
                Ok(frame) => {
                    if let Some(data) = frame.data_ref() {
                        total_size += data.len();
                        
                        // 1. If still within limits, record for cache
                        if can_cache && total_size <= MAX_CACHE_SIZE {
                            buffer.extend_from_slice(data);
                        } else {
                            can_cache = false; // Exceeded limit, stop buffering
                        }

                        // 2. Forward chunk to client immediately
                        if tx.send(Frame::data(data.clone())).await.is_err() {
                            break; // Client disconnected
                        }
                    }
                    // Handle trailers if any
                    if let Some(trailers) = frame.trailers_ref() {
                        let _ = tx.send(Frame::trailers(trailers.clone())).await;
                    }
                }
                Err(_) => break,
            }
        }

        // 3. If finished and still eligible, Put in Cache
        if can_cache && total_size > 0 {
            let _ = cache_tx_clone.send(CacheMessage::Put {
                key: cache_key_clone,
                value: Bytes::from(buffer),
            }).await;
        }
    });

    // 5. we return the response stream to the client with the status code. 
    // now they listen for incoming stream and hyper automatically handles calling stream.next().await when we pipe body
    let mut res = Response::new(response_body);
    *res.headers_mut() = parts.headers;
    *res.status_mut() = parts.status;

    Ok(res)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    let listener = TcpListener::bind(addr).await?;
    println!("Reverse Proxy listening on http://{}", addr);
    // creates the channel to communicate with cache actor
    let (cache_tx, cache_rx) = tokio::sync::mpsc::channel::<CacheMessage>(100);

    spawn_periodic_evictor(cache_tx.clone());
    spawn_cache_actor(cache_rx);
    // connection pooling(very imp since TLS handshakes are expensive)
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    connector.set_keepalive(Some(Duration::from_secs(60)));
    connector.set_connect_timeout(Some(Duration::from_secs(60)));
    let client: HttpClient = Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .build(connector); 

    let rate_limit_tx = spawn_rate_limiter(30.0, 5.0);

    loop {
        let (stream, client_address) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let cache_tx_clone = cache_tx.clone();
        let client_clone = client.clone();
        let rate_limit_tx_clone = rate_limit_tx.clone();

        // Spawn a tokio task to serve requests concurrently from this TCP stream (Keep-Alive)
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| {
                    proxy_handler(req, cache_tx_clone.clone(), client_clone.clone(),client_address,rate_limit_tx_clone.clone())
                }))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}
