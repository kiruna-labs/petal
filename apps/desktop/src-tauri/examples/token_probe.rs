//! #654 spike, questions 1 and 4: prove the Gemini Live path from Rust using an
//! **ephemeral token** (never the raw API key on the wire to the WS), confirm
//! **manual-activity (push-to-talk) mode**, and observe whether the token's
//! `expireTime` actually terminates a live session (the only server-side cost
//! bound the hosted design in #655 leans on).
//!
//! This is a self-contained probe: it does BOTH halves the real product splits
//! across the backend and the client — it mints the token itself (the job of
//! `/api/ai-token`, #655) and then connects the WS (the job of the client,
//! #656). The raw `GEMINI_API_KEY` is used ONLY for the mint HTTP call and is
//! never placed on the WS URL; the WS authenticates with the minted token.
//!
//! Usage:
//! ```text
//! export GEMINI_API_KEY=...        # or put it in apps/desktop/.env
//! cargo run --example token_probe -- --seconds 20
//! cargo run --example token_probe -- --expiry-secs 60 --seconds 120   # Q4
//! ```
//! Flags:
//!   --model <id>        Live model (default models/gemini-3.1-flash-live-preview)
//!   --expiry-secs N     token expireTime = now + N (default 720 = 12 min)
//!   --seconds N         how long to hold the WS open after setup (default 25)
//!   --new-session-secs N  newSessionExpireTime = now + N (default 60)
//!
//! For Q4, set --expiry-secs SMALL (e.g. 60) and --seconds LARGER (e.g. 120):
//! the probe keeps the socket open past expiry and logs if/when/how the server
//! closes it (close code + reason). If the socket survives well past
//! expireTime, that is the "no server-side cost bound" finding #655 must act on.
//!
//! The Gemini key is read from the environment / `.env` and is NEVER logged. The
//! WS connect URL (which carries the token) is NEVER logged either — only a
//! redacted form.

use chrono::{Duration, Utc};
use desktop_lib::ai_chat::protocol::{self, ServerEvent};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const AUTH_TOKENS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/auth_tokens";
// Ephemeral tokens authenticate the CONSTRAINED bidi method, not the plain one.
// Connecting to `BidiGenerateContent` with a token yields "Method doesn't allow
// unregistered callers"; the token is minted for `BidiGenerateContentConstrained`
// (#654 finding — the raw API key uses the unconstrained method, tokens use this).
const WS_BASE: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContentConstrained";

struct Args {
    model: String,
    expiry_secs: i64,
    new_session_secs: i64,
    seconds: u64,
    lock_model: bool,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let get = |name: &str| -> Option<String> {
        raw.iter()
            .position(|a| a == name)
            .and_then(|i| raw.get(i + 1))
            .cloned()
    };
    Args {
        model: get("--model").unwrap_or_else(|| protocol::DEFAULT_MODEL_ID.to_string()),
        expiry_secs: get("--expiry-secs")
            .and_then(|s| s.parse().ok())
            .unwrap_or(720),
        new_session_secs: get("--new-session-secs")
            .and_then(|s| s.parse().ok())
            .unwrap_or(60),
        seconds: get("--seconds").and_then(|s| s.parse().ok()).unwrap_or(25),
        lock_model: raw.iter().any(|a| a == "--lock-model"),
    }
}

