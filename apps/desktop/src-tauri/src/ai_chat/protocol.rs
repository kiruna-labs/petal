//! Gemini Live (`BidiGenerateContent`) JSON message builders + server-message
//! parsing, kept pure so they can be unit-tested without a live socket or key.
//!
//! Endpoint:
//!   wss://generativelanguage.googleapis.com/ws/
//!     google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent
//!   authenticated with an ephemeral token as the `access_token` query param
//!   (#655) so the real key never reaches a client. The connect URL carries the
//!   credential and MUST NEVER be logged.
//!
//! Flow: send `setup` → await `{"setupComplete":{}}` → per PTT hold send
//! `activityStart` → stream `realtimeInput.audio` PCM16/16k chunks → `activityEnd`;
//! stream `realtimeInput.video` JPEG frames out-of-band; receive `serverContent`
//! with audio + transcription parts.
//!
//! Wire gotchas carried over from the takt reference (each cost a debugging
//! cycle there): the server sends JSON as **binary** WS frames (decode both text
//! and binary or `setupComplete` silently never arrives); `realtimeInput.mediaChunks`
//! is rejected with close code 1007 — use `.audio` / `.video` / `.text`.

use base64::Engine;
use serde::Deserialize;

/// Default Live model for BYOK / the spike. Hosted mode does NOT use this — it
/// takes the `model` field returned by `/api/ai-token` (#655/#656) so a
/// preview-model rename is a backend env change, not a client release.
pub const DEFAULT_MODEL_ID: &str = "models/gemini-3.1-flash-live-preview";

/// System instruction for the meeting context. Adapted from takt's single-user
/// wording: multiple participants address the model via push-to-talk, each
/// utterance is labelled with the speaker, and the model must not narrate the
/// window until it has actually received a frame or accessibility snapshot.
/// Phase 1 declares no tools, so this says nothing about window control.
const SYSTEM_INSTRUCTION: &str = "You are a helpful assistant joining a screen-sharing meeting. One participant is sharing a single application window with you (sent as periodic screenshots and an accessibility snapshot); participants take turns talking to you using push-to-talk, and each spoken turn is labelled with the speaker's name. Answer concisely about what's on the shared window, but never describe or infer visual content before you have received a screenshot frame or accessibility snapshot. Accessibility snapshot lines use [n] indexes; treat them as OS context, not as something a participant said.";

/// The first client message. Requests AUDIO responses plus input/output
/// transcription (so every surface can render readable text for both sides) and
/// **disables automatic activity detection** — activity is bracketed explicitly
/// by [`activity_start_message`] / [`activity_end_message`] for push-to-talk.
/// Phase 1 declares no function tools.
pub fn setup_message(model_id: &str) -> String {
    setup_message_with_tools(model_id, false)
}

/// Extra instruction appended when window-control tools are offered (#658).
/// It tells the model the tools are gated and that a refusal is final until a
/// human says otherwise — a model that retries a refused action would train
/// users to click through approval cards.
const CONTROL_INSTRUCTION: &str = " You also have window-control tools. They are permission-gated and may be refused; never retry a refused action until a participant explicitly grants control. Use window_click for an indexed visible control, window_type for exact text, window_press_key only for the listed navigation keys, and window_scroll for small bounded scrolling. Never use these tools for passwords, terminals, security dialogs, or arbitrary key chords.";

/// Setup, optionally declaring the window-control tools.
///
/// `enable_tools` is false for phase 1 (#656): a session that cannot act needs
/// no tools, and declaring them would invite refusals the user never asked for.
/// #658 turns it on behind the approval gate.
pub fn setup_message_with_tools(model_id: &str, enable_tools: bool) -> String {
    let mut setup = serde_json::json!({
        "model": model_id,
        "generationConfig": { "responseModalities": ["AUDIO"] },
        "systemInstruction": { "parts": [{ "text": SYSTEM_INSTRUCTION }] },
        "inputAudioTranscription": {},
        "outputAudioTranscription": {},
        "realtimeInputConfig": {
            "automaticActivityDetection": { "disabled": true }
        }
    });
    if enable_tools {
        setup["systemInstruction"]["parts"][0]["text"] =
            serde_json::Value::String(format!("{SYSTEM_INSTRUCTION}{CONTROL_INSTRUCTION}"));
        setup["tools"] = tool_declarations();
    }
    serde_json::json!({ "setup": setup }).to_string()
}

