-- Remote-control latency dashboard (#379), PostHog project 317298.
--
-- One HogQL insight per block. A block starts with `-- insight: <key>`,
-- followed by `-- name:` and `-- description:` header lines, then the query.
-- scripts/posthog-sync-latency-dashboard.mjs turns this file into insight
-- payloads (dry run only); scripts/test-posthog-sync-latency-dashboard.mjs
-- checks every `properties.*` name against `remoteControlLatencySummary` in
-- contracts/petal-contracts.json. The decision tile is mirrored in JS by
-- scripts/ci/rc-latency-decision-tile.mjs.
--
-- Every query reads the last 14 days of `remote_control_latency_summary` and
-- leaves out self-hosted SFUs. Each event is one control-minute of one side.
-- Host and controller mint their own `rc_session_id`, so the two sides are
-- never joined: cross-side views compare fleet medians.

-- insight: input-leg-by-region
-- name: RC latency: median input leg per region
-- description: Offset-corrected controller->host input leg (host role), median of per-minute p50/p95, by SFU edge region.
SELECT properties.lk_region AS region,
       count(DISTINCT properties.rc_session_id) AS sessions,
       medianExactLow(toFloat(properties.input_leg_ms_p50)) AS median_of_p50,
       medianExactLow(toFloat(properties.input_leg_ms_p95)) AS median_of_p95,
       avg(toFloat(properties.offset_err_ms)) AS offset_err_ms
FROM events
WHERE event = 'remote_control_latency_summary'
  AND timestamp > now() - INTERVAL 14 DAY
  AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
  AND properties.role = 'host'
  AND toFloat(properties.input_leg_ms_n) >= 5
GROUP BY region
ORDER BY sessions DESC

-- insight: input-leg-by-edge
-- name: RC latency: same-edge vs cross-edge input leg
-- description: Host input leg split by whether both sides hit the same SFU node.
SELECT properties.same_edge AS same_edge,
       count(DISTINCT properties.rc_session_id) AS sessions,
       medianExactLow(toFloat(properties.input_leg_ms_p50)) AS median_of_p50,
       medianExactLow(toFloat(properties.input_leg_ms_p95)) AS median_of_p95
FROM events
WHERE event = 'remote_control_latency_summary'
  AND timestamp > now() - INTERVAL 14 DAY
  AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
  AND properties.role = 'host'
  AND toFloat(properties.input_leg_ms_n) >= 5
GROUP BY same_edge
ORDER BY sessions DESC

-- insight: ice-path-mix
-- name: RC latency: ICE path mix
-- description: Selected ICE candidate type and protocol, with RTT and input leg medians per path. The web controller reports no ICE yet.
SELECT properties.ice_candidate_type AS candidate_type,
       properties.ice_protocol AS protocol,
       count() AS control_minutes,
       medianExactLowIf(toFloat(properties.pub_rtt_ms_p50), toFloat(properties.pub_rtt_ms_n) >= 5) AS pub_rtt_p50,
       medianExactLowIf(toFloat(properties.sub_rtt_ms_p50), toFloat(properties.sub_rtt_ms_n) >= 5) AS sub_rtt_p50,
       medianExactLowIf(toFloat(properties.input_leg_ms_p50), toFloat(properties.input_leg_ms_n) >= 5) AS input_leg_p50
FROM events
WHERE event = 'remote_control_latency_summary'
  AND timestamp > now() - INTERVAL 14 DAY
  AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
  AND properties.ice_candidate_type IS NOT NULL
GROUP BY candidate_type, protocol
ORDER BY control_minutes DESC

-- insight: stall-rate-by-region
-- name: RC latency: head-of-line stall rate
-- description: Share of host control-minutes with at least one input stall, by region. Only minutes where the stall detector ran count.
SELECT properties.lk_region AS region,
       count() AS control_minutes,
       countIf(ifNull(toFloat(properties.input_stall_count), 0) > 0) / count() AS stall_minute_share
FROM events
WHERE event = 'remote_control_latency_summary'
  AND timestamp > now() - INTERVAL 14 DAY
  AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
  AND properties.role = 'host'
  AND toFloat(properties.input_excess_delay_ms_n) >= 1
GROUP BY region
ORDER BY control_minutes DESC

-- insight: stage-medians
-- name: RC latency: stage share of photon
-- description: Fleet median of each stage's per-minute p50 against photon. The residual is shown as unattributed; it is NULL until every stage has data.
SELECT medianExactLowIf(toFloat(properties.photon_ms_p50), toFloat(properties.photon_ms_n) >= 5) AS photon,
       medianExactLowIf(toFloat(properties.input_leg_ms_p50), toFloat(properties.input_leg_ms_n) >= 5) AS input_leg,
       medianExactLowIf(toFloat(properties.replay_inject_ms_p50), toFloat(properties.replay_inject_ms_n) >= 5) AS replay_inject,
       medianExactLowIf(toFloat(properties.inject_to_capture_ms_p50), toFloat(properties.inject_to_capture_ms_n) >= 5) AS inject_to_capture,
       medianExactLowIf(toFloat(properties.capture_to_encoded_ms_p50), toFloat(properties.capture_to_encoded_ms_n) >= 5) AS capture_to_encoded,
       medianExactLowIf(toFloat(properties.encoded_to_packetized_ms_p50), toFloat(properties.encoded_to_packetized_ms_n) >= 5) AS encoded_to_packetized,
       medianExactLowIf(toFloat(properties.video_net_ms_p50), toFloat(properties.video_net_ms_n) >= 5) AS network,
       medianExactLowIf(toFloat(properties.jb_delay_ms_p50), toFloat(properties.jb_delay_ms_n) >= 5) AS jb_delay,
       medianExactLowIf(toFloat(properties.decode_ms_p50), toFloat(properties.decode_ms_n) >= 5) AS decode,
       medianExactLowIf(toFloat(properties.receive_to_present_ms_p50), toFloat(properties.receive_to_present_ms_n) >= 5) AS receive_to_present,
       if(countIf(toFloat(properties.photon_ms_n) >= 5) > 0
            AND countIf(toFloat(properties.input_leg_ms_n) >= 5) > 0
            AND countIf(toFloat(properties.replay_inject_ms_n) >= 5) > 0
            AND countIf(toFloat(properties.inject_to_capture_ms_n) >= 5) > 0
            AND countIf(toFloat(properties.capture_to_encoded_ms_n) >= 5) > 0
            AND countIf(toFloat(properties.encoded_to_packetized_ms_n) >= 5) > 0
            AND countIf(toFloat(properties.video_net_ms_n) >= 5) > 0
            AND countIf(toFloat(properties.jb_delay_ms_n) >= 5) > 0
            AND countIf(toFloat(properties.decode_ms_n) >= 5) > 0
            AND countIf(toFloat(properties.receive_to_present_ms_n) >= 5) > 0,
          photon - input_leg - replay_inject - inject_to_capture - capture_to_encoded - encoded_to_packetized - network - jb_delay - decode - receive_to_present,
          NULL) AS unattributed
FROM events
WHERE event = 'remote_control_latency_summary'
  AND timestamp > now() - INTERVAL 14 DAY
  AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'

-- insight: photon-by-client
-- name: RC latency: photon by client and OS
-- description: Daily median of per-minute photon p50 and p95 by controller client and OS, against the < 100 ms target.
SELECT toStartOfDay(timestamp) AS day,
       properties.client AS client,
       properties.os AS os,
       medianExactLow(toFloat(properties.photon_ms_p50)) AS photon_p50,
       medianExactLow(toFloat(properties.photon_ms_p95)) AS photon_p95,
       100 AS target_ms
FROM events
WHERE event = 'remote_control_latency_summary'
  AND timestamp > now() - INTERVAL 14 DAY
  AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
  AND toFloat(properties.photon_ms_n) >= 5
GROUP BY day, client, os
ORDER BY day

-- insight: jitter-buffer
-- name: RC latency: window jitter buffer
-- description: Controlled window's jitter buffer delay and target by client and jitterBufferMinimumDelay bucket.
SELECT properties.client AS client,
       properties.jb_min_ms AS jb_min_bucket,
       count() AS control_minutes,
       medianExactLow(toFloat(properties.jb_delay_ms_p50)) AS jb_delay_p50,
       medianExactLowIf(toFloat(properties.jb_target_ms_p50), toFloat(properties.jb_target_ms_n) >= 5) AS jb_target_p50
FROM events
WHERE event = 'remote_control_latency_summary'
  AND timestamp > now() - INTERVAL 14 DAY
  AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
  AND properties.role = 'controller'
  AND toFloat(properties.jb_delay_ms_n) >= 5
GROUP BY client, jb_min_bucket
ORDER BY control_minutes DESC

-- insight: data-health
-- name: RC latency: data health
-- description: Summary events per day and role, self-hosted included, with the share of controller minutes lacking frame metadata or a fresh clock offset.
-- self-hosted: included
SELECT toStartOfDay(timestamp) AS day,
       properties.role AS role,
       count() AS events,
       count(DISTINCT properties.rc_session_id) AS sessions,
       countIf(ifNull(toString(properties.self_hosted_sfu), '') = 'true') AS self_hosted_events,
       countIf(properties.frame_metadata = 'unavailable') / count() AS frame_metadata_unavailable_share,
       countIf(properties.offset_err_ms IS NULL) / count() AS no_fresh_offset_share
FROM events
WHERE event = 'remote_control_latency_summary'
  AND timestamp > now() - INTERVAL 14 DAY
GROUP BY day, role
ORDER BY day, role

-- insight: decision-tile
-- name: RC latency: decision tile
-- description: One row per option (a)/(b) threshold. INSUFFICIENT unless the rule's data covers >= 50 sessions over >= 14 calendar days; a missing stage is never read as 0.
WITH stage_stats AS (
  SELECT medianExactLowIf(toFloat(properties.photon_ms_p50), (toFloat(properties.photon_ms_n) >= 5 AND toFloat(properties.photon_ms_p50) IS NOT NULL)) AS photon_med,
         uniqExactIf(properties.rc_session_id, (toFloat(properties.photon_ms_n) >= 5 AND toFloat(properties.photon_ms_p50) IS NOT NULL)) AS photon_sessions,
         dateDiff('day', minIf(timestamp, (toFloat(properties.photon_ms_n) >= 5 AND toFloat(properties.photon_ms_p50) IS NOT NULL)), maxIf(timestamp, (toFloat(properties.photon_ms_n) >= 5 AND toFloat(properties.photon_ms_p50) IS NOT NULL))) + 1 AS photon_days,
         medianExactLowIf(toFloat(properties.input_leg_ms_p50), (toFloat(properties.input_leg_ms_n) >= 5 AND toFloat(properties.input_leg_ms_p50) IS NOT NULL)) AS input_leg_med,
         (uniqExactIf(properties.rc_session_id, (toFloat(properties.input_leg_ms_n) >= 5 AND toFloat(properties.input_leg_ms_p50) IS NOT NULL)) >= 50
           AND dateDiff('day', minIf(timestamp, (toFloat(properties.input_leg_ms_n) >= 5 AND toFloat(properties.input_leg_ms_p50) IS NOT NULL)), maxIf(timestamp, (toFloat(properties.input_leg_ms_n) >= 5 AND toFloat(properties.input_leg_ms_p50) IS NOT NULL))) + 1 >= 14) AS input_leg_ok,
         medianExactLowIf(toFloat(properties.replay_inject_ms_p50), (toFloat(properties.replay_inject_ms_n) >= 5 AND toFloat(properties.replay_inject_ms_p50) IS NOT NULL)) AS replay_inject_med,
         (uniqExactIf(properties.rc_session_id, (toFloat(properties.replay_inject_ms_n) >= 5 AND toFloat(properties.replay_inject_ms_p50) IS NOT NULL)) >= 50
           AND dateDiff('day', minIf(timestamp, (toFloat(properties.replay_inject_ms_n) >= 5 AND toFloat(properties.replay_inject_ms_p50) IS NOT NULL)), maxIf(timestamp, (toFloat(properties.replay_inject_ms_n) >= 5 AND toFloat(properties.replay_inject_ms_p50) IS NOT NULL))) + 1 >= 14) AS replay_inject_ok,
         medianExactLowIf(toFloat(properties.inject_to_capture_ms_p50), (toFloat(properties.inject_to_capture_ms_n) >= 5 AND toFloat(properties.inject_to_capture_ms_p50) IS NOT NULL)) AS inject_to_capture_med,
         (uniqExactIf(properties.rc_session_id, (toFloat(properties.inject_to_capture_ms_n) >= 5 AND toFloat(properties.inject_to_capture_ms_p50) IS NOT NULL)) >= 50
           AND dateDiff('day', minIf(timestamp, (toFloat(properties.inject_to_capture_ms_n) >= 5 AND toFloat(properties.inject_to_capture_ms_p50) IS NOT NULL)), maxIf(timestamp, (toFloat(properties.inject_to_capture_ms_n) >= 5 AND toFloat(properties.inject_to_capture_ms_p50) IS NOT NULL))) + 1 >= 14) AS inject_to_capture_ok,
         medianExactLowIf(toFloat(properties.capture_to_encoded_ms_p50), (toFloat(properties.capture_to_encoded_ms_n) >= 5 AND toFloat(properties.capture_to_encoded_ms_p50) IS NOT NULL)) AS capture_to_encoded_med,
         (uniqExactIf(properties.rc_session_id, (toFloat(properties.capture_to_encoded_ms_n) >= 5 AND toFloat(properties.capture_to_encoded_ms_p50) IS NOT NULL)) >= 50
           AND dateDiff('day', minIf(timestamp, (toFloat(properties.capture_to_encoded_ms_n) >= 5 AND toFloat(properties.capture_to_encoded_ms_p50) IS NOT NULL)), maxIf(timestamp, (toFloat(properties.capture_to_encoded_ms_n) >= 5 AND toFloat(properties.capture_to_encoded_ms_p50) IS NOT NULL))) + 1 >= 14) AS capture_to_encoded_ok,
         medianExactLowIf(toFloat(properties.encoded_to_packetized_ms_p50), (toFloat(properties.encoded_to_packetized_ms_n) >= 5 AND toFloat(properties.encoded_to_packetized_ms_p50) IS NOT NULL)) AS encoded_to_packetized_med,
         (uniqExactIf(properties.rc_session_id, (toFloat(properties.encoded_to_packetized_ms_n) >= 5 AND toFloat(properties.encoded_to_packetized_ms_p50) IS NOT NULL)) >= 50
           AND dateDiff('day', minIf(timestamp, (toFloat(properties.encoded_to_packetized_ms_n) >= 5 AND toFloat(properties.encoded_to_packetized_ms_p50) IS NOT NULL)), maxIf(timestamp, (toFloat(properties.encoded_to_packetized_ms_n) >= 5 AND toFloat(properties.encoded_to_packetized_ms_p50) IS NOT NULL))) + 1 >= 14) AS encoded_to_packetized_ok,
         medianExactLowIf(toFloat(properties.video_net_ms_p50), (toFloat(properties.video_net_ms_n) >= 5 AND toFloat(properties.video_net_ms_p50) IS NOT NULL)) AS video_net_med,
         (uniqExactIf(properties.rc_session_id, (toFloat(properties.video_net_ms_n) >= 5 AND toFloat(properties.video_net_ms_p50) IS NOT NULL)) >= 50
           AND dateDiff('day', minIf(timestamp, (toFloat(properties.video_net_ms_n) >= 5 AND toFloat(properties.video_net_ms_p50) IS NOT NULL)), maxIf(timestamp, (toFloat(properties.video_net_ms_n) >= 5 AND toFloat(properties.video_net_ms_p50) IS NOT NULL))) + 1 >= 14) AS video_net_ok,
         medianExactLowIf(toFloat(properties.jb_delay_ms_p50), (toFloat(properties.jb_delay_ms_n) >= 5 AND toFloat(properties.jb_delay_ms_p50) IS NOT NULL)) AS jb_delay_med,
         (uniqExactIf(properties.rc_session_id, (toFloat(properties.jb_delay_ms_n) >= 5 AND toFloat(properties.jb_delay_ms_p50) IS NOT NULL)) >= 50
           AND dateDiff('day', minIf(timestamp, (toFloat(properties.jb_delay_ms_n) >= 5 AND toFloat(properties.jb_delay_ms_p50) IS NOT NULL)), maxIf(timestamp, (toFloat(properties.jb_delay_ms_n) >= 5 AND toFloat(properties.jb_delay_ms_p50) IS NOT NULL))) + 1 >= 14) AS jb_delay_ok,
         medianExactLowIf(toFloat(properties.decode_ms_p50), (toFloat(properties.decode_ms_n) >= 5 AND toFloat(properties.decode_ms_p50) IS NOT NULL)) AS decode_med,
         (uniqExactIf(properties.rc_session_id, (toFloat(properties.decode_ms_n) >= 5 AND toFloat(properties.decode_ms_p50) IS NOT NULL)) >= 50
           AND dateDiff('day', minIf(timestamp, (toFloat(properties.decode_ms_n) >= 5 AND toFloat(properties.decode_ms_p50) IS NOT NULL)), maxIf(timestamp, (toFloat(properties.decode_ms_n) >= 5 AND toFloat(properties.decode_ms_p50) IS NOT NULL))) + 1 >= 14) AS decode_ok,
         medianExactLowIf(toFloat(properties.receive_to_present_ms_p50), (toFloat(properties.receive_to_present_ms_n) >= 5 AND toFloat(properties.receive_to_present_ms_p50) IS NOT NULL)) AS receive_to_present_med,
         (uniqExactIf(properties.rc_session_id, (toFloat(properties.receive_to_present_ms_n) >= 5 AND toFloat(properties.receive_to_present_ms_p50) IS NOT NULL)) >= 50
           AND dateDiff('day', minIf(timestamp, (toFloat(properties.receive_to_present_ms_n) >= 5 AND toFloat(properties.receive_to_present_ms_p50) IS NOT NULL)), maxIf(timestamp, (toFloat(properties.receive_to_present_ms_n) >= 5 AND toFloat(properties.receive_to_present_ms_p50) IS NOT NULL))) + 1 >= 14) AS receive_to_present_ok
  FROM events
  WHERE event = 'remote_control_latency_summary'
    AND timestamp > now() - INTERVAL 14 DAY
    AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
)
SELECT rule, value, threshold, sessions, span_days, fires FROM (
  SELECT 'a_input_leg_p50_share' AS rule,
         toFloat(countIf(session_p50 > 20) / count()) AS value,
         '>= 25% of sessions with median input_leg_ms_p50 > 20 ms' AS threshold,
         toInt(count()) AS sessions,
         toInt(dateDiff('day', min(first_ts), max(last_ts)) + 1) AS span_days,
         multiIf(sessions < 50 OR span_days < 14, 'INSUFFICIENT', value >= 0.25, 'FIRES', 'CLEAR') AS fires
  FROM (
    SELECT properties.rc_session_id AS sid,
           medianExactLow(toFloat(properties.input_leg_ms_p50)) AS session_p50,
           min(timestamp) AS first_ts,
           max(timestamp) AS last_ts
    FROM events
    WHERE event = 'remote_control_latency_summary'
      AND timestamp > now() - INTERVAL 14 DAY
      AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
      AND (toFloat(properties.input_leg_ms_n) >= 5 AND toFloat(properties.input_leg_ms_p50) IS NOT NULL)
    GROUP BY sid
  )

  UNION ALL

  SELECT 'a_input_leg_p95_fleet' AS rule,
         toFloat(medianExactLow(toFloat(properties.input_leg_ms_p95))) AS value,
         'fleet median of input_leg_ms_p95 > 60 ms' AS threshold,
         toInt(count(DISTINCT properties.rc_session_id)) AS sessions,
         toInt(dateDiff('day', min(timestamp), max(timestamp)) + 1) AS span_days,
         multiIf(sessions < 50 OR span_days < 14, 'INSUFFICIENT', value > 60, 'FIRES', 'CLEAR') AS fires
  FROM events
  WHERE event = 'remote_control_latency_summary'
    AND timestamp > now() - INTERVAL 14 DAY
    AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
    AND (toFloat(properties.input_leg_ms_n) >= 5 AND toFloat(properties.input_leg_ms_p95) IS NOT NULL)

  UNION ALL

  SELECT 'a_stall_minutes' AS rule,
         toFloat(countIf(ifNull(toFloat(properties.input_stall_count), 0) > 0) / count()) AS value,
         '> 10% of control-minutes with input_stall_count > 0' AS threshold,
         toInt(count(DISTINCT properties.rc_session_id)) AS sessions,
         toInt(dateDiff('day', min(timestamp), max(timestamp)) + 1) AS span_days,
         multiIf(sessions < 50 OR span_days < 14, 'INSUFFICIENT', value > 0.1, 'FIRES', 'CLEAR') AS fires
  FROM events
  WHERE event = 'remote_control_latency_summary'
    AND timestamp > now() - INTERVAL 14 DAY
    AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
    AND toFloat(properties.input_excess_delay_ms_n) >= 1

  UNION ALL

  SELECT 'a_cross_edge_penalty' AS rule,
         toFloat(if(cross_sessions > 0 AND same_sessions > 0, cross_p50 - same_p50, NULL)) AS value,
         'cross-edge minus same-edge session median input_leg_ms_p50 > 15 ms with > 30% cross-edge sessions' AS threshold,
         toInt(edge_sessions) AS sessions,
         toInt(edge_days) AS span_days,
         multiIf(sessions < 50 OR span_days < 14 OR cross_sessions = 0 OR same_sessions = 0, 'INSUFFICIENT',
                 value > 15 AND cross_share > 0.3, 'FIRES', 'CLEAR') AS fires
  FROM (
    SELECT medianExactLowIf(session_p50, edge = 'false') AS cross_p50,
           medianExactLowIf(session_p50, edge = 'true') AS same_p50,
           countIf(edge = 'false') AS cross_sessions,
           countIf(edge = 'true') AS same_sessions,
           count() AS edge_sessions,
           cross_sessions / edge_sessions AS cross_share,
           dateDiff('day', min(first_ts), max(last_ts)) + 1 AS edge_days
    FROM (
      SELECT properties.rc_session_id AS sid,
             medianExactLow(toFloat(properties.input_leg_ms_p50)) AS session_p50,
             argMax(ifNull(toString(properties.same_edge), 'unknown'), timestamp) AS edge,
             min(timestamp) AS first_ts,
             max(timestamp) AS last_ts
      FROM events
      WHERE event = 'remote_control_latency_summary'
        AND timestamp > now() - INTERVAL 14 DAY
        AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
        AND (toFloat(properties.input_leg_ms_n) >= 5 AND toFloat(properties.input_leg_ms_p50) IS NOT NULL)
      GROUP BY sid
      -- The latest interval decides; a session ending `unknown` is left out.
      HAVING edge IN ('true', 'false')
    )
  )

  UNION ALL

  SELECT 'b_jb_delay' AS rule,
         toFloat(medianExactLow(toFloat(properties.jb_delay_ms_p50))) AS value,
         'fleet median of jb_delay_ms_p50 > 30 ms' AS threshold,
         toInt(count(DISTINCT properties.rc_session_id)) AS sessions,
         toInt(dateDiff('day', min(timestamp), max(timestamp)) + 1) AS span_days,
         multiIf(sessions < 50 OR span_days < 14, 'INSUFFICIENT', value > 30, 'FIRES', 'CLEAR') AS fires
  FROM events
  WHERE event = 'remote_control_latency_summary'
    AND timestamp > now() - INTERVAL 14 DAY
    AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
    AND (toFloat(properties.jb_delay_ms_n) >= 5 AND toFloat(properties.jb_delay_ms_p50) IS NOT NULL)

  UNION ALL

  SELECT 'b_jb_target_vs_min' AS rule,
         toFloat(medianExactLow(toFloat(properties.jb_target_ms_p50)
           / multiIf(properties.jb_min_ms = '1_19', 19, properties.jb_min_ms = '20_49', 49, properties.jb_min_ms = '50_99', 99, 199))) AS value,
         'fleet median of jb_target_ms_p50 / jb_min_ms bucket upper bound > 2' AS threshold,
         toInt(count(DISTINCT properties.rc_session_id)) AS sessions,
         toInt(dateDiff('day', min(timestamp), max(timestamp)) + 1) AS span_days,
         multiIf(sessions < 50 OR span_days < 14, 'INSUFFICIENT', value > 2, 'FIRES', 'CLEAR') AS fires
  FROM events
  WHERE event = 'remote_control_latency_summary'
    AND timestamp > now() - INTERVAL 14 DAY
    AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
    AND (toFloat(properties.jb_target_ms_n) >= 5 AND toFloat(properties.jb_target_ms_p50) IS NOT NULL)
    AND properties.jb_min_ms IN ('1_19', '20_49', '50_99', '100_199')

  UNION ALL

  SELECT 'b_receive_to_present' AS rule,
         toFloat(medianExactLow(toFloat(properties.receive_to_present_ms_p50))) AS value,
         'fleet median of receive_to_present_ms_p50 > 25 ms' AS threshold,
         toInt(count(DISTINCT properties.rc_session_id)) AS sessions,
         toInt(dateDiff('day', min(timestamp), max(timestamp)) + 1) AS span_days,
         multiIf(sessions < 50 OR span_days < 14, 'INSUFFICIENT', value > 25, 'FIRES', 'CLEAR') AS fires
  FROM events
  WHERE event = 'remote_control_latency_summary'
    AND timestamp > now() - INTERVAL 14 DAY
    AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
    AND (toFloat(properties.receive_to_present_ms_n) >= 5 AND toFloat(properties.receive_to_present_ms_p50) IS NOT NULL)

  UNION ALL

  SELECT 'b_photon_residual' AS rule,
         toFloat(if(all_ok, photon_med - input_leg_med - replay_inject_med - inject_to_capture_med
                              - capture_to_encoded_med - encoded_to_packetized_med - video_net_med, NULL)) AS value,
         'fleet median photon_ms_p50 minus send and network stage medians > 50 ms' AS threshold,
         toInt(photon_sessions) AS sessions,
         toInt(photon_days) AS span_days,
         multiIf(NOT all_ok, 'INSUFFICIENT', value > 50, 'FIRES', 'CLEAR') AS fires
  FROM (
    SELECT *,
           (photon_sessions >= 50 AND photon_days >= 14 AND input_leg_ok AND replay_inject_ok AND inject_to_capture_ok
             AND capture_to_encoded_ok AND encoded_to_packetized_ok AND video_net_ok) AS all_ok
    FROM stage_stats
  )

  UNION ALL

  SELECT 'capture_cadence' AS rule,
         toFloat(medianExactLow(toFloat(properties.inject_to_capture_ms_p50))) AS value,
         'fleet median of inject_to_capture_ms_p50 > 33 ms' AS threshold,
         toInt(count(DISTINCT properties.rc_session_id)) AS sessions,
         toInt(dateDiff('day', min(timestamp), max(timestamp)) + 1) AS span_days,
         multiIf(sessions < 50 OR span_days < 14, 'INSUFFICIENT', value > 33, 'FIRES', 'CLEAR') AS fires
  FROM events
  WHERE event = 'remote_control_latency_summary'
    AND timestamp > now() - INTERVAL 14 DAY
    AND ifNull(toString(properties.self_hosted_sfu), '') != 'true'
    AND (toFloat(properties.inject_to_capture_ms_n) >= 5 AND toFloat(properties.inject_to_capture_ms_p50) IS NOT NULL)

  UNION ALL

  -- A missing stage could be the one over 40%, so only FIRES can ignore it.
  SELECT 'stage_share_of_photon' AS rule,
         toFloat(if(photon_ok, greatest(if(input_leg_ok, input_leg_med, 0), if(replay_inject_ok, replay_inject_med, 0),
                                       if(inject_to_capture_ok, inject_to_capture_med, 0), if(capture_to_encoded_ok, capture_to_encoded_med, 0),
                                       if(encoded_to_packetized_ok, encoded_to_packetized_med, 0), if(video_net_ok, video_net_med, 0),
                                       if(jb_delay_ok, jb_delay_med, 0), if(decode_ok, decode_med, 0),
                                       if(receive_to_present_ok, receive_to_present_med, 0)) / photon_med, NULL)) AS value,
         'any stage median > 40% of fleet median photon_ms_p50' AS threshold,
         toInt(photon_sessions) AS sessions,
         toInt(photon_days) AS span_days,
         multiIf(NOT photon_ok, 'INSUFFICIENT', value > 0.4, 'FIRES', every_ok, 'CLEAR', 'INSUFFICIENT') AS fires
  FROM (
    SELECT *,
           (photon_sessions >= 50 AND photon_days >= 14 AND photon_med > 0) AS photon_ok,
           (input_leg_ok AND replay_inject_ok AND inject_to_capture_ok AND capture_to_encoded_ok AND encoded_to_packetized_ok
             AND video_net_ok AND jb_delay_ok AND decode_ok AND receive_to_present_ok) AS every_ok
    FROM stage_stats
  )
)
