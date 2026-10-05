// Native gallery layout lab: the scenario model and synthetic participants
// shared by the interactive /dev/gallery-lab route and the Chromium matrix
// (scripts/verify-native-gallery-matrix.mjs, tests/galleryLabRendered.test.ts).
// Every scenario renders the REAL meeting composition (MeetingChrome ->
// Gallery -> ParticipantTile) at a window size, so a layout problem seen here
// is the shipped layout, not a replica of it. The web client's equivalent is
// scripts/verify-meeting-layout-matrix.mjs (#239).
//
// Pure apart from `syntheticCameraStream` (canvas + captureStream), so the
// scenario maths can be unit-tested under node.

import type { GalleryParticipant } from '$lib/components/Gallery.svelte';

export type LabLayoutMode = 'grid' | 'spotlight';

/** Camera frame shapes a participant can send: a laptop webcam (16:9), an
 * older/USB webcam (4:3), a phone held upright (9:16). 'mixed' cycles all. */
export type LabCameraAspect = '16:9' | '4:3' | '9:16' | 'mixed';

export interface GalleryLabScenario {
  /** The meeting window's inner size, logical px. */
  width: number;
  height: number;
  /** Everyone in the meeting, you included (you are always first). */
  count: number;
  /** How many of the OTHER participants have their camera off. */
  camerasOff: number;
  aspect: LabCameraAspect;
  mode: LabLayoutMode;
  chatOpen: boolean;
  /** Plugin toolbar buttons in the control bar (the built-in Reactions is one). */
  plugins: number;
  /** A connection/state card above the tiles ("Reconnecting…"). */
  stateCard: boolean;
  longNames: boolean;
  /** Someone is sharing a window (their tile gets the sharing ring). */
  sharing: boolean;
}

export const DEFAULT_LAB_SCENARIO: GalleryLabScenario = {
  width: 840,
  height: 560,
  count: 4,
  camerasOff: 0,
  aspect: '16:9',
  mode: 'grid',
  chatOpen: false,
  plugins: 1,
  stateCard: false,
  longNames: false,
  sharing: false
};

/** Window shapes worth looking at, named for what a person is doing with the
 * window. The narrow and short ones are the reason this lab exists: a column
 * of faces beside an editor, a row of faces along the top of the screen. */
export const LAB_WINDOW_PRESETS: ReadonlyArray<{ key: string; label: string; width: number; height: number }> = [
  { key: 'default', label: 'Default 840×560', width: 840, height: 560 },
  { key: 'laptop', label: 'Laptop 1280×800', width: 1280, height: 800 },
  { key: 'full-hd', label: 'Full screen 1920×1080', width: 1920, height: 1080 },
  { key: 'column', label: 'Side column 300×900', width: 300, height: 900 },
  { key: 'column-min', label: 'Narrowest 240×700', width: 240, height: 700 },
  { key: 'column-short', label: 'Short column 300×450', width: 300, height: 450 },
  { key: 'column-wide', label: 'Wide column 420×1000', width: 420, height: 1000 },
  { key: 'tall', label: 'Tall 600×1000', width: 600, height: 1000 },
  { key: 'bar', label: 'Top bar 1400×220', width: 1400, height: 220 },
  { key: 'bar-short', label: 'Thin bar 1100×170', width: 1100, height: 170 },
  { key: 'bar-min', label: 'Shortest 900×160', width: 900, height: 160 },
  { key: 'short', label: 'Short 900×340', width: 900, height: 340 },
  { key: 'small', label: 'Small 520×380', width: 520, height: 380 }
];