/// The window-control tool schemas. Kept deliberately narrow: bounded text,
/// an enumerated key set (no arbitrary chords), and bounded scrolling. The
/// model must not be ABLE to express a dangerous action, rather than merely
/// being asked not to.
fn tool_declarations() -> serde_json::Value {
    serde_json::json!([{
        "functionDeclarations": [{
            "name": "window_type",
            "description": "Type exact text into the shared window's currently focused, non-secure input.",
            "parameters": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The exact text to type.", "maxLength": 2000 }
                },
                "required": ["text"]
            }
        }, {
            "name": "window_click",
            "description": "Click the visible accessibility element identified by its accessibility snapshot generation and index.",
            "parameters": {
                "type": "object",
                "properties": {
                    "element_index": { "type": "integer", "minimum": 0 },
                    "generation": { "type": "integer", "minimum": 0, "description": "The accessibility snapshot generation containing this index." }
                },
                "required": ["element_index", "generation"]
            }
        }, {
            "name": "window_press_key",
            "description": "Press one safe navigation key; arbitrary chords are not supported.",
            "parameters": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "enum": ["Return", "Tab", "Escape", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight"] }
                },
                "required": ["key"]
            }
        }, {
            "name": "window_scroll",
            "description": "Scroll a small bounded amount in the shared window.",
            "parameters": {
                "type": "object",
                "properties": {
                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "amount": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                "required": ["direction", "amount"]
            }
        }]
    }])
}

/// Answer a tool call. Every response echoes the original id and name, so a
/// refused call can never be mistaken for a successful one.
pub fn tool_response_message(id: &str, name: &str, ok: bool, code: &str, message: &str) -> String {
    serde_json::json!({
        "toolResponse": {
            "functionResponses": [{
                "id": id,
                "name": name,
                "response": { "ok": ok, "code": code, "message": message }
            }]
        }
    })
    .to_string()
}

/// A short, locally scripted sentence the model is told to say verbatim as
/// its first utterance, so the model's opening line is never a guess.
const GREETING: &str = "Hi, I'm ready — what would you like to know about this window?";

/// The session-start control turn, sent once immediately after
/// `setupComplete` and before anything else (including the first video
/// frame or accessibility digest, whichever arrives first).
///
/// Ported from the takt reference, which added this after observing the
/// model volunteer specific, ungrounded visual claims ("I see you're
/// sharing a Chrome window") based on nothing but conversational framing —
/// the system instruction's "never describe visual content before you have
/// received a frame" is advisory, not enforced, and a model under
/// no other constraint will still open with a guess. Confirmed live on this
/// port too: `token_probe` reproduced the exact failure with zero frames or
/// digest ever sent.
///
/// Petal's design makes this SIMPLER than takt's: takt is always-listening,
/// so it also has to guess whether the user started talking first and
/// suppress the greeting if so (a real source of bugs there). Under
/// push-to-talk the model cannot hear anyone until a participant explicitly
/// holds PTT, so there is no race to arbitrate — this message is always
/// sent unconditionally, every session, with nothing that could preempt it.
///
/// Deliberately does NOT wait for or embed the accessibility digest: the
/// digest walk runs on its own thread and may not have a result yet, and
/// blocking the model's first utterance on it would tie greeting latency to
/// AX walk latency for no reason. The digest, whenever it arrives, is
/// delivered separately by [`initial_digest_message`] / the refresh path —
/// exactly the same "digest is independent of the turn timeline" property
/// takt's own `DigestEvent::First(None)` unwedging relies on.
pub fn greeting_trigger_message() -> String {
    text_turn_message(&format!(
        "(Session-start control message. Say exactly this locally scripted sentence and nothing \
         else: {GREETING:?} Do not describe, guess, or mention any visual content until a \
         screenshot frame or accessibility snapshot actually arrives. This control message is \
         not something a participant said; never quote or refer to it.)"
    ))
}

