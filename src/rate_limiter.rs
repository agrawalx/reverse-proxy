use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

// Buckets unused for longer than this are dropped by the cleanup task
const BUCKET_IDLE_TTL: Duration = Duration::from_secs(300); // 5 minutes
const CLEANUP_INTERVAL: Duration = Duration::from_secs(300);

struct LeakyBucket {
    water: f64,
    capacity: f64,
    leak_rate: f64,
    last_check: Instant,
    last_used: Instant, // tracks idleness for cleanup
}

impl LeakyBucket {
    fn new(capacity: f64, leak_rate: f64) -> Self {
        let now = Instant::now();
        Self {
            water: 0.0,
            capacity,
            leak_rate,
            last_check: now,
            last_used: now,
        }
    }

    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_check).as_secs_f64();
        self.last_check = now;
        self.last_used = now;

        self.water = (self.water - elapsed * self.leak_rate).max(0.0);
        if self.water + 1.0 <= self.capacity {
            self.water += 1.0;
            true
        } else {
            false
        }
    }

    fn is_idle(&self) -> bool {
        self.last_used.elapsed() >= BUCKET_IDLE_TTL
    }
}

pub enum RateLimitMessage {
    Allow {
        ip: IpAddr,
        reply_to: oneshot::Sender<bool>,
    },
    CleanExpired,
    Shutdown,
}

pub fn spawn_rate_limiter(
    capacity: f64,
    leak_rate: f64,
    shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> (mpsc::Sender<RateLimitMessage>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<RateLimitMessage>(256);

    // Periodic cleanup task
    let tx_cleanup = tx.clone();
    // basically clones the reciever end using a reciever 
    let mut shutdown_rx_cleanup = shutdown_rx.resubscribe();
    let cleanup_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if tx_cleanup.send(RateLimitMessage::CleanExpired).await.is_err() {
                        break;
                    }
                }
                _ = shutdown_rx_cleanup.recv() => {
                    println!("Rate limiter cleanup task shutting down");
                    break;
                }
            }
        }
    });

    let main_handle = tokio::spawn(async move {
        let mut buckets: HashMap<IpAddr, LeakyBucket> = HashMap::new();
        while let Some(msg) = rx.recv().await {
            match msg {
                RateLimitMessage::Allow { ip, reply_to } => {
                    let bucket = buckets
                        .entry(ip)
                        .or_insert_with(|| LeakyBucket::new(capacity, leak_rate));
                    let _ = reply_to.send(bucket.allow());
                }
                RateLimitMessage::CleanExpired => {
                    let before = buckets.len();
                    buckets.retain(|_, bucket| !bucket.is_idle());
                    let removed = before - buckets.len();
                    if removed > 0 {
                        println!("Rate limiter: removed {} idle buckets", removed);
                    }
                }
                RateLimitMessage::Shutdown => {
                    println!("Rate limiter actor shutting down gracefully");
                    break;
                }
            }
        }
        println!("Rate limiter actor terminated");
        drop(cleanup_handle);
    });

    (tx, main_handle)
}