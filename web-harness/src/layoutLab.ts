// Layout lab: a dev-only visual probe for shared/logic/galleryGeometry.ts.
// Renders two independently skinned panes -- Web (this client's own tile
// look) and Native (the desktop gallery's look) -- driven by ONE shared set
// of controls and the SAME packing function, so a shape difference between
// them can only come from their different padding/gap skin, never from two
// copies of the algorithm drifting apart (#P0). No DOM/algorithm logic is
// duplicated here: every column/row/fill/tile-size number comes straight out
// of `computeGalleryLayout`.
import { computeGalleryLayout, type GalleryArrangement, type GalleryGeometry } from '@petal/shared/logic/galleryGeometry';
import { CAMERA_TILE_ASPECT_RANGE } from '@petal/shared/logic/cameraCrop';
import { getTileReflowController } from './tileReflow.ts';

const ASPECT_RATIOS: Record<string, number> = {
  '16:9': 16 / 9,
  '4:3': 4 / 3,
  '1:1': 1
};

const SIZE_PRESETS: Array<{ label: string; width: number; height: number }> = [
  { label: '840x560', width: 840, height: 560 },
  { label: '1100x1750', width: 1100, height: 1750 },
  { label: '380x360', width: 380, height: 360 },
  { label: '1600x900', width: 1600, height: 900 }
];

interface PaneSkin {
  readonly key: 'web' | 'native';
  readonly label: string;
  readonly pad: number;
  readonly tileClass: string;
}

const PANES: PaneSkin[] = [
  { key: 'web', label: 'Web', pad: 20, tileClass: 'tile-web' },
  { key: 'native', label: 'Native', pad: 28, tileClass: 'tile-native' }
];

/** The content box available to the packer once a pane's own fixed padding
 * is removed from its (shared) outer frame size -- mirrors how Gallery.svelte
 * and the web tile grid both measure their surface (clientWidth minus
 * padding) before calling the packer. */
export function paneContentSize(frameWidth: number, frameHeight: number, pad: number) {
  return {
    width: Math.max(0, frameWidth - pad * 2),
    height: Math.max(0, frameHeight - pad * 2)
  };
}

export interface LabControls {
  count: number;
  frameWidth: number;
  frameHeight: number;
  arrangement: GalleryArrangement;
  gap: number;
  tileAspect: number;
  /** #248: pack with the camera crop range (~7:6 up to 16:9) instead of the
   * fixed tile aspect -- what a camera-only web grid and the desktop gallery
   * do -- so the lab can reproduce them. */
  cameraCrop?: boolean;
}

/** The one call every pane makes into the shared packer -- a pane's skin
 * (its fixed padding) is the only thing that can make its result differ from
 * another pane's. */
export function computePaneLayout(pane: Pick<PaneSkin, 'pad'>, controls: LabControls): GalleryGeometry {
  const content = paneContentSize(controls.frameWidth, controls.frameHeight, pane.pad);
  return computeGalleryLayout(controls.count, content.width, content.height, {
    gap: controls.gap,
    tileAspect: controls.tileAspect,
    tileAspectRange: controls.cameraCrop ? CAMERA_TILE_ASPECT_RANGE : null,
    arrangement: controls.arrangement
  });
}

export function formatReadout(layout: GalleryGeometry): string {
  const fillPct = Math.round(layout.fill * 100);
  const tileW = Math.round(layout.tileWidth);
  const tileH = Math.round(layout.tileHeight);
  const overflow = layout.overflow ? ' · overflow' : '';
  return `${layout.columns}x${layout.rows} · ${fillPct}% fill · ${tileW}x${tileH}px tile · gap ${layout.gap}px${overflow}`;
}

interface PaneDom {
  skin: PaneSkin;
  frameEl: HTMLDivElement;
  tilesEl: HTMLDivElement;
  readoutEl: HTMLSpanElement;
  tileEls: HTMLDivElement[];
}