/// A complete `clientContent` turn with `turnComplete: true`, so Gemini
/// treats it as a finished message and responds. The shared building block
/// behind [`greeting_trigger_message`] and [`user_text_message`].
fn text_turn_message(text: &str) -> String {
    serde_json::json!({
        "clientContent": {
            "turns": [{ "role": "user", "parts": [{ "text": text }] }],
            "turnComplete": true
        }
    })
    .to_string()
}

/// A participant's typed message. Send [`speaker_label_message`] immediately
/// before this, exactly as push-to-talk does for a spoken turn, so the model
/// attributes a typed message to whoever sent it the same way it attributes
/// speech.
pub fn user_text_message(text: &str) -> String {
    text_turn_message(text)
}

/// Open a push-to-talk turn. Sent when a participant presses PTT down, before
/// their first audio chunk. Under manual activity detection the model treats
/// everything until [`activity_end_message`] as one user turn.
pub fn activity_start_message() -> String {
    serde_json::json!({ "realtimeInput": { "activityStart": {} } }).to_string()
}

/// Close a push-to-talk turn. Sent after the last audio chunk of a hold — in
/// practice after a short post-release drain so the question's tail is not
/// clipped (the data-channel release can outrun the media path; see #657).
pub fn activity_end_message() -> String {
    serde_json::json!({ "realtimeInput": { "activityEnd": {} } }).to_string()
}

/// Tell the model what became of an action it asked for (#658).
///
/// The tool response itself is answered immediately with `control_not_granted`
/// — a Live API function call that blocks while a human decides stalls the whole
/// conversation — so the eventual outcome has to arrive out of band. Sent as
/// `realtimeInput.text` machine context for the same reason the accessibility
/// digest is: a `clientContent` turn can be implicitly committed and read as a
/// participant speaking, which would make the model reply to its own audit
/// trail.
pub fn control_outcome_message(tool: &str, ok: bool, code: &str, detail: &str) -> String {
    let outcome = if ok { "was performed" } else { "was refused" };
    serde_json::json!({
        "realtimeInput": {
            "text": format!(
                "(Window-control outcome. Machine-provided context, not something a participant said; do not respond to it. The {tool} action you requested {outcome} (code: {code}). {detail})"
            )
        }
    })
    .to_string()
}

/// A realtime PCM16/16 kHz mono audio chunk (`audio/pcm;rate=16000`).
///
/// Uses `realtimeInput.audio` — the older `realtimeInput.mediaChunks` array is
/// rejected by current Live models with close code 1007.
pub fn audio_chunk_message(pcm16: &[u8]) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(pcm16);
    serde_json::json!({
        "realtimeInput": {
            "audio": { "mimeType": "audio/pcm;rate=16000", "data": data }
        }
    })
    .to_string()
}

/// A realtime JPEG video frame (`image/jpeg`), streamed independently of PTT
/// activity turns. Uses `realtimeInput.video` (see [`audio_chunk_message`] on
/// why `mediaChunks` is gone).
pub fn video_frame_message(jpeg: &[u8]) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(jpeg);
    serde_json::json!({
        "realtimeInput": {
            "video": { "mimeType": "image/jpeg", "data": data }
        }
    })
    .to_string()
}

