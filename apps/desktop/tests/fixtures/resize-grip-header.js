import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';

// Minimal harness reproducing the two-layer stacking that a real remote
// window's compositor surface renders (apps/desktop/src/routes/compositor/
// surface/+page.svelte): RemoteWindowHeader.svelte's real `.header` and the
// page's own `.resize-zones` overlay (28x28 corner grips, z-index:3) as
// SIBLINGS under a shared, non-stacking-context `.drag-handle` wrapper --
// exactly the DOM relationship the real page uses. Refs #674.
const HARNESS_CSS = `
  html, body { margin: 0; padding: 0; background: #1a1a1a; }
  .remote-window-chrome { width: 100vw; height: 200px; overflow: hidden; box-sizing: border-box; }
  .drag-handle { position: relative; width: 100%; box-sizing: border-box; }
  .resize-zones { position: absolute; inset: 0; z-index: 3; pointer-events: none; }
  .resize-zone { position: absolute; border: 0; padding: 0; margin: 0; background: transparent; pointer-events: auto; }
  .resize-nw, .resize-ne { top: 0; width: 28px; height: 28px; }
  .resize-nw { left: 0; cursor: nwse-resize; }
  .resize-ne { right: 0; cursor: nesw-resize; }
`;

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()));
}

function rectOf(element) {
  if (!element) return null;
  const bounds = element.getBoundingClientRect();
  return { left: bounds.left, top: bounds.top, right: bounds.right, bottom: bounds.bottom, width: bounds.width, height: bounds.height };
}

function centerOf(element) {
  const bounds = element.getBoundingClientRect();
  return { x: bounds.left + bounds.width / 2, y: bounds.top + bounds.height / 2 };
}

function hitDescribe(element) {
  if (!element) return null;
  const classes = [...element.classList].join('.');
  return `${element.tagName.toLowerCase()}${classes ? `.${classes}` : ''}`;
}

async function renderFixture() {
  try {
    const style = document.createElement('style');
    style.textContent = HARNESS_CSS;
    document.head.appendChild(style);

    const [{ mount }, { default: RemoteWindowHeader }] = await Promise.all([
      import('svelte'),
      import('$lib/components/RemoteWindowHeader.svelte')
    ]);

    const chrome = document.createElement('div');
    chrome.className = 'remote-window-chrome';
    const dragHandle = document.createElement('div');
    dragHandle.className = 'drag-handle';
    chrome.appendChild(dragHandle);
    document.querySelector('#app').appendChild(chrome);

    mount(RemoteWindowHeader, {
      target: dragHandle,
      props: {
        ownerName: 'Fixture Owner',
        identity: 'blue',
        sourceTitle: 'Fixture Source — App',
        autoHide: false,
        onHideWindow: () => {},
        onFitToSource: () => {},
        onOpenModeMenu: () => {}
      }
    });

    const resizeZones = document.createElement('div');
    resizeZones.className = 'resize-zones';
    resizeZones.innerHTML = `
      <button type="button" tabindex="-1" aria-label="Resize north west" class="resize-zone resize-nw"></button>
      <button type="button" tabindex="-1" aria-label="Resize north east" class="resize-zone resize-ne"></button>
    `;
    dragHandle.appendChild(resizeZones);

    await document.fonts.ready;
    await nextFrame();
    await nextFrame();

    const trafficHide = dragHandle.querySelector('.traffic-hide');
    const trafficFit = dragHandle.querySelector('.traffic-fit');
    const overflowBtn = dragHandle.querySelector('.overflow-btn');
    const winMin = dragHandle.querySelector('.win-min');
    const resizeNw = dragHandle.querySelector('.resize-nw');
    const resizeNe = dragHandle.querySelector('.resize-ne');

    // trafficHide/trafficFit and winMin are mutually exclusive branches
    // ({#if isWindows()}) -- only the resize zones are always required.
    if (!resizeNw || !resizeNe) {
      throw new Error('resize-grip fixture did not render the expected resize zones');
    }
    if (!trafficHide && !winMin) {
      throw new Error('resize-grip fixture rendered neither the traffic dots nor the win-ctl buttons');
    }

    function probe(element) {
      if (!element) return null;
      const { x, y } = centerOf(element);
      const hit = document.elementFromPoint(x, y);
      return {
        point: { x, y },
        rect: rectOf(element),
        hitSelector: hitDescribe(hit),
        hitIsSelf: hit === element,
        hitIsResizeNw: hit === resizeNw,
        hitIsResizeNe: hit === resizeNe
      };
    }

    const measurement = {
      viewport: { width: window.innerWidth },
      trafficHide: probe(trafficHide),
      trafficFit: probe(trafficFit),
      overflowBtn: probe(overflowBtn),
      winMin: probe(winMin),
      resizeNwRect: rectOf(resizeNw),
      resizeNeRect: rectOf(resizeNe)
    };

    document.body.dataset.resizeGripMeasurement = encodeURIComponent(JSON.stringify(measurement));
  } catch (error) {
    const message = error instanceof Error ? `${error.message}\n${error.stack ?? ''}` : String(error);
    document.body.dataset.resizeGripMeasurementError = encodeURIComponent(message);
  }
}

void renderFixture();
