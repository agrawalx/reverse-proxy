use std::net::SocketAddr;
use std::time::Duration;
use std::num::NonZeroUsize;
use hyper::header::HOST;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, body::Bytes};
use hyper_util::rt::TokioIo;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use http_body_util::{BodyExt, Empty, Full};

mod lru_cache;
use lru_cache::LruCache;

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct CacheKey  {
    pub method: String,
    pub host: String,
    pub path: String,
    pub query: Option<String>,
}

pub enum CacheMessage {
    Get {
        key: CacheKey,
        reply_to: oneshot::Sender<Option<Bytes>>,
    },
    Put {
        key: CacheKey,
        value: Bytes,
    },
    EvictExpired,
}

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
) -> Result<Response<Full<Bytes>>, hyper::Error> {
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
            return Ok(Response::new(Full::new(cached_bytes)));
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

    // Copy original headers and remove Host (the backend expects its own host)
    *backend_req.headers_mut() = parts.headers;
    backend_req.headers_mut().remove(hyper::header::HOST);

    // Forward the request via pooled client
    let backend_res = match client.request(backend_req).await {
        Ok(res) => res,
        Err(err) => {
            eprintln!("Error connecting to backend: {}", err);
            let mut res = Response::new(Full::new(Bytes::from("502 Bad Gateway\n")));
            *res.status_mut() = hyper::StatusCode::BAD_GATEWAY;
            return Ok(res);
        }
    };
    
    // Read the response body
    let (parts, body) = backend_res.into_parts();
    let body_bytes = body.collect().await?.to_bytes();

    if can_cache {
        // 4. Send Put message to map the URL to the new Body in our Cache Actor
        let _ = cache_tx.send(CacheMessage::Put {
            key: cache_key,
            value: body_bytes.clone(),
        }).await;
    }

    // 5. Send backend body down to the user
    let mut res = Response::new(Full::new(body_bytes));
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
    let (cache_tx, mut cache_rx) = tokio::sync::mpsc::channel::<CacheMessage>(100);

    // Spawn periodic evictor task
    let cache_tx_evict = cache_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let _ = cache_tx_evict.send(CacheMessage::EvictExpired).await;
        }
    });

    // Spawn Cache Actor task
    tokio::spawn(async move {
        // Initialize the LRU Cache with a capacity
        let capacity = NonZeroUsize::new(1000).unwrap();
        let mut cache: LruCache<CacheKey, Bytes> = LruCache::new(capacity);
        let default_ttl = Duration::from_secs(60);

        while let Some(msg) = cache_rx.recv().await {
            match msg {
                CacheMessage::Get { key, reply_to } => {
                    let mut cached_value = None;
                    if let Some(val) = cache.get(&key) {
                        cached_value = Some(val.clone());
                    }
                    let _ = reply_to.send(cached_value); 
                }
                CacheMessage::Put { key, value } => {
                    cache.put(key, value, default_ttl);
                }
                CacheMessage::EvictExpired => {
                    let count = cache.evict_expired();
                    if count > 0 {
                        println!("Evicted {} expired cache entries", count);
                    }
                }
            }
        }
    });
    // connection pooling(very imp since TLS handshakes are expensive)
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    connector.set_keepalive(Some(Duration::from_secs(60)));
    connector.set_connect_timeout(Some(Duration::from_secs(60)));
    let client: HttpClient = Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .build(connector); 

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let cache_tx_clone = cache_tx.clone();
        let client_clone = client.clone();

        // Spawn a tokio task to serve requests concurrently from this TCP stream (Keep-Alive)
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| {
                    proxy_handler(req, cache_tx_clone.clone(), client_clone.clone())
                }))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}