const FIRST_NAMES = ['You', 'Ada', 'Grace', 'Alan', 'Katherine', 'Linus', 'Barbara', 'Edsger', 'Frances', 'Ken', 'Radia', 'Donald', 'Margaret', 'Dennis', 'Hedy', 'Tim'];
const LONG_NAMES = ['You', 'Ada Lovelace-Byron', 'Grace Brewster Hopper', 'Alan Mathison Turing', 'Katherine Coleman Johnson', 'Linus Benedict Torvalds', 'Barbara Liskov-Huberman', 'Edsger Wybe Dijkstra', 'Frances Elizabeth Allen', 'Kenneth Lane Thompson', 'Radia Joy Perlman', 'Donald Ervin Knuth', 'Margaret Heafield Hamilton', 'Dennis MacAlistair Ritchie', 'Hedy Lamarr Markey', 'Tim Berners-Lee'];
/** Distinct, muted backdrops so tiles are told apart in a screenshot. */
const BACKDROPS = ['#2d3b4f', '#4a3b2c', '#2f4a3a', '#4a2f45', '#3b3f52', '#52432f', '#2f4d52', '#4d3434'];
const ASPECTS: Record<Exclude<LabCameraAspect, 'mixed'>, number> = { '16:9': 16 / 9, '4:3': 4 / 3, '9:16': 9 / 16 };
const MIXED: Array<Exclude<LabCameraAspect, 'mixed'>> = ['16:9', '4:3', '16:9', '9:16'];

export function clampScenario(input: Partial<GalleryLabScenario>): GalleryLabScenario {
  const merged = { ...DEFAULT_LAB_SCENARIO, ...input };
  const count = Math.max(1, Math.min(16, Math.round(merged.count)));
  return {
    ...merged,
    width: Math.max(120, Math.round(merged.width)),
    height: Math.max(120, Math.round(merged.height)),
    count,
    camerasOff: Math.max(0, Math.min(count - 1, Math.round(merged.camerasOff))),
    plugins: Math.max(0, Math.min(3, Math.round(merged.plugins)))
  };
}

/** The camera shape participant `index` sends under `aspect`. */
export function labCameraAspect(aspect: LabCameraAspect, index: number): number {
  return ASPECTS[aspect === 'mixed' ? MIXED[index % MIXED.length] : aspect];
}

export interface LabParticipantSpec {
  id: string;
  name: string;
  isLocal: boolean;
  videoOn: boolean;
  /** width / height of the frames this participant's camera sends. */
  cameraAspect: number;
  backdrop: string;
  speaking: boolean;
  muted: boolean;
  sharing: boolean;
}

/** Who is in the meeting under `scenario`, without any media yet. Cameras
 * go off from the END of the list, so you and the first few keep video. */
export function labParticipantSpecs(scenario: GalleryLabScenario): LabParticipantSpec[] {
  const names = scenario.longNames ? LONG_NAMES : FIRST_NAMES;
  return Array.from({ length: scenario.count }, (_, index) => {
    const base = names[index % names.length];
    const name = index >= names.length ? `${base} ${Math.floor(index / names.length) + 1}` : base;
    return {
      id: `lab-${index}`,
      name: index === 0 ? 'You' : name,
      isLocal: index === 0,
      videoOn: index === 0 || index < scenario.count - scenario.camerasOff,
      cameraAspect: labCameraAspect(scenario.aspect, index),
      backdrop: BACKDROPS[index % BACKDROPS.length],
      speaking: index === 1,
      muted: index % 3 === 2,
      sharing: scenario.sharing && index === 1
    };
  });
}

/** A camera-shaped test picture: backdrop, head and shoulders, the name, and
 * a frame inset by 10% so a crop past the caps shows as a missing edge. */
export function syntheticCameraStream(spec: Pick<LabParticipantSpec, 'name' | 'cameraAspect' | 'backdrop'>, fps = 15): MediaStream {
  const height = 360;
  const width = Math.round(height * spec.cameraAspect);
  const canvas = document.createElement('canvas');
  canvas.width = width;
  canvas.height = height;
  const ctx = canvas.getContext('2d')!;
  let frame = 0;
  const draw = () => {
    frame += 1;
    ctx.fillStyle = spec.backdrop;
    ctx.fillRect(0, 0, width, height);
    // The 10% safe frame: what the crop caps promise never to cut.
    ctx.strokeStyle = 'rgba(255,255,255,0.35)';
    ctx.lineWidth = 3;
    ctx.strokeRect(width * 0.1, height * 0.1, width * 0.8, height * 0.8);
    const cx = width / 2 + Math.sin(frame / 20) * width * 0.02;
    ctx.fillStyle = 'rgba(236, 214, 190, 0.92)';
    ctx.beginPath();
    ctx.arc(cx, height * 0.42, height * 0.16, 0, Math.PI * 2);
    ctx.fill();
    ctx.fillStyle = 'rgba(30, 30, 36, 0.85)';
    ctx.beginPath();
    ctx.ellipse(cx, height * 0.98, Math.min(width * 0.38, height * 0.42), height * 0.32, 0, Math.PI, 0);
    ctx.fill();
    // An off-centre marker: proves which way a self-view is mirrored.
    ctx.fillStyle = '#e5484d';
    ctx.fillRect(width * 0.12, height * 0.12, height * 0.08, height * 0.08);
  };
  draw();
  const timer = setInterval(draw, 1000 / fps);
  const stream = canvas.captureStream(fps);
  const [track] = stream.getVideoTracks();
  track?.addEventListener('ended', () => clearInterval(timer));
  return stream;
}

