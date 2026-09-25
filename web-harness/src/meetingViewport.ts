import type { HarnessContext } from './context';

// ---------------------------------------------------------------------------
// Meeting viewport (#239): the behaviour a phone needs beyond CSS.
//
//  - A full-screen toggle: on a phone the browser's URL bar is the largest
//    piece of chrome left, and full screen is the only way to drop it. A
//    button, never automatic (maintainer decision, 2026-09-25), and only where
//    `document.fullscreenEnabled` -- iPhone Safari gets no button at all.
//  - The landscape top bar of a touch screen fades after a few idle seconds
//    and comes back on any tap or key.
//  - The developer drawer: a parked sheet on phones, opened by `?dev=1` or
//    openDevTools().
//  - Until the control bar's ⋯ overflow (#247) lands, controls that do not
//    fit scroll, cut so the next one visibly peeks where that cuts no
//    control that fits, and a cut control hides its half-label.
//
// The landscape layout itself (icon rail, overlay top bar) is style.css's
// MEETING_RAIL_QUERY block; this module only toggles classes and sizes.
// ---------------------------------------------------------------------------

/** style.css's landscape-phone block. Keep the two in step. The aspect
 * clause is load-bearing: with interactive-widget=resizes-content a portrait
 * phone's on-screen keyboard leaves a short, "landscape" viewport (360x294)
 * that must not flip into the rail; a phone on its side is 1.6:1 or wider. */
export const MEETING_RAIL_QUERY = '(orientation: landscape) and (max-height: 500px) and (min-aspect-ratio: 3/2)';

/** How long the landscape top bar stays after the last interaction. */
export const MEETING_CHROME_IDLE_MS = 3000;

/** How far the first control that does not fit shows past a scroller's edge. */
export const CONTROL_PEEK_PX = 12;

/** `?dev=1` asks for the developer drawer; remembered for the tab. */
const DEV_TOOLS_SESSION_KEY = 'petal.devTools';

function fullscreenIconSvg(active: boolean, size: number): string {
  // Corner brackets pointing out (enter) or in (exit).
  const paths = active
    ? ['M8 3v3a2 2 0 0 1-2 2H3', 'M21 8h-3a2 2 0 0 1-2-2V3', 'M3 16h3a2 2 0 0 1 2 2v3', 'M16 21v-3a2 2 0 0 1 2-2h3']
    : ['M8 3H5a2 2 0 0 0-2 2v3', 'M21 8V5a2 2 0 0 0-2-2h-3', 'M3 16v3a2 2 0 0 0 2 2h3', 'M16 21h3a2 2 0 0 0 2-2v-3'];
  return [
    `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor"`,
    'stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">',
    ...paths.map((d) => `<path d="${d}"></path>`),
    '</svg>',
  ].join(' ');
}

/**
 * Builds the top-bar button and the rail cell -- or nothing at all where full
 * screen is unsupported.
 */
function installFullscreenToggle(
  meetingScreen: HTMLElement,
  topbarRight: HTMLElement,
  logEvent: HarnessContext['ui']['logEvent'],
  doc: Document = document
) {
  if (!doc.fullscreenEnabled) return;

  const topbarButton = doc.createElement('button');
  topbarButton.type = 'button';
  topbarButton.id = 'topbar-fullscreen';
  topbarButton.className = 'topbar-fullscreen';
  topbarRight.appendChild(topbarButton);
  const buttons: Array<[button: HTMLButtonElement, iconSize: number]> = [[topbarButton, 15]];

  // The rail cell sits between the scrolling controls and Leave, so it stays
  // pinned with Leave instead of scrolling away with the rest.
  const controlbar = meetingScreen.querySelector<HTMLElement>('.controlbar');
  if (controlbar) {
    const railButton = doc.createElement('button');
    railButton.type = 'button';
    railButton.id = 'ctl-fullscreen';
    railButton.className = 'control-button';
    const label = doc.createElement('span');
    label.className = 'meeting-control-label';
    label.textContent = 'Full screen';
    const cell = doc.createElement('div');
    cell.className = 'control-cell fullscreen-cell';
    cell.append(railButton, label);
    controlbar.insertBefore(cell, controlbar.querySelector('.leave-cell'));
    buttons.push([railButton, 20]);
  }

  function render() {
    const active = Boolean(doc.fullscreenElement);
    const label = active ? 'Exit full screen' : 'Enter full screen';
    for (const [button, iconSize] of buttons) {
      button.innerHTML = fullscreenIconSvg(active, iconSize);
      button.setAttribute('aria-label', label);
      button.setAttribute('aria-pressed', active ? 'true' : 'false');
      button.title = label;
    }
  }

  async function toggle() {
    try {
      if (doc.fullscreenElement) {
        await doc.exitFullscreen();
      } else {
        // 'hide': no browser navigation UI either (Android's system bars).
        await doc.documentElement.requestFullscreen({ navigationUI: 'hide' });
      }
    } catch (error) {
      logEvent(`full screen unavailable: ${error instanceof Error ? error.message : String(error)}`, 'warn');
    }
  }

  for (const [button] of buttons) button.addEventListener('click', () => void toggle());
  // The system can leave full screen on its own (back gesture, Esc): the
  // buttons follow the document, not the last click.
  doc.addEventListener('fullscreenchange', render);
  render();
}

