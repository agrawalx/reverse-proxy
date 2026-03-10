# Reverse Proxy

A high-performance, async HTTP reverse proxy written in Rust. Built on top of [Tokio](https://tokio.rs/) and [Hyper](https://hyper.rs/), this proxy demonstrates a pure **Actor Model** architecture — No shared mutable state. No mutexes. No data races. just channels 

---

## Table of Contents

1. [Architecture Overview](#architecture-overview)
2. [Actor Model — The Core](#actor-model--the-core)
3. [Actor Communication Diagrams](#actor-communication-diagrams)
4. [Streaming — Bytes, Not Buffers](#streaming--bytes-not-buffers)
5. [Leaky-Bucket Rate Limiting](#leaky-bucket-rate-limiting)
6. [LRU Cache & Memory Safety](#lru-cache--memory-safety)
7. [HTTP Headers Handling](#http-headers-handling)
8. [Graceful Shutdown](#graceful-shutdown)
9. [Getting Started](#getting-started)
10. [Future Work](#future-work)

---

## Architecture Overview

The proxy sits between any number of clients and a pool of upstream backend servers. Every incoming TCP connection is accepted in a tight `loop` on the main task, then immediately handed off to an independently spawned Tokio task — so the accept loop itself is never blocked. Each spawned task drives Hyper's `http1::Builder::serve_connection`, which handles HTTP/1.1 keep-alive, pipelining, and framing automatically.

```
┌────────────────────────────────────────────────────────────────────┐
│                          CLIENT BROWSER / curl                     │
└─────────────────────────────┬──────────────────────────────────────┘
                              │ TCP :8080
                              ▼
┌────────────────────────────────────────────────────────────────────┐
│                    TcpListener  (main accept loop)                 │
│        spawns a new Tokio task for every accepted connection       │
└──────┬──────────────────────┬──────────────────────┬──────────────┘
       │                      │                      │
 [conn task 1]          [conn task 2]          [conn task N]
       │                      │                      │
       └──────────────────────┴──────────────────────┘
                              │
                    proxy_handler(req, ...)
                              │
           ┌──────────────────┼──────────────────┐
           ▼                  ▼                  ▼
   Rate Limiter Actor    Cache Actor       hyper_util Client
   (mpsc channel)        (mpsc channel)   (connection pool)
           │                  │                  │
           │                  │        Upstream backends
           │                  │        :9000 / :9001 / :9002 / :9003
           └──────────────────┘
```

There are three separate address spaces of ownership:

- **Connection tasks** — stateless, short-lived, one per TCP connection.
- **Cache Actor** — single long-lived Tokio task that owns the entire LRU map.
- **Rate Limiter Actor** — single long-lived Tokio task that owns all per-IP leaky buckets.

Neither the cache map nor the bucket map is ever touched by connection tasks directly. The only way to interact with either is to send a typed message down a channel and, where a reply is needed, await a `oneshot` response. This is the essence of the Actor Model.

---

## Actor Model — The Core

The actor model is an architectural pattern in which the unit of concurrency is an **actor** — an autonomous entity that:

1. Owns its own private state exclusively (no outside code can reach in).
2. Communicates only by passing immutable, typed **messages**.
3. Processes messages one at a time, serially, so its internal state never needs a lock.

In this codebase every actor is a plain `tokio::spawn`ed async block that loops over a `tokio::sync::mpsc::Receiver`. When the receiver is drained or a `Shutdown` message arrives the task returns, dropping all owned state cleanly.

### Why Actor Model instead of `Arc<Mutex<T>>`?

With a mutex every connection task must acquire a lock, do its work, and release the lock. Under high concurrency that lock becomes a bottleneck: threads/tasks spin waiting for the lock to become available, and any slow operation inside the critical section punishes every other caller. Deadlock is also possible whenever two locks are acquired in different orders across call sites.

With the actor model there is no lock at all. Connection tasks send a message and either fire-and-forget (`Put`) or wait for a `oneshot` reply (`Get`, `Allow`). The actor processes those messages one at a time, so its internal data structures can be plain, non-`Sync` Rust types. Throughput scales with the channel's buffer size, not with a lock's contention window.

### Actors in this project

| Actor | Spawned by | Owns | Message type |
|---|---|---|---|
| `cache_actor` | `spawn_cache_actor` | `LruCache<CacheKey, Bytes>` | `CacheMessage` |
| `periodic_evictor` | `spawn_periodic_evictor` | timer only | sends `CacheMessage::EvictExpired` |
| `rate_limiter` | `spawn_rate_limiter` | `HashMap<IpAddr, LeakyBucket>` | `RateLimitMessage` |
| `rate_limiter_cleanup` | inside `spawn_rate_limiter` | timer only | sends `RateLimitMessage::CleanExpired` |

---

## Actor Communication Diagrams

### 1. Full request lifecycle — all actors

```
  Connection Task (per TCP conn)
  ─────────────────────────────────────────────────────────────────
  │
  │  ① sends RateLimitMessage::Allow { ip, reply_to: oneshot_tx }
  │──────────────────────────────────────────────────────────────►  Rate Limiter Actor
  │                                                                  (mpsc channel, cap 256)
  │  ② awaits oneshot_rx  ◄────────────────────────────────────────  reply: bool
  │
  │  [if false → return 429, done]
  │  [if true  → continue]
  │
  │  ③ sends CacheMessage::Get { key, reply_to: oneshot_tx }
  │──────────────────────────────────────────────────────────────►  Cache Actor
  │                                                                  (mpsc channel, cap 100)
  │  ④ awaits oneshot_rx  ◄────────────────────────────────────────  reply: Option<Bytes>
  │
  │  [Some(bytes) → return cached response, done]
  │  [None        → forward to backend]
  │
  │  ⑤ client.request(backend_req)  ──────────────────────────────► hyper_util Client
  │                                   (connection pool, keep-alive)  upstream backend
  │  ⑥ backend response arrives
  │
  │  ⑦ tokio::spawn background task ──────┐
  │  returns StreamBody to client         │ pulls frames from backend body
  │                                       │ sends Frame::data(chunk) down
  │                                       │   mpsc::channel(16)  ──► StreamBody ──► client socket
  │                                       │ accumulates buffer (if ≤ 10 MB)
  │                                       │ sends CacheMessage::Put { key, bytes }
  │                                       └──────────────────────────────────────► Cache Actor
  └──────────────────────────────────────────────────────────────────────────────────────
```

### 2. Cache actor internal message dispatch

```
  Sender (any connection task)          Cache Actor (single Tokio task)
  ─────────────────────────────         ──────────────────────────────────────────────
                                         owns: LruCache<CacheKey, Bytes>
  CacheMessage::Get { key, tx }  ──►    match msg {
                                           Get  → cache.get(&key)
                                                  tx.send(result)
  CacheMessage::Put { key, val } ──►       Put  → cache.put(key, val, 60s TTL)
                                           EvictExpired → cache.evict_expired()
  (from periodic_evictor every 60s)
                                           Shutdown → break  (task returns, cache dropped)
  CacheMessage::Shutdown         ──►    }
```

### 3. Rate limiter actor internal message dispatch

```
  Sender (any connection task)          Rate Limiter Actor (single Tokio task)
  ─────────────────────────────         ──────────────────────────────────────────────
                                         owns: HashMap<IpAddr, LeakyBucket>
  RateLimitMessage::Allow {             match msg {
    ip, reply_to: tx          ──►          Allow → buckets.entry(ip)
  }                                                   .or_insert_with(new_bucket)
                                                  bucket.allow() → tx.send(bool)
  RateLimitMessage::CleanExpired ──►      CleanExpired → buckets.retain(!is_idle)
  (from cleanup task every 5 min)
                                           Shutdown → break
  RateLimitMessage::Shutdown     ──►    }
```

### 4. Shutdown broadcast fan-out

```
  main()
    │
    │ tokio::sync::broadcast::channel::<()>(1)
    │       shutdown_tx (owned by main)
    │           │
    │           ├── .subscribe() ──► periodic_evictor task
    │           │                       (breaks loop on recv)
    │           │
    │           └── .subscribe() ──► rate_limiter_cleanup task
    │                                   (breaks loop on recv)
    │
    │ on SIGTERM / SIGINT:
    │   shutdown_tx.send(())    ←── fans out to all subscribers simultaneously
    │   cache_tx.send(Shutdown) ←── direct mpsc message to cache actor
    │   rate_limit_tx.send(Shutdown) ←── direct mpsc message to rate limiter
    └──────────────────────────────────────────────────────────────
```

### 5. Channel types at a glance

```
  ┌──────────────────────────────────────────────────────────────────────┐
  │  Channel          │  Type        │  Cap  │  Direction                │
  ├──────────────────────────────────────────────────────────────────────┤
  │  cache_tx/rx      │  mpsc        │  100  │  many conn tasks → cache  │
  │  rate_limit_tx/rx │  mpsc        │  256  │  many conn tasks → rl     │
  │  shutdown_tx      │  broadcast   │  1    │  main → evictor + cleanup │
  │  reply (cache)    │  oneshot     │  1    │  cache → conn task        │
  │  reply (rl)       │  oneshot     │  1    │  rl    → conn task        │
  │  body_tx/rx       │  mpsc        │  16   │  bg task → hyper stream   │
  │  connection_tx/rx │  mpsc        │  1    │  guard tracking (see §8)  │
  └──────────────────────────────────────────────────────────────────────┘
```

---

## Streaming — Bytes, Not Buffers

A naive proxy implementation would look like this:

```rust
// ❌  naive buffering approach
let body_bytes = body.collect().await?.to_bytes();  // blocks until ALL data arrives
let response = Response::new(Full::new(body_bytes));
```

This has two serious problems. First, the proxy must hold the entire body in memory before the first byte reaches the client. Second, for large responses (video, file downloads) memory usage grows proportionally to the size of the response, for every concurrent connection.

This project avoids both problems through a **streaming pipeline**.

### How it works

When a backend response arrives, its body is not collected. Instead:

1. An `mpsc::channel::<Frame<Bytes>>(16)` is created. The receiver end is wrapped in a `ReceiverStream` and then a `StreamBody`, which is returned to Hyper immediately as the response body. Hyper starts sending HTTP headers and the chunked body to the client's socket right away.

2. A background Tokio task is spawned. This task drives the backend body via `body.frame().await` — pulling one `Frame<Bytes>` at a time — and immediately forwards each frame to the sender end of the channel.

```rust
// ✅  streaming approach
let (tx, rx) = mpsc::channel(16);
let body_stream = ReceiverStream::new(rx).map(Ok::<_, hyper::Error>);
let response_body = StreamBody::new(body_stream).boxed();

tokio::spawn(async move {
    while let Some(frame_result) = body.frame().await {
        // chunk arrives from backend ──► forwarded to client immediately
        if tx.send(Frame::data(data.clone())).await.is_err() {
            break; // client disconnected, stop pulling from backend
        }
    }
});

return Ok(Response::new(response_body)); // returned before background task finishes
```

The channel backpressure is automatic: if the client's socket is slow (small TCP window), `tx.send(...)` will await until there is capacity, which naturally slows the background task's pull rate from the backend. No explicit throttling logic is needed.

### The caching exception

Pure streaming conflicts with caching: you cannot cache what you have not yet seen. The proxy resolves this with a dual-path strategy inside the same background task:

```
backend body frame arrives
        │
        ├── can_cache == true && total_size <= 10 MB?
        │       YES → buffer.extend_from_slice(data)   (accumulate for cache)
        │       NO  → can_cache = false                 (stop accumulating)
        │
        └── tx.send(Frame::data(data))  ──► client  (always, immediately)

after last frame:
   can_cache && total_size > 0 → CacheMessage::Put { key, Bytes::from(buffer) }
```

The client always receives a true stream. The cache store is a side-effect that happens concurrently, bounded at 10 MB. Responses larger than 10 MB are streamed without caching. The `Content-Length` header is inspected first — if it already exceeds the limit, the buffer is never allocated.

---

## Leaky-Bucket Rate Limiting

### The algorithm

The leaky bucket is an intuitive analogy: imagine a bucket with a hole in the bottom. Water (requests) drip in from the top. The hole leaks water out at a fixed rate. If the bucket overflows, the request is rejected.

Mathematically:

```
water_level_now = max(0, water_level_prev - leak_rate × elapsed_seconds)
if water_level_now + 1.0 ≤ capacity:
    water_level_now += 1.0
    allow request
else:
    reject request (429)
```

The leak is computed lazily — only when a new request arrives for that IP. This avoids the need for a background timer per bucket.

### Implementation

```rust
struct LeakyBucket {
    water:      f64,      // current fill level
    capacity:   f64,      // max fill level  (default: 30.0)
    leak_rate:  f64,      // units/second    (default: 5.0)
    last_check: Instant,  // when leak was last applied
    last_used:  Instant,  // for idle cleanup
}
```

With `capacity = 30.0` and `leak_rate = 5.0`:
- A client can burst up to 30 requests at once.
- Sustained throughput settles to 5 requests per second.
- After 6 seconds of silence the bucket drains completely and the client has full burst capacity again.

### Actor ownership prevents data races

Every `LeakyBucket` lives inside a `HashMap<IpAddr, LeakyBucket>` that is owned **exclusively** by the rate limiter actor. Connection tasks never touch the hashmap. They send a `RateLimitMessage::Allow` message and wait for a `oneshot` bool. Because the actor processes one message at a time, `bucket.allow()` is always called single-threadedly — no lock, no race, no UB.

### Memory leak prevention — idle bucket cleanup

A high-traffic proxy sees thousands of distinct client IPs over its lifetime. Buckets for clients that stopped connecting would accumulate forever if not cleaned up. A dedicated periodic cleanup task sends `RateLimitMessage::CleanExpired` to the rate limiter actor every 5 minutes:

```rust
const BUCKET_IDLE_TTL: Duration = Duration::from_secs(300);

RateLimitMessage::CleanExpired => {
    buckets.retain(|_, bucket| !bucket.is_idle());
}
```

A bucket is considered idle when `last_used.elapsed() >= 300s`. The `retain` call is O(n) and runs inside the actor — no locking, no coordination needed.

---

## LRU Cache & Memory Safety

The cache is implemented from scratch using a **doubly-linked list** (for O(1) move-to-front on access) backed by a **HashMap** (for O(1) lookup by key). The Rust standard library does not expose a safe doubly-linked list with interior pointers, so the implementation uses raw pointers with careful manual memory management.

### Data structure

```
HashMap<KeyRef<K>, NonNull<LruEntry<K,V>>>
           │                    │
           │                    └── raw pointer into heap
           └── pointer to the key field inside the node (avoids double-storing keys)

Doubly-linked list:
   head(sentinel) ←→ [most-recently-used] ←→ ... ←→ [least-recently-used] ←→ tail(sentinel)
```

Sentinel nodes (`head` and `tail`) are allocated once at construction and never hold real keys or values. They use `MaybeUninit::uninit()` to avoid constructing a `K` or `V` without a valid value.

### MaybeUninit and the memory leak problem

Rust's `MaybeUninit<T>` is a union that allocates space for `T` but does not initialize it and **does not run `T`'s destructor** when dropped. This is intentional — the programmer declares ownership of init/drop manually.

There are four distinct places where a node's `key` and `value` must be explicitly dropped:

**1. Updating an existing key** — the old value must be dropped before the new one is written, or the old allocation leaks:
```rust
// ❌  without this line, old value is overwritten without being freed
(*node_ptr).value.assume_init_drop();
(*node_ptr).value = MaybeUninit::new(v);
```

**2. Evicting the LRU entry** (`remove_lru`):
```rust
let mut entry = Box::from_raw(lru);
ptr::drop_in_place(entry.key.as_mut_ptr());
ptr::drop_in_place(entry.value.as_mut_ptr());
drop(entry);
// Box::drop runs here, freeing the heap allocation,
// but does NOT touch key/value (already dropped above)
```

**3. Lazy expiry eviction on `get`** — if an entry is found but its TTL has passed, it is evicted in-place. The same pattern applies.

**4. Bulk periodic eviction** (`evict_expired`) — walks from tail to head, collects all expired node pointers, then removes and drops each one using the same `drop_in_place` pattern.

**5. `LruCache::drop`** — the custom `Drop` impl drains the HashMap (which is the authoritative owner of all live nodes) and drops key/value for each, then frees the two sentinel nodes without calling drop on their uninit fields:
```rust
impl<K, V> Drop for LruCache<K, V> {
    fn drop(&mut self) {
        unsafe {
            self.map.drain().for_each(|(_, node)| {
                let node_ptr = node.as_ptr();
                let mut entry = *Box::from_raw(node_ptr);
                ptr::drop_in_place(entry.key.as_mut_ptr());
                ptr::drop_in_place(entry.value.as_mut_ptr());
                // entry goes out of scope here: Box is freed, but Drop is NOT run
                // for key/value (already done above)
            });
            // Sentinel nodes: do NOT call drop_in_place — fields are uninit
            let _ = *Box::from_raw(self.head);
            let _ = *Box::from_raw(self.tail);
        }
    }
}
```

### TTL-based expiry

Every cache entry carries an `expiry: Instant` field set to `Instant::now() + ttl` at insert time. Expiry is enforced in two complementary ways:

- **Lazy check on `get`**: before returning a cached value, the current time is compared against `expiry`. If expired, the entry is removed and `None` is returned.
- **Proactive sweep**: the `periodic_evictor` actor sends `CacheMessage::EvictExpired` to the cache actor every 60 seconds. The actor walks from tail (oldest) to head and removes all expired entries in a single pass.

This dual strategy ensures memory is bounded both in steady-state (periodic sweep) and on access (lazy eviction).

### Cache actor isolation

The entire `LruCache` is owned by a single Tokio task. Because `LruCache` uses raw pointers internally it does not implement `Sync`. The `Send` bound is implemented manually:

```rust
unsafe impl<K: Send, V: Send> Send for LruCache<K, V> {}
```

This is sound because the cache is always accessed from exactly one task — the actor — so there is never concurrent access.

---

## HTTP Headers Handling

### Hop-by-hop header stripping

HTTP defines two classes of headers:

- **End-to-end** headers — intended for the final recipient; must be forwarded.
- **Hop-by-hop** headers — relevant only to the immediate connection; must **not** be forwarded.

Forwarding hop-by-hop headers to a backend can cause protocol errors (e.g., forwarding `Transfer-Encoding: chunked` when the backend expects raw framing from Hyper). The proxy strips all eight standard hop-by-hop headers before forwarding:

```rust
let hop_headers = [
    "connection", "keep-alive", "proxy-authenticate",
    "proxy-authorization", "te", "trailers",
    "transfer-encoding", "upgrade",
];

for header in &hop_headers {
    backend_req.headers_mut().remove(*header);
}
```

### HOST header replacement

The `Host` header sent by the client refers to the virtual host the client is connecting to (e.g., `api.example.com`). The backend server expects its own `Host` value. Forwarding the client's `Host` would confuse backends that use it for virtual hosting or certificate validation. The proxy removes it entirely and lets Hyper set the correct value when it opens the upstream connection:

```rust
backend_req.headers_mut().remove(hyper::header::HOST);
```

### X-Forwarded-For injection

Without this header, the backend would see the proxy's address as the client IP, losing all visibility into real client origins:

```rust
backend_req.headers_mut().insert(
    "x-forwarded-for",
    HeaderValue::from_str(&client_ip.ip().to_string()).unwrap()
);
```

This allows backends to log, geo-locate, or rate-limit by the real client IP independently.

### Cache-control awareness

The proxy respects the `Cache-Control: no-store` directive. If a request carries this header the response is never stored in the LRU cache, even if all other conditions are met. Requests with a `Cookie` header are also excluded from caching, as cookie-bearing requests typically return personalised responses that must not be shared across users:

```rust
let can_cache = is_get_or_head && !has_cookie && !has_no_store;
```

Only `GET` and `HEAD` responses are cached. `POST`, `PUT`, `DELETE`, and other mutating methods always bypass the cache.

### Host-based backend routing

The `resolve_backend` function implements virtual-host routing by inspecting the `Host` header:

```rust
fn resolve_backend(host: &str) -> &'static str {
    match host {
        "api.example.com"    => "http://127.0.0.1:9001",
        "images.example.com" => "http://127.0.0.1:9002",
        "blog.example.com"   => "http://127.0.0.1:9003",
        _                    => "http://127.0.0.1:9000",
    }
}
```

---

## Graceful Shutdown

Graceful shutdown is one of the most tricky part. Dropping the listener and killing the process would leave in-flight requests half-answered, corrupt streaming bodies mid-transfer, and give actors no chance to flush state. This proxy implements a multi-phase, signal-driven shutdown sequence.

### Phase overview

```
SIGTERM / SIGINT
      │
      ▼
Phase 1 — Stop accepting new connections
   drop(listener)     ← TcpListener is closed; new requests are rejected

Phase 2 — Drain active connections (30-second timeout)
   wait until all connection guards drop
   OR tokio::time::sleep(30s) fires

Phase 3 — Signal auxiliary tasks
   broadcast shutdown_tx.send(())  ← periodic_evictor + rate_limiter_cleanup exit
   cache_tx.send(CacheMessage::Shutdown)
   rate_limit_tx.send(RateLimitMessage::Shutdown)

Phase 4 — Wait for actor tasks (5-second timeout)
   tokio::join!(cache_handle, evictor_handle, rate_limit_handle)
```

### Signal handling

The proxy registers handlers for both `SIGTERM` (sent by `systemd`, Kubernetes, etc.) and `SIGINT` (Ctrl+C) on Unix:

```rust
tokio::select! {
    _ = sigterm.recv() => println!("Received SIGTERM"),
    _ = sigint.recv()  => println!("Received SIGINT"),
}
```

On non-Unix platforms (Windows) only `Ctrl+C` is handled via `tokio::signal::ctrl_c()`.

### The Guard Pattern — tracking active connections

The key insight is that Rust's drop semantics can be weaponised as a counting mechanism. An `mpsc::channel::<()>(1)` is created. The **sender** is the "guard". A clone of the sender is moved into every spawned connection task:

```rust
let (connection_tx, mut connection_rx) = mpsc::channel::<()>(1);

// For each accepted connection:
let connection_tx_clone = connection_tx.clone();
tokio::task::spawn(async move {
    let _guard = connection_tx_clone; // ← guard is held alive by the task
    http1::Builder::new()
        .serve_connection(io, service_fn(...))
        .await
});
```

`_guard` is never explicitly used; it is a deliberately unused binding whose only purpose is to keep the clone of `connection_tx` alive for as long as the task is alive. When the task completes (connection closes), `_guard` drops, which drops the sender clone.

After the accept loop exits, the **original** `connection_tx` is also dropped:

```rust
drop(connection_tx); // drop the original so we don't hold the channel open ourselves
```

Now the channel has as many senders as there are active connections. When every connection task finishes and drops its `_guard`, all senders are gone. `connection_rx.recv()` returns `None` because an `mpsc` receiver returns `None` only when the channel is closed — which happens when **every** sender has been dropped. This is used as the drain completion signal:

```rust
tokio::select! {
    _ = connection_rx.recv() => {
        println!("All connections closed");
    }
    _ = tokio::time::sleep(Duration::from_secs(30)) => {
        println!("Shutdown timeout reached, forcing shutdown");
    }
}
```
The reference-counting built into the `mpsc` channel itself is the connection counter.

### Broadcast shutdown for auxiliary tasks

The `periodic_evictor` and `rate_limiter_cleanup` tasks are on timer loops. They do not watch the main channel for a `Shutdown` message; instead they subscribe to a `broadcast::channel::<()>(1)`:

```rust
let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

// Each auxiliary task receives a dedicated receiver:
spawn_periodic_evictor(cache_tx.clone(), shutdown_tx.subscribe());
spawn_rate_limiter(capacity, leak_rate, shutdown_tx.subscribe());
// Inside rate_limiter, cleanup task also calls shutdown_rx.resubscribe()
```

`broadcast` is the right channel type here because:
- A single `send(())` on `shutdown_tx` wakes **all** subscribers simultaneously.
- `mpsc` is many-to-one; `broadcast` is one-to-many. The shutdown signal is a one-to-many fan-out.

Each subscriber wraps its timer tick in a `tokio::select!`:

```rust
tokio::select! {
    _ = interval.tick()        => { /* do work */ }
    _ = shutdown_rx.recv()     => { break; }
}
```

This means auxiliary tasks exit cleanly on the next select poll after the signal is sent, without waiting for the next timer tick.

### Actor shutdown via direct message

The cache actor and rate limiter actor are driven by `mpsc::Receiver` loops. They cannot subscribe to a broadcast channel because they already have their own receiver. Instead, a typed `Shutdown` variant is sent down the existing channel:

```rust
let _ = cache_tx.send(CacheMessage::Shutdown).await;
let _ = rate_limit_tx.send(RateLimitMessage::Shutdown).await;
```

When the actor matches `Shutdown` it breaks out of the `while let Some(msg) = rx.recv()` loop, runs any cleanup code, then returns — allowing the `JoinHandle` to complete. All owned state (the LRU map, the bucket HashMap) is dropped here automatically by Rust.

### Complete shutdown timeline

```
t=0    SIGTERM / Ctrl+C received
t=0    stop accepting new TCP connections (listener dropped)
t=0    enter drain wait (up to 30s)

t=?    last connection task completes → connection_rx.recv() == None
       (or t=30 → forced drain timeout)

t=?+0  shutdown_tx.send(()) ──► periodic_evictor exits loop
                              ──► rate_limiter_cleanup exits loop
t=?+0  cache_tx.send(Shutdown)  ──► cache actor breaks loop, LruCache dropped
t=?+0  rate_limit_tx.send(Shutdown) ──► rate limiter actor breaks loop, HashMap dropped

t=?+5  (at most) tokio::join! completes or 5s timeout fires
t=?+5  main() returns → process exits cleanly
```

---

## Getting Started

### Prerequisites

- Rust 1.85+ (edition 2024)
- Cargo

### Build and run

```bash
git clone <repo>
cd reverse-proxy
cargo build --release
cargo run --release
```

The proxy listens on `127.0.0.1:8080` by default.

### Backend routing table

Edit `resolve_backend` in `src/main.rs` to add your upstream services:

```rust
fn resolve_backend(host: &str) -> &'static str {
    match host {
        "api.myapp.com"  => "http://127.0.0.1:9001",
        _                => "http://127.0.0.1:9000",
    }
}
```

### Configuration reference

| Parameter | Location | Default | Description |
|---|---|---|---|
| Listen address | `main.rs` | `127.0.0.1:8080` | Proxy bind address |
| Rate limit capacity | `spawn_rate_limiter(...)` | `30.0` | Max burst requests per IP |
| Rate limit leak rate | `spawn_rate_limiter(...)` | `5.0` | Sustained req/s per IP |
| Max cache size | `proxy_handler` | `10 MB` | Max single response cached |
| Cache capacity | `spawn_cache_actor` | `1000` entries | LRU eviction threshold |
| Cache TTL | `spawn_cache_actor` | `60 s` | Entry time-to-live |
| Eviction interval | `spawn_periodic_evictor` | `60 s` | How often expired entries are swept |
| Bucket idle TTL | `rate_limiter.rs` | `300 s` | When idle IP buckets are cleaned up |
| Connection pool idle | `main.rs` | `30 s` | Pool connection idle timeout |
| Pool max idle per host | `main.rs` | `32` | Max idle connections kept per backend |
| Keepalive | `main.rs` | `60 s` | TCP keepalive on backend connections |
| Drain timeout | `main.rs` | `30 s` | Max wait for active connections on shutdown |

### Running tests

```bash
cargo test
```

The LRU cache module has a comprehensive test suite covering: basic put/get, LRU eviction order, access-order promotion, TTL expiry, bulk `evict_expired`, TTL refresh on update, single-capacity edge case, and string key/value types.

---

## Future Work

### TLS Termination

Currently the proxy accepts and forwards plain HTTP. Adding TLS termination would allow the proxy to handle `HTTPS` from clients while maintaining plain HTTP connections to backends on the local network (a common deployment pattern).

The recommended approach is to integrate `tokio-rustls` and `rustls`. On each accepted TCP connection, before passing the stream to Hyper, wrap it in a `TlsAcceptor`:

```rust
let tls_acceptor = TlsAcceptor::from(Arc::new(server_config));
let tls_stream = tls_acceptor.accept(tcp_stream).await?;
let io = TokioIo::new(tls_stream);
// rest of connection handling unchanged
```

Certificate loading would use `rustls-pemfile` to parse PEM-encoded certificates and private keys. Hot-reloading certificates (without restart) can be implemented by watching the cert files with `notify` and sending the new `ServerConfig` through another actor-style channel. ALPN negotiation can be added to support HTTP/2 alongside HTTP/1.1.

### Load Balancing

Currently `resolve_backend` does static host-to-backend routing. True load balancing would distribute traffic across multiple upstream instances of the same service.

Planned strategies:

- **Round-robin**: maintain an `AtomicUsize` counter per service, increment modulo the number of backends.
- **Least-connections**: track in-flight request count per backend (a `Vec<AtomicUsize>`), always pick the minimum.
- **Consistent hashing**: route the same client IP to the same backend for session affinity.

Each strategy can be implemented as a separate type implementing a common `LoadBalancer` trait, selected at startup via config.

Health checking would run as another actor on a timer, sending HEAD requests to each backend and marking unreachable ones as unavailable, removing them from the rotation until they recover.

### Rate Limiter: From Actor Model to DashMap + Mutex

The current rate limiter is an actor. This works well and avoids all locking, but it introduces latency through the message-passing round-trip — every request must send a message and await a reply on a `oneshot` channel before it can proceed.

An alternative is a lock-based concurrent map:

```rust
use dashmap::DashMap;
use std::sync::Mutex;

// DashMap shards the map internally, reducing contention vs a single Mutex<HashMap>
let buckets: Arc<DashMap<IpAddr, Mutex<LeakyBucket>>> = Arc::new(DashMap::new());
```

`DashMap` provides lock-free reads and fine-grained shard locks for writes. Each `LeakyBucket` is wrapped in its own `Mutex` so that two concurrent requests from different IPs never contend at all — they access different shards and different per-IP mutexes.

The tradeoff compared to the actor model:

| | Actor model | DashMap + Mutex |
|---|---|---|
| Concurrency | serial (one msg at a time) | parallel across IPs |
| Latency | +channel overhead | near-zero overhead |
| Complexity | simple, no locking | requires care with lock ordering |
| Memory cleanup | handled in actor loop | requires background sweep |
| Testability | easy to mock channels | harder to inject |

For very high throughput (>100k req/s) the `DashMap` approach would likely outperform the actor due to eliminating the channel round-trip. For most workloads the actor model is simpler and safer.

### HTTP/2 Support

The proxy currently uses `hyper::server::conn::http1`. Adding HTTP/2 support would require upgrading to `hyper::server::conn::http2` and implementing ALPN negotiation in the TLS layer. HTTP/2 multiplexes many streams over a single TCP connection, which changes the connection guard pattern — the guard must track streams, not connections.

### Metrics and Observability

A `prometheus`-compatible metrics endpoint (`/metrics`) could be added as another actor that collects counters from the cache and rate limiter actors via dedicated `Metrics` message variants, then exposes them over HTTP.
