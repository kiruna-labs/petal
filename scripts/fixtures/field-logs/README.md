# Field-log fixtures

Synthetic `petal.log` excerpts for `scripts/test-analyze-field-log.mjs`. Every
line here is copied from the real emitting site (`camera_session.rs`,
`window_source.rs`, `logging.rs`'s startup markers) so a rename upstream breaks
the test rather than silently changing what the analyzer reads.

These carry no user data: identities appear only in their already-redacted
`<redacted:...>` form, exactly as `redact_for_export` would leave them.

`camera-preview-contended.log` carries the `settings: camera preview ...` lines
Settings started writing for #76 (`camera_session.rs`'s
`log_camera_preview_state`), so the analyzer can tell a contended episode from
one where the preview was not holding the device.