/**
 * The landscape top bar's idle fade. `.chrome-idle` only has an effect inside
 * style.css's rail block, and the clock only runs while the rail query
 * matches on a screen without hover -- a phone. Portrait, desktop and a short
 * desktop window (which gets the rail, but has a mouse) never lose their bar.
 */
function installChromeIdle(
  meetingScreen: HTMLElement,
  tilesEl: HTMLElement,
  win: Window = window,
  doc: Document = document
) {
  const topbar = meetingScreen.querySelector<HTMLElement>('.topbar');
  const fadeQuery =
    typeof win.matchMedia === 'function' ? win.matchMedia(`${MEETING_RAIL_QUERY} and (hover: none)`) : null;
  if (!topbar || !fadeQuery) return;

  let timer: ReturnType<typeof setTimeout> | null = null;
  let revealTap = false;

  // Never fade out from under someone: focus in the top bar (renaming the
  // room), or the tap-to-play prompt connection.ts puts there (it would
  // otherwise vanish before anyone could tap it).
  function topbarInUse(): boolean {
    return topbar!.contains(doc.activeElement) || topbar!.querySelector('.audio-playback-prompt') !== null;
  }

  function schedule() {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    if (!fadeQuery!.matches || meetingScreen.classList.contains('hidden')) return;
    timer = setTimeout(() => {
      timer = null;
      if (topbarInUse()) schedule();
      else meetingScreen.classList.add('chrome-idle');
    }, MEETING_CHROME_IDLE_MS);
  }

  function wake() {
    meetingScreen.classList.remove('chrome-idle');
    schedule();
  }

  // connection.ts adds the tap-to-play prompt whenever autoplay is refused,
  // often after the bar has already faded: bring the bar back with it.
  new MutationObserver(() => {
    if (topbar.querySelector('.audio-playback-prompt')) wake();
  }).observe(topbar, { childList: true, subtree: true });

  meetingScreen.addEventListener(
    'pointerdown',
    () => {
      revealTap = meetingScreen.classList.contains('chrome-idle');
      wake();
    },
    true
  );
  meetingScreen.addEventListener('keydown', wake, true);
  meetingScreen.addEventListener('focusin', wake);
  // The tap that brings the top bar back does only that: it must not also
  // spotlight the tile under the finger (tileLayout.ts pins on click), or
  // reaching for the layout picker would itself change the layout.
  tilesEl.addEventListener(
    'click',
    (event) => {
      if (!revealTap) return;
      revealTap = false;
      const target = event.target as Element | null;
      if (target?.closest('button, a, input, label, summary, textarea, select')) return;
      event.stopPropagation();
    },
    true
  );
  meetingScreen.addEventListener('click', () => {
    revealTap = false;
  });

  fadeQuery.addEventListener?.('change', wake);
  // Joining shows the top bar for a full idle period before it fades, and
  // starts from the top of a page that can no longer scroll (style.css locks
  // it), in case a scroll position survived from the home screen.
  let visible = !meetingScreen.classList.contains('hidden');
  new MutationObserver(() => {
    const nowVisible = !meetingScreen.classList.contains('hidden');
    if (nowVisible && !visible) {
      win.scrollTo(0, 0);
      wake();
    }
    visible = nowVisible;
  }).observe(meetingScreen, { attributes: true, attributeFilter: ['class'] });
  wake();
}

/**
 * Opens the developer & test tools: the drawer on desktop, the sheet on a
 * phone. For the control bar's ⋯ menu (#247) and `?dev=1`.
 */
export function openDevTools(doc: Document = document) {
  const panel = doc.getElementById('dev-panel') as HTMLDetailsElement | null;
  if (!panel) return;
  panel.classList.add('is-open');
  panel.open = true;
}