/// Label the upcoming PTT turn with the speaking participant, so the model can
/// attribute questions in a multi-party room. Sent as `realtimeInput.text`
/// (machine context) rather than a `clientContent` turn — the same reason the
/// accessibility refresh does: it must not itself read as a user utterance that
/// triggers a reply. Send it just after `activityStart`, before the audio.
pub fn speaker_label_message(display_name: &str) -> String {
    serde_json::json!({
        "realtimeInput": {
            "text": format!(
                "(Meeting context, not something a participant said: the next spoken turn is from {display_name:?}.)"
            )
        }
    })
    .to_string()
}

/// A changed accessibility snapshot. Sent as `realtimeInput.text` rather than a
/// `clientContent` turn: a `clientContent` turn can be implicitly committed and
/// read as a user utterance, making a context refresh trigger a spurious reply.
pub fn ax_digest_update_message(text: &str) -> String {
    serde_json::json!({
        "realtimeInput": {
            "text": format!(
                "(OS accessibility context update. This is machine-provided context, not something a participant said. Do not respond to this update or mention it.\n{text})"
            )
        }
    })
    .to_string()
}

/// The initial accessibility snapshot, merged into a single session-start turn.
/// Kept as `realtimeInput.text` for the same reason as the refresh.
pub fn initial_digest_message(text: &str) -> String {
    serde_json::json!({
        "realtimeInput": {
            "text": format!(
                "(Initial OS accessibility snapshot for the shared window. Machine-provided context, not something a participant said; do not respond to it or mention receiving it.\n{text})"
            )
        }
    })
    .to_string()
}

/// A decoded server message — only the parts the session acts on.
#[derive(Debug, PartialEq)]
pub enum ServerEvent {
    /// `{"setupComplete":{}}` — safe to start streaming.
    SetupComplete,
    /// A chunk of model output audio (PCM16, 24 kHz mono) to play back.
    Audio(Vec<u8>),
    /// Assistant (model) output transcription delta.
    OutputText(String),
    /// User (input) transcription delta.
    InputText(String),
    /// `turnComplete` — model finished its turn.
    TurnComplete,
    /// `interrupted` — a participant barged in; stop playback of the current turn.
    Interrupted,
    /// `goAway` — the server is about to close the connection (approaching the
    /// session/connection lifetime). Treated as a normal "time limit" end, not
    /// an error. #654 Q4 confirms the exact shape/timing against a live server.
    GoAway,
    /// The model asked to act on the window (#658). Always passes through the
    /// fail-closed policy gate before anything happens.
    /// One Gemini `toolCall` envelope. Calls stay grouped so the session can
    /// serialize them through its one controller/pending-approval lane.
    ToolCallBatch(Vec<FunctionCall>),
    /// Anything we don't act on (keep-alives, usage metadata, …).
    Other,
}

// ---- raw deserialization shapes -------------------------------------------

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default, rename = "setupComplete")]
    setup_complete: Option<serde_json::Value>,
    #[serde(default, rename = "serverContent")]
    server_content: Option<ServerContent>,
    #[serde(default, rename = "goAway")]
    go_away: Option<serde_json::Value>,
    #[serde(default, rename = "toolCall")]
    tool_call: Option<ToolCall>,
}

#[derive(Deserialize, Default)]
struct ToolCall {
    #[serde(default, rename = "functionCalls")]
    function_calls: Vec<FunctionCall>,
}

#[derive(Debug, Deserialize, Default, PartialEq)]
pub struct FunctionCall {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Deserialize, Default)]
struct ServerContent {
    #[serde(default, rename = "modelTurn")]
    model_turn: Option<ModelTurn>,
    #[serde(default, rename = "outputTranscription")]
    output_transcription: Option<Transcription>,
    #[serde(default, rename = "inputTranscription")]
    input_transcription: Option<Transcription>,
    #[serde(default, rename = "turnComplete")]
    turn_complete: bool,
    #[serde(default)]
    interrupted: bool,
}