/// Redact a URL that carries a credential in its query string, for logging.
fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{base}?<redacted>"),
        None => url.to_string(),
    }
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::from_path("../.env");
    let _ = dotenvy::dotenv();
    let args = parse_args();

    let api_key = match std::env::var("GEMINI_API_KEY") {
        Ok(k) if !k.trim().is_empty() => k,
        _ => {
            eprintln!(
                "token_probe: set GEMINI_API_KEY (env or apps/desktop/.env). It is used only to mint the ephemeral token and is never sent to the WS or logged."
            );
            std::process::exit(2);
        }
    };

    // ---- Step 1: mint an ephemeral token (the /api/ai-token job, #655) ------
    let now = Utc::now();
    let expire = (now + Duration::seconds(args.expiry_secs)).to_rfc3339();
    let new_session = (now + Duration::seconds(args.new_session_secs)).to_rfc3339();
    let mut mint_body = serde_json::json!({
        "uses": 1,
        "expireTime": expire,
        "newSessionExpireTime": new_session,
    });
    // Constraint-locking (model + modality) is opt-in: the LIVE v1beta REST API
    // currently rejects `liveConnectConstraints` as an unknown field even though
    // the public docs show it (doc/API-version skew, #654 finding). The real
    // backend (#655) will mint via the official google-genai SDK, which applies
    // the transform. For the spike, an unconstrained token fully answers Q1/Q4.
    if args.lock_model {
        mint_body["liveConnectConstraints"] = serde_json::json!({
            "model": args.model,
            "config": { "responseModalities": ["AUDIO"] }
        });
    }

    println!(
        "token_probe: minting ephemeral token (model={}, expireTime=+{}s, newSessionExpireTime=+{}s)…",
        args.model, args.expiry_secs, args.new_session_secs
    );
    let http = reqwest::Client::new();
    let mint_resp = http
        .post(AUTH_TOKENS_URL)
        .header("x-goog-api-key", &api_key)
        .json(&mint_body)
        .send()
        .await;
    let mint_resp = match mint_resp {
        Ok(r) => r,
        Err(e) => {
            eprintln!("token_probe: mint request failed (network): {e}");
            std::process::exit(3);
        }
    };
    let status = mint_resp.status();
    let text = mint_resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // The error body may echo request fields but not the key; still, don't
        // dump it wholesale — print status + a bounded prefix.
        eprintln!(
            "token_probe: auth_tokens returned {} — first 300 chars: {}",
            status,
            text.chars().take(300).collect::<String>()
        );
        eprintln!("  (401/403 → key/permission; 400 → constraint/timestamp shape; check the model id exists.)");
        std::process::exit(4);
    }
    // Inspect the response SHAPE (key names + string lengths, never the value)
    // so we can tell whether the usable token is `name` or a separate field.
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&text) {
        let shape: Vec<String> = map
            .iter()
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => format!("{k}: <string len {}>", s.len()),
                other => format!("{k}: {other}"),
            })
            .collect();
        println!("token_probe: mint response keys: [{}]", shape.join(", "));
    }
    let token_name = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => v
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string()),
        Err(_) => None,
    };
    let token = match token_name {
        Some(t) if !t.is_empty() => t,
        _ => {
            eprintln!("token_probe: mint succeeded but no token `name` in response");
            std::process::exit(5);
        }
    };
    println!("token_probe: minted token (name length {} chars) — value not logged", token.len());

    // ---- Step 2: connect the WS with the TOKEN (the client job, #656) -------
    // The minted token (`authTokens/…`, passed verbatim) authenticates via the
    // `access_token` query param against the CONSTRAINED endpoint above. The raw
    // API key never goes here — only the short-lived token.
    let ws_url = format!("{WS_BASE}?access_token={token}");
    println!("token_probe: connecting {} …", redact_url(&ws_url));
    let connect = tokio_tungstenite::connect_async(&ws_url).await;
    let (mut ws, _resp) = match connect {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("token_probe: WS connect failed: {e}");
            eprintln!("  (if this is a TLS/CryptoProvider error, the production path will need rustls setup; this probe uses native-tls to sidestep it.)");
            std::process::exit(6);
        }
    };
    println!("token_probe: WS connected. Sending setup (manual-activity / PTT mode)…");

    // setup: AUDIO responses, transcription, automaticActivityDetection disabled.
    ws.send(Message::Text(protocol::setup_message(&args.model).into()))
        .await
        .expect("send setup");

    // Drive the session: await setupComplete, then do one manual-activity turn
    // with a short synthetic tone, then hold the socket open to observe expiry.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(args.seconds);
    let mut setup_complete = false;
    let mut sent_greeting = false;
    let mut greeting_turn_done = false;
    let mut sent_activity = false;
    let mut got_audio = false;
    let mut got_output_text = false;
    let mut go_away = false;
    // Auth is proven the moment the server processes our request past the
    // credential check — i.e. we get a serverside close about billing/quota/
    // policy rather than about the token itself.
    let mut close_reason: Option<String> = None;

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            println!("token_probe: reached --seconds window; closing.");
            break;
        }
        let next = tokio::time::timeout(remaining.min(std::time::Duration::from_secs(2)), ws.next());
        match next.await {
            Err(_) => {
                // timeout tick — after setup, send the greeting turn (mirrors
                // session.rs's real SetupComplete handling, added after this
                // probe reproduced the model volunteering an ungrounded
                // "I see you're sharing a Chrome window" with zero frames or
                // digest ever sent), then push a PTT turn once.
                if setup_complete && !sent_greeting {
                    println!("token_probe: setupComplete OK → sending scripted greeting turn (mirrors session.rs)…");
                    let _ = ws
                        .send(Message::Text(protocol::greeting_trigger_message().into()))
                        .await;
                    sent_greeting = true;
                }
                // Wait for the greeting's own turn to actually finish before
                // starting a PTT turn -- sending activityStart while the
                // greeting is still mid-generation would overlap two turns.
                if setup_complete && greeting_turn_done && !sent_activity {
                    println!("token_probe: sending activityStart + tone + activityEnd (manual PTT)…");
                    send_ptt_tone(&mut ws).await;
                    sent_activity = true;
                }
                continue;
            }
            Ok(None) => {
                println!("token_probe: WS stream ended (server closed, no close frame).");
                break;
            }
            Ok(Some(Err(e))) => {
                // A close frame surfaces here; tungstenite carries the code.
                println!("token_probe: WS error/close: {e}");
                break;
            }
            Ok(Some(Ok(msg))) => {
                let raw = match &msg {
                    // Gemini sends JSON as BINARY frames — decode both.
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(b).to_string(),
                    Message::Close(frame) => {
                        close_reason = frame.as_ref().map(|f| f.reason.to_string());
                        println!("token_probe: received Close frame: {frame:?}");
                        break;
                    }
                    _ => continue,
                };
                for ev in protocol::parse_server_message(&raw) {
                    match ev {
                        ServerEvent::SetupComplete => {
                            setup_complete = true;
                            println!("token_probe: ← setupComplete");
                        }
                        ServerEvent::Audio(bytes) => {
                            if !got_audio {
                                println!("token_probe: ← first audio chunk ({} bytes) — model is replying", bytes.len());
                            }
                            got_audio = true;
                        }
                        ServerEvent::OutputText(t) => {
                            got_output_text = true;
                            println!("token_probe: ← outputTranscription: {t:?}");
                        }
                        ServerEvent::InputText(t) => {
                            println!("token_probe: ← inputTranscription: {t:?}");
                        }
                        ServerEvent::TurnComplete => {
                            println!("token_probe: ← turnComplete");
                            if sent_greeting && !greeting_turn_done {
                                greeting_turn_done = true;
                            }
                        }
                        ServerEvent::Interrupted => println!("token_probe: ← interrupted"),
                        ServerEvent::GoAway => {
                            go_away = true;
                            println!("token_probe: ← goAway (server ending connection — Q4 lifetime signal)");
                        }
                        ServerEvent::ToolCallBatch(calls) => {
                            // This probe never declares tools, so a tool call
                            // here would mean the setup message is not the one
                            // we think we sent — worth reporting, not ignoring.
                            println!(
                                "token_probe: ← UNEXPECTED tool-call batch ({} call(s); this probe declares no tools)",
                                calls.len()
                            );
                        }
                        ServerEvent::Other => {}
                    }
                }
            }
        }
    }

    // Classify the outcome. A close whose reason talks about auth/token/key ⇒
    // auth FAILED; a close about billing/credits/quota/policy AFTER a successful
    // connect ⇒ auth PASSED and something downstream stopped the session.
    let reason_l = close_reason.as_deref().unwrap_or("").to_lowercase();
    let auth_failed = reason_l.contains("api key")
        || reason_l.contains("unregistered")
        || reason_l.contains("token")
        || reason_l.contains("credential")
        || reason_l.contains("permission");
    let billing_blocked = reason_l.contains("credit")
        || reason_l.contains("billing")
        || reason_l.contains("quota")
        || reason_l.contains("prepayment");
    let auth_ok = setup_complete || (!close_reason.is_none() && !auth_failed);

    println!("\n==== token_probe summary (#654 Q1/Q4) ====");
    println!(
        "  token-only WS auth:   {}   (Q1: connect BidiGenerateContentConstrained?access_token=<token>)",
        if auth_ok { "PASSED" } else { "FAILED/unknown" }
    );
    if billing_blocked {
        println!(
            "  NOTE: session stopped at the BILLING gate (credits/quota), not auth — add Gemini billing to complete the live round-trip + Q4."
        );
    }
    println!("  setupComplete:        {setup_complete}");
    println!("  manual-activity sent: {sent_activity}   (activityStart/End accepted, no 1007 close)");
    println!("  model audio reply:    {got_audio}");
    println!("  output transcription: {got_output_text}");
    println!("  goAway observed:      {go_away}");
    println!(
        "  Q4: elapsed vs expireTime — token expired at +{}s; session ran up to {}s. If the socket stayed open well past expiry with no close, `expireTime` does NOT bound a live session and #655's cost model must change.",
        args.expiry_secs, args.seconds
    );
}