function installDevTools(win: Window = window, doc: Document = document) {
  const panel = doc.getElementById('dev-panel') as HTMLDetailsElement | null;
  if (!panel) return;
  let requested = false;
  try {
    if (new URLSearchParams(win.location.search).get('dev') === '1') {
      win.sessionStorage.setItem(DEV_TOOLS_SESSION_KEY, '1');
    }
    requested = win.sessionStorage.getItem(DEV_TOOLS_SESSION_KEY) === '1';
  } catch {
    // Storage can be unavailable (privacy modes); the drawer just stays shut.
  }
  // Closing the drawer by its summary also parks the phone sheet again.
  panel.addEventListener('toggle', () => {
    if (!panel.open) panel.classList.remove('is-open');
  });
  if (requested) openDevTools(doc);
}

/**
 * Stopgap until the ⋯ overflow (#247): when the controls do not all fit, the
 * scrolling middle of the bar (portrait) or rail (landscape) is cut so the
 * first control that does not fit peeks by CONTROL_PEEK_PX -- an unmistakable
 * "there is more" -- instead of wherever the screen edge falls, where half a
 * button reads as a rendering glitch and none at all hides that it scrolls.
 * A control that fits is never cut for it: when the first one that does not
 * fit starts too near the edge to peek, the scroller keeps its size. Any
 * control the edge cuts hides its label (.is-clipped) until scrolled to.
 */
function installControlPeek(meetingScreen: HTMLElement, win: Window = window) {
  const controlbar = meetingScreen.querySelector<HTMLElement>('.controlbar');
  const scroller = meetingScreen.querySelector<HTMLElement>('.controls-left');
  if (!controlbar || !scroller || typeof ResizeObserver !== 'function') return;

  function fit() {
    scroller!.style.removeProperty('max-height');
    scroller!.style.removeProperty('max-width');
    const style = win.getComputedStyle(scroller!);
    const vertical = style.flexDirection === 'column';
    const box = scroller!.getBoundingClientRect();
    const size = vertical ? box.height : box.width;
    const scrolls =
      /auto|scroll/.test(vertical ? style.overflowY : style.overflowX) &&
      (vertical ? scroller!.scrollHeight : scroller!.scrollWidth) > size + 1;
    if (scrolls) {
      const scrolled = vertical ? scroller!.scrollTop : scroller!.scrollLeft;
      // Each control's span inside the scroller, in VISUAL order (CSS `order`
      // lifts Chat up in the scrolling layouts).
      const spans = Array.from(scroller!.children, (child) => {
        const rect = child.getBoundingClientRect();
        const start = (vertical ? rect.top - box.top : rect.left - box.left) + scrolled;
        return { start, end: start + (vertical ? rect.height : rect.width), shown: rect.width > 0 };
      })
        .filter((span) => span.shown)
        .sort((a, b) => a.start - b.start);
      // Only ever shorten into the first control that does not fit, and only
      // if that leaves it a full peek: a control that fits is never cut.
      const firstCut = spans.find((span) => span.end > size + 0.5);
      if (firstCut && firstCut.start + CONTROL_PEEK_PX <= size) {
        scroller!.style.setProperty(vertical ? 'max-height' : 'max-width', `${firstCut.start + CONTROL_PEEK_PX}px`);
      }
    }
    markClipped();
  }

  /** A clipped control's label would show as cut text ("Ir"): hide it until
   * the control scrolls fully into view. */
  function markClipped() {
    const box = scroller!.getBoundingClientRect();
    for (const child of Array.from(scroller!.children)) {
      const rect = child.getBoundingClientRect();
      const clipped =
        rect.width > 0 &&
        (rect.left < box.left - 0.5 || rect.right > box.right + 0.5 || rect.top < box.top - 0.5 || rect.bottom > box.bottom + 0.5);
      child.classList.toggle('is-clipped', clipped);
    }
  }

  // The bar's own size follows the viewport; the scroller's contents change
  // when plugins add or remove toolbar cells; scrolling moves the clip.
  new ResizeObserver(fit).observe(controlbar);
  new MutationObserver(fit).observe(scroller, { childList: true });
  scroller.addEventListener('scroll', () => win.requestAnimationFrame(markClipped), { passive: true });
}

export function setupMeetingViewport(ctx: HarnessContext) {
  const { dom, ui } = ctx;
  installFullscreenToggle(dom.meetingScreen, dom.topbarRight, ui.logEvent);
  installChromeIdle(dom.meetingScreen, dom.tilesEl);
  installDevTools();
  installControlPeek(dom.meetingScreen);
}