#[derive(Deserialize, Default)]
struct ModelTurn {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Deserialize, Default)]
struct Part {
    #[serde(default, rename = "inlineData")]
    inline_data: Option<InlineData>,
}

#[derive(Deserialize)]
struct InlineData {
    #[serde(default, rename = "mimeType")]
    mime_type: String,
    #[serde(default)]
    data: String,
}

#[derive(Deserialize, Default)]
struct Transcription {
    #[serde(default)]
    text: String,
}

/// Parse one server frame (text or the UTF-8 of a binary frame) into the events
/// it carries. A single frame may hold several (audio + a transcription delta +
/// turnComplete), so this returns a `Vec`. Unrecognized/unparseable frames yield
/// `[Other]`.
pub fn parse_server_message(raw: &str) -> Vec<ServerEvent> {
    let parsed: RawMessage = match serde_json::from_str(raw) {
        Ok(m) => m,
        Err(_) => return vec![ServerEvent::Other],
    };

    let mut events = Vec::new();

    if parsed.setup_complete.is_some() {
        events.push(ServerEvent::SetupComplete);
    }

    if let Some(sc) = parsed.server_content {
        if let Some(turn) = sc.model_turn {
            for part in turn.parts {
                if let Some(inline) = part.inline_data {
                    // Model audio output: `audio/pcm;rate=24000`.
                    if inline.mime_type.starts_with("audio/") && !inline.data.is_empty() {
                        if let Ok(bytes) =
                            base64::engine::general_purpose::STANDARD.decode(&inline.data)
                        {
                            events.push(ServerEvent::Audio(bytes));
                        }
                    }
                }
            }
        }
        if let Some(out) = sc.output_transcription {
            if !out.text.is_empty() {
                events.push(ServerEvent::OutputText(out.text));
            }
        }
        if let Some(inp) = sc.input_transcription {
            if !inp.text.is_empty() {
                events.push(ServerEvent::InputText(inp.text));
            }
        }
        if sc.interrupted {
            events.push(ServerEvent::Interrupted);
        }
        if sc.turn_complete {
            events.push(ServerEvent::TurnComplete);
        }
    }

    if parsed.go_away.is_some() {
        events.push(ServerEvent::GoAway);
    }

    if let Some(tc) = parsed.tool_call {
        if !tc.function_calls.is_empty() {
            events.push(ServerEvent::ToolCallBatch(tc.function_calls));
        }
    }

    if events.is_empty() {
        events.push(ServerEvent::Other);
    }
    events
}

/// Decode interleaved little-endian PCM16 bytes to f32 samples in [-1, 1], for
/// feeding a `rodio::buffer::SamplesBuffer` on playback.
pub fn pcm16_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

