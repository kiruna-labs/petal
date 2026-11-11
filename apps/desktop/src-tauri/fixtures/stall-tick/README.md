Real screen captures from nightly-loopback run 42835452916 (#498), used by
`test_cockpit` tests of `decode_pattern_tick`.

- `runner-occluded-freeze.png`: a SHARE-W2N-STALL freeze-phase region capture
  of the receiver panel. The always-on-top native test-pattern window covers
  its right side, so the top-right calibration square is gone. It must decode
  as uncalibrated (3/4 corners).
- `runner-pattern-frame-2376.png`: the same run's full-screen capture cropped
  to the native test-pattern window (960x600, drawn counter "frame 2376"). It
  must decode as 2376.
