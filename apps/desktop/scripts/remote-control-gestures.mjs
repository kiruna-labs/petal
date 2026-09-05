// Shared input-shape definitions for the focused cockpit smoke check and the
// TextEdit-oriented remote-control matrix. Keep assertions and target setup in
// their respective drivers; these values only describe the gesture payloads.

export const REMOTE_CONTROL_BUTTONS = Object.freeze({
  left: 0,
  middle: 1,
  right: 2,
});

export const REMOTE_CONTROL_COORDINATES = Object.freeze({
  cockpitCenter: Object.freeze({ x: 0.50, y: 0.50 }),
  cockpitDragFrom: Object.freeze({ x: 0.35, y: 0.50 }),
  cockpitDragTo: Object.freeze({ x: 0.65, y: 0.50 }),
  suiteClick: Object.freeze({ x: 0.20, y: 0.30 }),
  suiteDragFrom: Object.freeze({ x: 0.16, y: 0.28 }),
  suiteDragTo: Object.freeze({ x: 0.58, y: 0.28 }),
  suiteHeldInput: Object.freeze({ x: 0.50, y: 0.50 }),
});

export const REMOTE_CONTROL_DRAG_STEPS = Object.freeze({
  cockpit: 6,
  suite: 10,
  short: 4,
});

export const REMOTE_CONTROL_SHORTCUTS = Object.freeze({
  cmdA: Object.freeze({ key: 'a', code: 'KeyA', modifiers: Object.freeze({ meta: true }) }),
  cmdC: Object.freeze({ key: 'c', code: 'KeyC', modifiers: Object.freeze({ meta: true }) }),
  cmdV: Object.freeze({ key: 'v', code: 'KeyV', modifiers: Object.freeze({ meta: true }) }),
});

export const REMOTE_CONTROL_SCROLL_DELTAS = Object.freeze({
  cockpit: Object.freeze({ x: 0.50, y: 0.50, deltaY: 120, deltaMode: 0 }),
  pixel: Object.freeze({ x: 0.50, y: 0.50, deltaY: 720, deltaMode: 0 }),
  line: Object.freeze({ x: 0.50, y: 0.50, deltaY: 8, deltaMode: 1 }),
  horizontal: Object.freeze({ x: 0.50, y: 0.50, deltaX: 240, deltaY: 0, deltaMode: 0 }),
  // #811: aimed at the sentinel's horizontal scroll strip -- content rect
  // (60, 60, 840, 120) top-origin in the 960x600 sentinel, center (480, 120).
  // Keep in lockstep with `scrollStrip` in remote-control-photon-sentinel.swift.
  horizontalSentinel: Object.freeze({ x: 480 / 960, y: 120 / 600, deltaX: 240, deltaY: 0, deltaMode: 0 }),
});

// #446 acceptance geometry, expressed as fractions of the sentinel's shared
// content (960x600, top-origin -- the same origin the video tile and Petal's
// window-relative mapping use).
//   AX-hostile canvas : content rect (60, 440, 840, 140)
//   AppKit button     : content rect (560, 270, 300, 150)
// The two never overlap, so a hit in one can never be serviced by the other's
// route. Keep these in lockstep with `hostileRect`/`clickButton` in
// remote-control-photon-sentinel.swift.
export const REMOTE_CONTROL_ACCEPTANCE_446 = Object.freeze({
  hostileCenter: Object.freeze({ x: 480 / 960, y: 510 / 600 }),
  hostileDragFrom: Object.freeze({ x: 200 / 960, y: 510 / 600 }),
  hostileDragTo: Object.freeze({ x: 760 / 960, y: 510 / 600 }),
  axButtonCenter: Object.freeze({ x: 710 / 960, y: 345 / 600 }),
  dragSteps: 8,
  wheel: Object.freeze({ deltaY: 240, deltaMode: 0 }),
});