/// Send one push-to-talk turn: activityStart → a short 16 kHz tone as PCM16
/// audio → activityEnd. A synthetic tone (not real speech) is enough to confirm
/// the server ACCEPTS a manual-activity turn without a 1007 close; a coherent
/// spoken reply is not the point of this probe.
async fn send_ptt_tone<S>(ws: &mut S)
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    if let Err(e) = ws
        .send(Message::Text(protocol::activity_start_message().into()))
        .await
    {
        eprintln!("token_probe: activityStart send failed: {e}");
        return;
    }
    // ~500ms of a 440 Hz tone at 16 kHz mono, chunked into 20ms frames.
    let sample_rate = 16_000usize;
    let total = sample_rate / 2; // 0.5s
    let mut phase = 0f32;
    let step = 2.0 * std::f32::consts::PI * 440.0 / sample_rate as f32;
    let frame = sample_rate / 50; // 20ms = 320 samples
    let mut buf: Vec<f32> = Vec::with_capacity(frame);
    for i in 0..total {
        buf.push(phase.sin() * 0.25);
        phase += step;
        if buf.len() == frame || i == total - 1 {
            let pcm = protocol::f32_to_pcm16(&buf);
            if let Err(e) = ws
                .send(Message::Text(protocol::audio_chunk_message(&pcm).into()))
                .await
            {
                eprintln!("token_probe: audio chunk send failed: {e}");
                return;
            }
            buf.clear();
        }
    }
    if let Err(e) = ws
        .send(Message::Text(protocol::activity_end_message().into()))
        .await
    {
        eprintln!("token_probe: activityEnd send failed: {e}");
    }
}