/** Stop every track a lab participant list owns. */
export function stopLabStreams(participants: readonly GalleryParticipant[]): void {
  for (const participant of participants) participant.videoStream?.getTracks().forEach((track) => track.stop());
}

/** The gallery participants the meeting route would pass for `specs`. */
export function labGalleryParticipants(
  specs: readonly LabParticipantSpec[],
  streamFor: (spec: LabParticipantSpec) => MediaStream | undefined = (spec) => syntheticCameraStream(spec)
): GalleryParticipant[] {
  return specs.map((spec) => {
    const stream = spec.videoOn ? streamFor(spec) : undefined;
    return {
      id: spec.id,
      name: spec.isLocal ? 'You (you)' : spec.name,
      videoOn: !!stream,
      videoStream: stream,
      mirrored: spec.isLocal,
      speaking: spec.speaking,
      activeSpeaker: spec.speaking,
      muted: spec.muted,
      isLocal: spec.isLocal,
      sharing: spec.sharing,
      shareCount: spec.sharing ? 1 : 0,
      sharingLiveBackground: spec.sharing ? '#7aa2ff' : undefined,
      sharingLiveColor: spec.sharing ? '#0b1020' : undefined
    };
  });
}

/** `?w=300&h=900&n=5&off=1&aspect=mixed&mode=spotlight&chat=1&plugins=1&state=1&long=1&share=1`
 * <-> scenario, so a matrix cell is a URL anyone can open. */
export function scenarioFromQuery(query: string): GalleryLabScenario {
  const params = new URLSearchParams(query.replace(/^[?#]/, ''));
  const num = (key: string, fallback: number) => {
    const value = Number(params.get(key));
    return params.has(key) && Number.isFinite(value) ? value : fallback;
  };
  const flag = (key: string, fallback: boolean) => (params.has(key) ? params.get(key) === '1' : fallback);
  const aspect = params.get('aspect');
  const mode = params.get('mode');
  return clampScenario({
    width: num('w', DEFAULT_LAB_SCENARIO.width),
    height: num('h', DEFAULT_LAB_SCENARIO.height),
    count: num('n', DEFAULT_LAB_SCENARIO.count),
    camerasOff: num('off', DEFAULT_LAB_SCENARIO.camerasOff),
    aspect: aspect === '16:9' || aspect === '4:3' || aspect === '9:16' || aspect === 'mixed' ? aspect : DEFAULT_LAB_SCENARIO.aspect,
    mode: mode === 'spotlight' ? 'spotlight' : 'grid',
    chatOpen: flag('chat', DEFAULT_LAB_SCENARIO.chatOpen),
    plugins: num('plugins', DEFAULT_LAB_SCENARIO.plugins),
    stateCard: flag('state', DEFAULT_LAB_SCENARIO.stateCard),
    longNames: flag('long', DEFAULT_LAB_SCENARIO.longNames),
    sharing: flag('share', DEFAULT_LAB_SCENARIO.sharing)
  });
}

export function scenarioToQuery(scenario: GalleryLabScenario): string {
  const params = new URLSearchParams({
    w: String(scenario.width),
    h: String(scenario.height),
    n: String(scenario.count),
    off: String(scenario.camerasOff),
    aspect: scenario.aspect,
    mode: scenario.mode,
    chat: scenario.chatOpen ? '1' : '0',
    plugins: String(scenario.plugins),
    state: scenario.stateCard ? '1' : '0',
    long: scenario.longNames ? '1' : '0',
    share: scenario.sharing ? '1' : '0'
  });
  return params.toString();
}
