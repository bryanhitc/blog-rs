use std::{
    fmt::Write,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::{arg, command, Parser};
use htb::{BucketCfg, HTB};
use parking_lot::Mutex;
use quantiles::ckms::CKMS;
use reqwest::{Client, Request};
use tokio::task::JoinHandle;

trait MetricsRecorder {
    fn new() -> Self;
    fn record(&mut self, latency: Duration);
    fn write_to_output(&self, output: &mut String) -> anyhow::Result<()>;
}

#[derive(Debug, Default)]
struct NoopMetricsRecorder {}
impl MetricsRecorder for NoopMetricsRecorder {
    fn new() -> Self {
        NoopMetricsRecorder {}
    }

    fn record(&mut self, _latency: Duration) {}

    fn write_to_output(&self, _output: &mut String) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct ExpensiveMetricsRecorder {
    total_latency_ckms: CKMS<f64>,
}

impl ExpensiveMetricsRecorder {
    fn write_quantile_str(
        &self,
        name: &str,
        quantile: f64,
        output: &mut String,
    ) -> anyhow::Result<()> {
        let quantile_latency = Duration::from_secs_f64(
            self.total_latency_ckms
                .query(quantile)
                .ok_or_else(|| anyhow::format_err!("Unable to query quantile {}", quantile))?
                .1,
        );
        output.write_fmt(format_args!("{name}: {:?}\n", quantile_latency))?;
        Ok(())
    }
}

impl MetricsRecorder for ExpensiveMetricsRecorder {
    fn new() -> Self {
        Self {
            total_latency_ckms: CKMS::<f64>::new(0.0001),
        }
    }

    fn record(&mut self, latency: Duration) {
        self.total_latency_ckms.insert(latency.as_secs_f64());
    }

    fn write_to_output(&self, output: &mut String) -> anyhow::Result<()> {
        output.write_str("Latency summary:\n")?;
        self.write_quantile_str("p0", 0.0, output)?;
        self.write_quantile_str("p10", 0.1, output)?;
        self.write_quantile_str("p50", 0.5, output)?;
        self.write_quantile_str("p90", 0.9, output)?;
        self.write_quantile_str("p95", 0.95, output)?;
        self.write_quantile_str("p99", 0.99, output)?;
        self.write_quantile_str("p99.9", 0.999, output)?;
        self.write_quantile_str("p99.99", 0.9999, output)?;
        self.write_quantile_str("p100", 1.0, output)
    }
}

#[derive(Debug)]
struct Stats<T: MetricsRecorder + Send> {
    started_at: Instant,
    updated_at: Instant,
    num_requests: u64,
    total_requests: u64,
    metrics_recorder: T,
}

impl<T: MetricsRecorder + Send> Stats<T> {
    fn new() -> Self {
        let now = Instant::now();
        Stats {
            started_at: now,
            updated_at: now,
            num_requests: 0,
            total_requests: 0,
            // support up to p99.99
            metrics_recorder: T::new(),
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

    fn register_req(&mut self, latency: Duration) {
        self.num_requests += 1;
        self.total_requests += 1;
        self.metrics_recorder.record(latency);
    }
}

impl<T: MetricsRecorder + Send> Drop for Stats<T> {
    fn drop(&mut self) {
        let elapsed = self.started_at.elapsed();
        let mut output = String::new();
        output
            .write_fmt(format_args!(
                "\n{} total requests in {:.02?}",
                self.total_requests, elapsed
            ))
            .unwrap();
        output
            .write_fmt(format_args!(
                "\nAverage req/s: {:.02?}",
                self.total_requests as f64 / elapsed.as_secs_f64()
            ))
            .unwrap();
        output.write_str("\n\n").unwrap();
        self.metrics_recorder.write_to_output(&mut output).unwrap();
        println!("{}", output);
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

const TOKEN_BUCKET_LABEL: usize = 0;
const MIN_SLEEP_DURATION: Duration = Duration::from_millis(1);

fn parse_duration(arg: &str) -> Result<Duration, std::num::ParseIntError> {
    let millis = arg.parse()?;
    Ok(std::time::Duration::from_millis(millis))
}

/// Stress test `blog-rs`
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    // Port the local server is running on.
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Burst duration in milliseconds.
    #[arg(long, value_parser = parse_duration, default_value = "10")]
    burst_duration: Duration,

    /// Number of async workers to spawn
    #[arg(short, long, default_value_t = 32)]
    num_workers: u16,

    /// Target queries per second enforced by throttler
    #[arg(long, default_value_t = 10000)]
    qps: u64,

    // Record metrics (e.g., percentile latency)
    #[arg(long, default_value_t = false)]
    record_metrics: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let port = args.port;
    let rate = (args.qps, Duration::from_secs(1));
    let bucket_config = BucketCfg {
        this: TOKEN_BUCKET_LABEL,
        parent: None,
        rate,
        capacity: calc_duration_capacity(rate, args.burst_duration),
    };

    // won't achieve rate if we ever sleep without this property holding!
    assert!(MIN_SLEEP_DURATION < args.burst_duration);

    let token_bucket = HTB::new(&[bucket_config])?;
    let throttler = Arc::new(Throttler {
        token_bucket: Mutex::new(token_bucket),
        last_token_refresh: Mutex::new(Instant::now()),
    });

    let client = Client::default();
    let request_base = Arc::new(
        client
            .get(format!(
                "http://localhost:{port}/post/Why-Apple-Vision-Pro-Is-Reasonably-Priced"
            ))
            .build()?,
    );

    // this would be so much cleaner with https://github.com/rust-lang/rust/issues/92827... sad.
    // can't even create a closure to do this since closures don't support `impl Trait`.
    // this is the price we pay to avoid a branch (that is essentially guaranteed to be predicted)...
    let workers = {
        if args.record_metrics {
            let stats = Arc::new(Mutex::new(Stats::<ExpensiveMetricsRecorder>::new()));
            create_workers(args.num_workers, client, stats, throttler, request_base)
        } else {
            let stats = Arc::new(Mutex::new(Stats::<NoopMetricsRecorder>::new()));
            create_workers(args.num_workers, client, stats, throttler, request_base)
        }
    };

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

fn create_workers(
    num_workers: u16,
    client: Client,
    stats: Arc<Mutex<Stats<impl MetricsRecorder + Send + 'static>>>,
    throttler: Arc<Throttler>,
    request_base: Arc<Request>,
) -> Vec<JoinHandle<anyhow::Result<()>>> {
    let mut workers = Vec::with_capacity(num_workers.into());
    for _ in 0..num_workers {
        let client = client.clone();
        let stats = stats.clone();
        let throttler = throttler.clone();
        let request_base = request_base.clone();
        workers.push(tokio::spawn(async move {
            worker(client, stats, throttler, request_base).await
        }));
    }
    workers
}

async fn run_workers(workers: Vec<JoinHandle<Result<(), anyhow::Error>>>) {
    for worker in workers {
        worker.await.unwrap().unwrap()
    }
}

async fn worker(
    client: Client,
    stats: Arc<Mutex<Stats<impl MetricsRecorder + Send>>>,
    throttler: Arc<Throttler>,
    request_base: Arc<Request>,
) -> anyhow::Result<()> {
    let timeout = Duration::from_secs(1);
    loop {
        while throttler.take_with_timeout(timeout).await {
            let request = request_base
                .try_clone()
                .ok_or_else(|| anyhow::format_err!("Unable to clone request"))?;
            let start_time = Instant::now();
            client.execute(request).await?;
            let latency = start_time.elapsed();
            {
                let mut stats = stats.lock();
                stats.register_req(latency);
                stats.print_if_ready();
            }
        }

        panic!("worker timed out. should implement pseudo control theory auto ramp up/down (similar to TCP)");
    }
}
