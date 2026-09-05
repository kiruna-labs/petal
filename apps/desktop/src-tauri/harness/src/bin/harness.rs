#[cfg(all(feature = "live-io", target_os = "macos"))]
use std::path::PathBuf;

#[cfg(all(feature = "live-io", target_os = "macos"))]
use clap::Parser;

#[cfg(all(feature = "live-io", target_os = "macos"))]
use desktop_lib::transport::token::{fetch_access_token, BackendTokenRequest, TokenError};

#[cfg(all(feature = "live-io", target_os = "macos"))]
use petal_harness::bot::{
    BotError, PublisherBot, PublisherBotConfig, SubscriberBot, SubscriberBotConfig,
};
#[cfg(all(feature = "live-io", target_os = "macos"))]
use petal_harness::scorecard::{
    evaluate_absolute_thresholds, AbsoluteThresholds, ScenarioResult, Scorecard,
};

#[cfg(all(feature = "live-io", target_os = "macos"))]
#[derive(Debug, Parser)]
#[command(about = "Produce a live Petal SPEC §7 latency scorecard via LiveKit")]
struct Args {
    /// Petal room code/name to join. The backend/debug token path owns LiveKit slug derivation.
    #[arg(long)]
    room: String,

    /// Number of publishing bot participants.
    #[arg(long, default_value_t = 1)]
    publishers: u32,

    /// Synthetic shares/tracks per publishing bot.
    #[arg(long = "shares-per-bot", default_value_t = 1)]
    shares_per_bot: u32,

    /// Measurement duration after all bots are connected.
    #[arg(long = "duration-secs", default_value_t = 30)]
    duration_secs: u64,

    /// Scenario label for the scorecard.
    #[arg(long)]
    scenario: Option<String>,

    /// Impairment profile label. This runner records it but does not apply OS/network shaping.
    #[arg(long, default_value = "perfect")]
    impairment: String,

    /// Synthetic frame width.
    #[arg(long, default_value_t = 1280)]
    width: u32,

    /// Synthetic frame height.
    #[arg(long, default_value_t = 720)]
    height: u32,

    /// Synthetic publish frame rate.
    #[arg(long, default_value_t = 30.0)]
    fps: f64,

    /// Output scorecard JSON path.
    #[arg(long)]
    out: PathBuf,

    /// Absolute p95 ceiling used for this live run.
    #[arg(long = "max-p95-ms", default_value_t = 150.0)]
    max_p95_ms: f64,
}

#[cfg(all(feature = "live-io", target_os = "macos"))]
#[derive(Debug, thiserror::Error)]
enum RunnerError {
    #[error("token request failed: {0}")]
    Token(#[from] TokenError),
    #[error("bot failed: {0}")]
    Bot(#[from] BotError),
    #[error("scorecard serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to write scorecard: {0}")]
    Io(#[from] std::io::Error),
    #[error("live run received no timestamped frames; LiveKit publish/subscribe did not produce a measurable scorecard")]
    NoSamples,
    #[error("scorecard failed absolute p95 threshold")]
    Threshold,
}

#[cfg(all(feature = "live-io", target_os = "macos"))]
#[tokio::main]
async fn main() {
    env_logger::init();
    if let Err(error) = run().await {
        eprintln!("petal-harness failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(all(feature = "live-io", target_os = "macos"))]
async fn run() -> Result<(), RunnerError> {
    let args = Args::parse();
    let subscriber_identity = "petal-harness-sub";
    let subscriber_token = token_for(&args.room, subscriber_identity, false, true).await?;
    let url = subscriber_token.url.clone();
    let livekit_room = subscriber_token.room.clone();

    let subscriber = SubscriberBot::start(
        &url,
        &subscriber_token.token,
        SubscriberBotConfig {
            identity: subscriber_identity.to_string(),
            room_name: livekit_room.clone(),
        },
    )
    .await?;

    let mut publishers = Vec::with_capacity(args.publishers as usize);
    for index in 0..args.publishers {
        let identity = format!("petal-harness-pub-{index}");
        let token = token_for(&args.room, &identity, true, false).await?;
        let publisher = PublisherBot::start(
            &token.url,
            &token.token,
            PublisherBotConfig {
                identity,
                room_name: token.room,
                shares: args.shares_per_bot,
                width: args.width,
                height: args.height,
                fps: args.fps,
            },
        )
        .await?;
        publishers.push(publisher);
    }

    tokio::time::sleep(std::time::Duration::from_secs(args.duration_secs)).await;

    for publisher in &publishers {
        publisher.stop();
    }
    for publisher in publishers {
        publisher.join().await;
    }

    let (latency, freeze) = subscriber.snapshot();
    if latency.sample_count == 0 {
        return Err(RunnerError::NoSamples);
    }

    let delivered_fps = freeze.frames_received as f64 / args.duration_secs.max(1) as f64;
    let scenario_name = args.scenario.unwrap_or_else(|| {
        format!(
            "{}p-{}-{}share",
            args.publishers + 1,
            args.impairment,
            args.shares_per_bot
        )
    });
    let scorecard = Scorecard::new(
        now_ms(),
        vec![ScenarioResult {
            scenario_name,
            row_id: Some("A3".to_string()),
            source_issue: Some("#236".to_string()),
            coverage_kind: Some("synthetic-media".to_string()),
            participant_count: args.publishers + 1,
            shares_per_bot: args.shares_per_bot,
            impairment_profile: args.impairment,
            latency,
            freeze,
            delivered_fps,
            delivered_width: args.width,
            delivered_height: args.height,
            reconnect_ms: None,
        }],
    );

    std::fs::write(&args.out, scorecard.to_json_pretty()?)?;
    let absolute = evaluate_absolute_thresholds(
        &scorecard,
        AbsoluteThresholds {
            max_p95_latency_ms: args.max_p95_ms,
        },
    );
    if !absolute.passed {
        for violation in absolute.violations {
            eprintln!(
                "{} {}: {:.2}ms > {:.2}ms",
                violation.scenario_name, violation.metric, violation.current, violation.threshold
            );
        }
        return Err(RunnerError::Threshold);
    }

    println!("wrote scorecard {}", args.out.display());
    Ok(())
}

#[cfg(all(feature = "live-io", target_os = "macos"))]
async fn token_for(
    room: &str,
    identity: &str,
    can_publish: bool,
    can_subscribe: bool,
) -> Result<desktop_lib::transport::token::BackendTokenResponse, TokenError> {
    fetch_access_token(BackendTokenRequest {
        room,
        identity,
        display_name: Some(identity),
        can_publish,
        can_subscribe,
        can_publish_data: false,
        hidden: true,
    })
    .await
}

#[cfg(all(feature = "live-io", target_os = "macos"))]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(not(all(feature = "live-io", target_os = "macos")))]
fn main() {
    eprintln!(
        "petal-harness live runner requires macOS and `--features live-io`. \
         Use `petal-scorecard-gate` for the CI-safe scorecard gate."
    );
    std::process::exit(2);
}