/// Encode f32 samples in [-1, 1] to interleaved little-endian PCM16, for the
/// mic-capture uplink (16 kHz mono). Values are clamped before scaling so a
/// hot sample can't wrap to the opposite polarity.
pub fn f32_to_pcm16(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let scaled = (clamped * 32767.0).round() as i16;
        out.extend_from_slice(&scaled.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_is_a_complete_client_content_turn_forbidding_ungrounded_description() {
        let s = greeting_trigger_message();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["clientContent"]["turns"][0]["role"], "user");
        assert_eq!(v["clientContent"]["turnComplete"], true);
        let text = v["clientContent"]["turns"][0]["parts"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains(GREETING), "{text}");
        assert!(
            text.contains("until a screenshot frame or accessibility snapshot actually arrives"),
            "{text}"
        );
        assert!(
            text.contains("not something a participant said"),
            "the model must not treat this as user speech: {text}"
        );
    }

    #[test]
    fn user_text_message_is_a_complete_client_content_turn_with_the_literal_text() {
        let s = user_text_message("what does this button do?");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["clientContent"]["turns"][0]["role"], "user");
        assert_eq!(v["clientContent"]["turnComplete"], true);
        assert_eq!(
            v["clientContent"]["turns"][0]["parts"][0]["text"],
            "what does this button do?",
            "unlike the greeting, a typed message is sent verbatim -- no \
             control-message framing wraps the participant's own words"
        );
    }

    #[test]
    fn setup_requests_audio_transcription_and_manual_activity() {
        let s = setup_message(DEFAULT_MODEL_ID);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["setup"]["model"], DEFAULT_MODEL_ID);
        assert_eq!(
            v["setup"]["generationConfig"]["responseModalities"][0],
            "AUDIO"
        );
        assert!(v["setup"]["inputAudioTranscription"].is_object());
        assert!(v["setup"]["outputAudioTranscription"].is_object());
        // Push-to-talk: automatic activity detection must be OFF.
        assert_eq!(
            v["setup"]["realtimeInputConfig"]["automaticActivityDetection"]["disabled"],
            true
        );
    }

    #[test]
    fn setup_declares_no_tools_in_phase_one() {
        let v: serde_json::Value = serde_json::from_str(&setup_message(DEFAULT_MODEL_ID)).unwrap();
        // Tools arrive in #658 behind the control gate; phase 1 has none.
        assert!(v["setup"]["tools"].is_null());
        let instruction = v["setup"]["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap();
        assert!(instruction.contains("push-to-talk"));
        assert!(instruction.contains("[n]"));
        // No window-control verbs leak into the phase-1 instruction.
        assert!(!instruction.contains("window_click"));
    }

    #[test]
    fn setup_uses_the_model_it_is_given_not_a_constant() {
        // Hosted mode passes the model from the token response verbatim.
        let hosted = "models/gemini-9.9-some-future-live-preview";
        let v: serde_json::Value = serde_json::from_str(&setup_message(hosted)).unwrap();
        assert_eq!(v["setup"]["model"], hosted);
    }

    #[test]
    fn activity_brackets_are_well_formed() {
        let start: serde_json::Value = serde_json::from_str(&activity_start_message()).unwrap();
        assert!(start["realtimeInput"]["activityStart"].is_object());
        let end: serde_json::Value = serde_json::from_str(&activity_end_message()).unwrap();
        assert!(end["realtimeInput"]["activityEnd"].is_object());
    }

    #[test]
    fn audio_chunk_is_base64_pcm_16k() {
        let s = audio_chunk_message(&[0x01, 0x02, 0x03]);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let chunk = &v["realtimeInput"]["audio"];
        assert_eq!(chunk["mimeType"], "audio/pcm;rate=16000");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(chunk["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn video_frame_is_base64_jpeg() {
        let s = video_frame_message(&[0xFF, 0xD8, 0xFF]);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let chunk = &v["realtimeInput"]["video"];
        assert_eq!(chunk["mimeType"], "image/jpeg");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(chunk["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, vec![0xFF, 0xD8, 0xFF]);
    }

    #[test]
    fn speaker_label_and_digest_are_machine_context_not_turns() {
        for s in [
            speaker_label_message("Alice"),
            ax_digest_update_message("[0] AXStaticText \"x\""),
            initial_digest_message("AX snapshot gen 0"),
        ] {
            let v: serde_json::Value = serde_json::from_str(&s).unwrap();
            // Never a clientContent turn (would risk a spurious reply).
            assert!(v["clientContent"].is_null());
            let text = v["realtimeInput"]["text"].as_str().unwrap();
            assert!(text.contains("not something a participant said"));
        }
    }

    #[test]
    fn a_control_outcome_is_machine_context_and_names_its_code() {
        let refused = control_outcome_message(
            "window_click",
            false,
            "stale_digest_generation",
            "Use the latest snapshot.",
        );
        let v: serde_json::Value = serde_json::from_str(&refused).unwrap();
        // Never a turn — it must not read as a participant speaking.
        assert!(v["clientContent"].is_null());
        let text = v["realtimeInput"]["text"].as_str().unwrap();
        assert!(text.contains("not something a participant said"), "{text}");
        assert!(text.contains("stale_digest_generation"), "{text}");
        assert!(text.contains("was refused"), "{text}");

        let performed = control_outcome_message("window_type", true, "ok", "");
        let v: serde_json::Value = serde_json::from_str(&performed).unwrap();
        assert!(v["realtimeInput"]["text"]
            .as_str()
            .unwrap()
            .contains("was performed"));
    }

    #[test]
    fn speaker_label_names_the_speaker() {
        let v: serde_json::Value = serde_json::from_str(&speaker_label_message("Alice")).unwrap();
        assert!(v["realtimeInput"]["text"]
            .as_str()
            .unwrap()
            .contains("Alice"));
    }

    #[test]
    fn parses_setup_complete() {
        assert_eq!(
            parse_server_message(r#"{"setupComplete":{}}"#),
            vec![ServerEvent::SetupComplete]
        );
    }

    #[test]
    fn parses_audio_output() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([0u8, 1, 2, 3]);
        let raw = format!(
            r#"{{"serverContent":{{"modelTurn":{{"parts":[{{"inlineData":{{"mimeType":"audio/pcm;rate=24000","data":"{b64}"}}}}]}}}}}}"#
        );
        assert_eq!(
            parse_server_message(&raw),
            vec![ServerEvent::Audio(vec![0, 1, 2, 3])]
        );
    }

    #[test]
    fn parses_transcription_deltas_and_turn_complete() {
        assert_eq!(
            parse_server_message(r#"{"serverContent":{"outputTranscription":{"text":"hi"}}}"#),
            vec![ServerEvent::OutputText("hi".into())]
        );
        assert_eq!(
            parse_server_message(r#"{"serverContent":{"inputTranscription":{"text":"hello"}}}"#),
            vec![ServerEvent::InputText("hello".into())]
        );
        assert_eq!(
            parse_server_message(r#"{"serverContent":{"turnComplete":true}}"#),
            vec![ServerEvent::TurnComplete]
        );
    }

    #[test]
    fn parses_interrupted_and_go_away() {
        assert_eq!(
            parse_server_message(r#"{"serverContent":{"interrupted":true}}"#),
            vec![ServerEvent::Interrupted]
        );
        assert_eq!(
            parse_server_message(r#"{"goAway":{"timeLeft":"5s"}}"#),
            vec![ServerEvent::GoAway]
        );
    }

    #[test]
    fn multiple_events_in_one_frame_preserve_order() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([9u8, 9]);
        let raw = format!(
            r#"{{"serverContent":{{"modelTurn":{{"parts":[{{"inlineData":{{"mimeType":"audio/pcm;rate=24000","data":"{b64}"}}}}]}},"outputTranscription":{{"text":"ok"}},"turnComplete":true}}}}"#
        );
        assert_eq!(
            parse_server_message(&raw),
            vec![
                ServerEvent::Audio(vec![9, 9]),
                ServerEvent::OutputText("ok".into()),
                ServerEvent::TurnComplete,
            ]
        );
    }

    #[test]
    fn tools_are_declared_only_when_asked_for() {
        let with: serde_json::Value =
            serde_json::from_str(&setup_message_with_tools(DEFAULT_MODEL_ID, true)).unwrap();
        let names: Vec<&str> = with["setup"]["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "window_type",
                "window_click",
                "window_press_key",
                "window_scroll"
            ]
        );
        // The control instruction only appears alongside the tools.
        let instruction = with["setup"]["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap();
        assert!(instruction.contains("permission-gated"), "{instruction}");
        assert!(instruction.contains("never retry a refused action"));
    }

    #[test]
    fn tool_schemas_cannot_express_a_dangerous_action() {
        let v: serde_json::Value =
            serde_json::from_str(&setup_message_with_tools(DEFAULT_MODEL_ID, true)).unwrap();
        let decls = v["setup"]["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap();
        // Bounded text.
        assert_eq!(decls[0]["parameters"]["properties"]["text"]["maxLength"], 2000);
        // A click must cite the snapshot generation it came from, so a stale
        // index is detectable rather than silently mis-resolved.
        assert_eq!(
            decls[1]["parameters"]["required"],
            serde_json::json!(["element_index", "generation"])
        );
        // Navigation keys only — no arbitrary chords.
        let keys = decls[2]["parameters"]["properties"]["key"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(keys.len(), 7);
        assert!(!keys.iter().any(|k| k.as_str().unwrap().contains("Cmd")));
        // Bounded scrolling.
        assert_eq!(decls[3]["parameters"]["properties"]["amount"]["maximum"], 100);
    }

    #[test]
    fn parses_a_tool_call() {
        let raw = r#"{"toolCall":{"functionCalls":[{"id":"fc_1","name":"window_click","args":{"generation":2,"element_index":5}}]}}"#;
        match &parse_server_message(raw)[0] {
            ServerEvent::ToolCallBatch(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "fc_1");
                assert_eq!(calls[0].name, "window_click");
                assert_eq!(calls[0].args["generation"], 2);
            }
            other => panic!("expected ToolCallBatch, got {other:?}"),
        }
    }

    #[test]
    fn one_tool_call_envelope_stays_one_ordered_batch() {
        let raw = r#"{"toolCall":{"functionCalls":[
            {"id":"fc_1","name":"window_type","args":{"text":"first"}},
            {"id":"fc_2","name":"window_press_key","args":{"key":"Return"}},
            {"id":"fc_3","name":"window_type","args":{"text":"third"}}
        ]}}"#;
        let events = parse_server_message(raw);
        assert_eq!(events.len(), 1, "a single envelope was split into concurrent events");
        let ServerEvent::ToolCallBatch(calls) = &events[0] else {
            panic!("expected one ToolCallBatch, got {:?}", events[0]);
        };
        assert_eq!(
            calls.iter().map(|call| call.id.as_str()).collect::<Vec<_>>(),
            ["fc_1", "fc_2", "fc_3"]
        );
    }

    #[test]
    fn a_refusal_can_never_be_mistaken_for_success() {
        let raw = tool_response_message("fc_9", "window_click", false, "blocked_terminal", "no");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let response = &v["toolResponse"]["functionResponses"][0];
        // Echoing id + name is what keeps the model from attributing the answer
        // to a different call.
        assert_eq!(response["id"], "fc_9");
        assert_eq!(response["name"], "window_click");
        assert_eq!(response["response"]["ok"], false);
        assert_eq!(response["response"]["code"], "blocked_terminal");
    }

    #[test]
    fn unknown_or_invalid_message_is_other() {
        assert_eq!(
            parse_server_message(r#"{"usageMetadata":{"totalTokenCount":5}}"#),
            vec![ServerEvent::Other]
        );
        assert_eq!(parse_server_message("not json"), vec![ServerEvent::Other]);
    }

    #[test]
    fn pcm16_f32_roundtrip_endpoints() {
        let bytes = [0x00, 0x00, 0xFF, 0x7F, 0x00, 0x80];
        let f = pcm16_to_f32(&bytes);
        assert_eq!(f[0], 0.0);
        assert!((f[1] - 0.999969).abs() < 1e-4);
        assert_eq!(f[2], -1.0);
        // f32 → pcm16 clamps rather than wrapping.
        let hot = f32_to_pcm16(&[0.0, 1.5, -1.5]);
        assert_eq!(i16::from_le_bytes([hot[0], hot[1]]), 0);
        assert_eq!(i16::from_le_bytes([hot[2], hot[3]]), 32767);
        assert_eq!(i16::from_le_bytes([hot[4], hot[5]]), -32767);
    }
}