function buildShell(root: HTMLElement) {
  root.innerHTML = `
    <main class="lab-shell">
      <header>
        <h1>Gallery layout lab</h1>
        <p>
          One shared packing function (shared/logic/galleryGeometry.ts), rendered through two
          different skins. A shape mismatch here means the skins disagree about available space,
          never that the algorithm itself forked.
        </p>
      </header>
      <section class="controls">
        <label>Count
          <input type="range" id="count" min="1" max="16" step="1" />
          <span class="control-value" id="count-val"></span>
        </label>
        <div class="add-remove">
          <button type="button" id="remove-tile">− tile</button>
          <button type="button" id="add-tile">+ tile</button>
        </div>
        <label>Width
          <input type="range" id="width" min="200" max="2000" step="10" />
          <span class="control-value" id="width-val"></span>
        </label>
        <label>Height
          <input type="range" id="height" min="200" max="1800" step="10" />
          <span class="control-value" id="height-val"></span>
        </label>
        <div class="presets" id="presets"></div>
        <label>Arrangement
          <select id="arrangement">
            <option value="auto">auto</option>
            <option value="column">column</option>
            <option value="row">row</option>
          </select>
        </label>
        <label>Base gap
          <input type="range" id="gap" min="4" max="40" step="1" />
          <span class="control-value" id="gap-val"></span>
        </label>
        <label>Tile aspect
          <select id="aspect">
            <option value="16:9">16:9</option>
            <option value="4:3">4:3</option>
            <option value="1:1">1:1</option>
          </select>
        </label>
        <label>
          <input type="checkbox" id="camera-crop" />
          Camera crop range (7:6–16:9)
        </label>
      </section>
      <section class="panes" id="panes"></section>
    </main>
  `;
}

function buildPaneDom(panesEl: HTMLElement, skin: PaneSkin): PaneDom {
  const pane = document.createElement('div');
  pane.className = 'pane';
  pane.id = `pane-${skin.key}`;

  const heading = document.createElement('h2');
  heading.textContent = skin.label;
  const readoutEl = document.createElement('span');
  readoutEl.className = 'readout';
  heading.appendChild(readoutEl);

  const frameEl = document.createElement('div');
  frameEl.className = `frame frame-${skin.key}`;

  const tilesEl = document.createElement('div');
  tilesEl.className = `tiles tiles-${skin.key}`;
  frameEl.appendChild(tilesEl);

  pane.appendChild(heading);
  pane.appendChild(frameEl);
  panesEl.appendChild(pane);

  return { skin, frameEl, tilesEl, readoutEl, tileEls: [] };
}

/** Keeps `.tile` elements ALIVE across a count change (only appends/removes
 * the delta at the end) rather than re-rendering innerHTML -- tileReflow's
 * FLIP animation matches tiles by the same HTMLElement reference, so
 * recreating every tile on every render would silently defeat it. */
function syncTileCount(pane: PaneDom, count: number) {
  while (pane.tileEls.length < count) {
    const tile = document.createElement('div');
    tile.className = `tile ${pane.skin.tileClass}`;
    tile.textContent = pane.skin.key === 'native' ? `P${pane.tileEls.length + 1}` : String(pane.tileEls.length + 1);
    pane.tilesEl.appendChild(tile);
    pane.tileEls.push(tile);
  }
  while (pane.tileEls.length > count) {
    const tile = pane.tileEls.pop();
    tile?.remove();
  }
}

function applyPaneLayout(pane: PaneDom, layout: GalleryGeometry) {
  pane.frameEl.style.padding = `${pane.skin.pad}px`;
  pane.tilesEl.style.gridTemplateColumns = `repeat(${layout.columns}, ${layout.tileWidth}px)`;
  pane.tilesEl.style.gridAutoRows = `${layout.tileHeight}px`;
  // The packer's OWN gap (tightened for compact/tiny cells), not the base
  // gap the user set -- rendering the base gap here would visually disagree
  // with the cell sizes computeGalleryLayout already assumed.
  pane.tilesEl.style.gap = `${layout.gap}px`;
  pane.readoutEl.textContent = formatReadout(layout);
  pane.readoutEl.classList.toggle('is-overflow', layout.overflow);
}

