use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use htb::{BucketCfg, HTB};
use parking_lot::Mutex;
use reqwest::{Client, Request};
use tokio::task::JoinHandle;

#[derive(Debug)]
struct Stats {
    started_at: Instant,
    updated_at: Instant,
    num_requests: u64,
    total_requests: u64,
}

impl Stats {
    fn new() -> Stats {
        let now = Instant::now();
        Stats {
            started_at: now,
            updated_at: now,
            num_requests: 0,
            total_requests: 0,
        }
    }

    fn print_if_ready(&mut self) -> bool {
        // not great to use `Instant::now` every call, but it's whatever.
        // TODO: bryanhitc - should use Clock interface too.
        let now = Instant::now();
        if now - self.updated_at < Duration::from_secs(1) {
            return false;
        }

        println!("req/s: {}", self.num_requests);
        self.updated_at = now;
        self.num_requests = 0;
        true
    }

    fn register_req(&mut self) {
        self.num_requests += 1;
        self.total_requests += 1;
    }
}

impl Drop for Stats {
    fn drop(&mut self) {
        let now = Instant::now();
        let time_diff = now - self.started_at;
        println!(
            "\n{} total requests in {:.02?}.\nAverage req/s: {:.02}",
            self.total_requests,
            time_diff,
            self.total_requests as f64 / time_diff.as_secs_f64()
        );
    }
}

struct Throttler {
    // replace with non-htb?!
    token_bucket: Mutex<HTB<usize>>,
    // TODO: bryanhitc - inject clock instead of using `Instant` directly :)
    last_token_refresh: Mutex<Instant>,
}

impl Throttler {
    fn internal_take(&self) -> bool {
        let mut token_bucket = self.token_bucket.lock();
        if token_bucket.take(TOKEN_BUCKET_LABEL) {
            return true;
        }

        let time_diff;
        {
            let now = Instant::now();
            let mut last_refresh = self.last_token_refresh.lock();
            time_diff = now - *last_refresh;
            if time_diff < MIN_SLEEP_DURATION {
                return false;
            }
            *last_refresh = now;
        }

        // can advance outside of refresh lock to minimize contention. prob not necessary?
        token_bucket.advance(time_diff);
        token_bucket.take(TOKEN_BUCKET_LABEL)
    }

    pub async fn take(&self) {
        while !self.internal_take() {
            // save CPU... we are currently throttled. let token bucket refill a bit.
            tokio::time::sleep(MIN_SLEEP_DURATION).await;
        }
    }

    pub async fn take_with_timeout(&self, timeout: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(timeout) => false,
            _ = self.take() => true,
        }
    }
}

const NUM_WORKERS: u16 = 32;

const TOKEN_BUCKET_LABEL: usize = 0;
const MIN_SLEEP_DURATION: Duration = Duration::from_millis(1);
const BURST_DURATION: Duration = Duration::from_millis(50);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rate = (50000, Duration::from_secs(1));
    let bucket_config = BucketCfg {
        this: TOKEN_BUCKET_LABEL,
        parent: None,
        rate,
        capacity: calc_duration_capacity(rate, BURST_DURATION),
    };

    // won't achieve rate if we ever sleep without this property holding!
    assert!(MIN_SLEEP_DURATION < BURST_DURATION);

    let token_bucket = HTB::new(&[bucket_config])?;
    let throttler = Arc::new(Throttler {
        token_bucket: Mutex::new(token_bucket),
        last_token_refresh: Mutex::new(Instant::now()),
    });

    let client = Arc::new(Client::default());
    let request_base = Arc::new(
        client
            .get("http://localhost:4000/post/Why-Apple-Vision-Pro-Is-Reasonably-Priced")
            .build()?,
    );

    let stats = Arc::new(Mutex::new(Stats::new()));
    let mut workers = Vec::with_capacity(NUM_WORKERS.into());

    for _ in 0..NUM_WORKERS {
        let client = client.clone();
        let stats = stats.clone();
        let throttler = throttler.clone();
        let request_base = request_base.clone();
        workers.push(tokio::spawn(async move {
            worker(client, stats, throttler, request_base).await
        }));
    }

    tokio::select! {
        _ = run_workers(workers) => {
            println!("Workers finished.");
        },
        _ = tokio::signal::ctrl_c() => {
            println!("\nGracefully shutting down...");
        }
    }

    Ok(())
}

fn calc_duration_capacity(refill_rate: (u64, Duration), burst_duration: Duration) -> u64 {
    let refill_qps = refill_rate.0 as f64 * refill_rate.1.as_secs_f64();
    let capacity = refill_qps * burst_duration.as_secs_f64();
    f64::round(capacity) as u64
}

async fn run_workers(workers: Vec<JoinHandle<Result<(), anyhow::Error>>>) {
    for worker in workers {
        worker.await.unwrap().unwrap()
    }
}

async fn worker(
    client: Arc<Client>,
    stats: Arc<Mutex<Stats>>,
    throttler: Arc<Throttler>,
    request_base: Arc<Request>,
) -> anyhow::Result<()> {
    let timeout = Duration::from_millis(100);
    loop {
        while throttler.take_with_timeout(timeout).await {
            let request = request_base
                .try_clone()
                .ok_or_else(|| anyhow::format_err!("Unable to clone request"))?;
            client.execute(request).await?;
            {
                let mut stats = stats.lock();
                stats.register_req();
                stats.print_if_ready();
            }
        }

        panic!("worker timed out. should implement pseudo control theory auto ramp up/down (similar to TCP)");
    }
}