export function mountLayoutLab(root: HTMLElement) {
  buildShell(root);

  const countInput = root.querySelector<HTMLInputElement>('#count')!;
  const countVal = root.querySelector<HTMLSpanElement>('#count-val')!;
  const addTileBtn = root.querySelector<HTMLButtonElement>('#add-tile')!;
  const removeTileBtn = root.querySelector<HTMLButtonElement>('#remove-tile')!;
  const widthInput = root.querySelector<HTMLInputElement>('#width')!;
  const widthVal = root.querySelector<HTMLSpanElement>('#width-val')!;
  const heightInput = root.querySelector<HTMLInputElement>('#height')!;
  const heightVal = root.querySelector<HTMLSpanElement>('#height-val')!;
  const presetsEl = root.querySelector<HTMLDivElement>('#presets')!;
  const arrangementSelect = root.querySelector<HTMLSelectElement>('#arrangement')!;
  const gapInput = root.querySelector<HTMLInputElement>('#gap')!;
  const gapVal = root.querySelector<HTMLSpanElement>('#gap-val')!;
  const aspectSelect = root.querySelector<HTMLSelectElement>('#aspect')!;
  const cameraCropInput = root.querySelector<HTMLInputElement>('#camera-crop')!;
  const panesEl = root.querySelector<HTMLElement>('#panes')!;

  const panes = PANES.map((skin) => buildPaneDom(panesEl, skin));
  const reflowControllers = panes.map((pane) => getTileReflowController(pane.tilesEl));

  const controls: LabControls = {
    count: 5,
    frameWidth: 840,
    frameHeight: 560,
    arrangement: 'auto',
    gap: 18,
    tileAspect: ASPECT_RATIOS['16:9'],
    cameraCrop: false
  };

  countInput.value = String(controls.count);
  widthInput.value = String(controls.frameWidth);
  heightInput.value = String(controls.frameHeight);
  gapInput.value = String(controls.gap);

  for (const preset of SIZE_PRESETS) {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = preset.label;
    button.addEventListener('click', () => {
      controls.frameWidth = preset.width;
      controls.frameHeight = preset.height;
      widthInput.value = String(preset.width);
      heightInput.value = String(preset.height);
      render();
    });
    presetsEl.appendChild(button);
  }

  function render() {
    countVal.textContent = String(controls.count);
    widthVal.textContent = `${controls.frameWidth}px`;
    heightVal.textContent = `${controls.frameHeight}px`;
    gapVal.textContent = `${controls.gap}px`;

    for (let i = 0; i < panes.length; i += 1) {
      const pane = panes[i];
      pane.frameEl.style.width = `${controls.frameWidth}px`;
      pane.frameEl.style.height = `${controls.frameHeight}px`;
      const layout = computePaneLayout(pane.skin, controls);
      reflowControllers[i].withAnimation(() => {
        syncTileCount(pane, controls.count);
        applyPaneLayout(pane, layout);
      });
    }
  }

  countInput.addEventListener('input', () => {
    controls.count = Number(countInput.value);
    render();
  });
  addTileBtn.addEventListener('click', () => {
    controls.count = Math.min(16, controls.count + 1);
    countInput.value = String(controls.count);
    render();
  });
  removeTileBtn.addEventListener('click', () => {
    controls.count = Math.max(1, controls.count - 1);
    countInput.value = String(controls.count);
    render();
  });
  widthInput.addEventListener('input', () => {
    controls.frameWidth = Number(widthInput.value);
    render();
  });
  heightInput.addEventListener('input', () => {
    controls.frameHeight = Number(heightInput.value);
    render();
  });
  arrangementSelect.addEventListener('change', () => {
    controls.arrangement = arrangementSelect.value as GalleryArrangement;
    render();
  });
  gapInput.addEventListener('input', () => {
    controls.gap = Number(gapInput.value);
    render();
  });
  aspectSelect.addEventListener('change', () => {
    controls.tileAspect = ASPECT_RATIOS[aspectSelect.value] ?? ASPECT_RATIOS['16:9'];
    render();
  });
  cameraCropInput.addEventListener('change', () => {
    controls.cameraCrop = cameraCropInput.checked;
    aspectSelect.disabled = cameraCropInput.checked;
    render();
  });

  render();
}

// Guarded so this module can be imported under `node --test` (no DOM) purely
// for its pure helpers (`computePaneLayout`, `paneContentSize`) without
// trying to mount into a document that doesn't exist there.
if (typeof document !== 'undefined') {
  const root = document.querySelector<HTMLElement>('#lab-app');
  if (root) mountLayoutLab(root);
}
