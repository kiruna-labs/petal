import { installDismissibleLayer, type DismissibleLayerCleanup } from '@petal/shared/ui/dismissibleLayer';

// ---------------------------------------------------------------------------
// Control-bar overflow (#247). When the meeting controls do not fit the bar,
// everything except Mic, Camera and Leave moves behind a ⋯ button, lowest
// priority first, and its menu carries each hidden control's state.
//
// Measured, not by breakpoint: the bar's length along its main axis (width in
// the bottom bar, height when the bar is laid out as a vertical rail) against
// the natural size of every cell. So a plugin adding a button, a label
// changing, or a phone rotating all re-fit, and a window wide enough for
// everything never shows ⋯ at all.
//
// A hidden control stays in the DOM with its listeners and state; its menu
// row just clicks the real button, so each control keeps one code path. Same
// menu register (shared/ui/meeting-controls.css) and keyboard behaviour as
// the desktop's More menus (Gallery.svelte, MeetingChrome.svelte).
// ---------------------------------------------------------------------------

/** Controls that never leave the bar. */
const PINNED_CONTROLS: ReadonlySet<string> = new Set(['ctl-audio', 'ctl-video', 'ctl-leave']);

/**
 * The order the other controls give way in, lowest priority first. A control
 * not listed here (a plugin's, or any cell added later) goes before all of
 * them; among those, the one furthest along the bar goes first. Full screen
 * is the landscape rail's (#239) and goes last: it is the only way to hide a
 * phone browser's address bar, and in full screen the rail has room for the
 * controls that gave way (a Pixel 8 in Chrome is about 360px tall in
 * landscape, 412px in full screen).
 */
export const COLLAPSE_ORDER: readonly string[] = ['ctl-draw', 'ctl-invite', 'ctl-chat', 'ctl-share', 'ctl-fullscreen'];

/** A badge showing on a hidden control is what puts the dot on ⋯. */
const ATTENTION_BADGE_SELECTOR = '.chat-badge, .plugin-control-badge';

/**
 * Attribute changes answered in the same microtask: `hidden` changes what is
 * in the bar, `disabled` what a row may do. The rest (labels, pressed state,
 * classes) only change what the menu and the dot say, so they wait a frame.
 */
const IMMEDIATE_ATTRIBUTES: ReadonlySet<string> = new Set(['hidden', 'disabled']);

const MENU_GAP = 8; // between the bar and the menu, as the device menus
const VIEWPORT_PAD = 8;
/** Sub-pixel slack, so a bar that fits exactly is not rounded into overflow. */
const FIT_EPSILON = 0.5;

/** One flex line of the bar, measured along its main axis. */
export interface BarLine<Cell> {
  gap: number;
  /** Padding plus border at both ends of the line's own box. */
  inset: number;
  /** Cells, and nested lines (the bar's `.controls-left`). */
  items: Array<{ cell: Cell; size: number } | BarLine<Cell>>;
}

/** The line's length when only the cells `shown` accepts are laid out. */
export function barLength<Cell>(line: BarLine<Cell>, shown: (cell: Cell) => boolean): number {
  let length = line.inset;
  let count = 0;
  for (const item of line.items) {
    if ('cell' in item) {
      if (!shown(item.cell)) continue;
      length += item.size;
    } else {
      // A nested line stays a flex item even when all of its cells are gone.
      length += barLength(item, shown);
    }
    count += 1;
  }
  return length + line.gap * Math.max(0, count - 1);
}

/**
 * Which candidates to move into the menu so the bar fits `available`.
 * Candidates come lowest priority first; `more` (the ⋯ cell) takes its room
 * once something has gone, or from the start when `moreAnyway` (the menu has
 * items of its own). Returns an empty set when everything fits.
 */
export function planOverflow<Cell>(
  line: BarLine<Cell>,
  available: number,
  candidates: readonly Cell[],
  more: Cell,
  moreAnyway = false
): Set<Cell> {
  const collapsed = new Set<Cell>();
  const fits = () =>
    barLength(line, (cell) => (cell === more ? moreAnyway || collapsed.size > 0 : !collapsed.has(cell))) <=
    available + FIT_EPSILON;
  for (const cell of candidates) {
    if (fits()) break;
    collapsed.add(cell);
  }
  return collapsed;
}

/**
 * The cells that may collapse, in the order they give way. `cells` is in the
 * order they appear along the bar, each with the id of its control ('' for a
 * plugin's, which has none).
 */
export function collapseCandidates<Cell>(cells: ReadonlyArray<{ cell: Cell; controlId: string }>): Cell[] {
  const rank = (controlId: string) => COLLAPSE_ORDER.indexOf(controlId);
  return cells
    .map((entry, position) => ({ ...entry, position }))
    .filter(({ controlId }) => !PINNED_CONTROLS.has(controlId))
    .sort((a, b) => rank(a.controlId) - rank(b.controlId) || b.position - a.position)
    .map(({ cell }) => cell);
}

interface Rect {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

/**
 * Where the menu goes. Along a bottom bar it opens above the bar, never over
 * its buttons, right-aligned with ⋯ like the device menus; beside a vertical
 * rail it opens on whichever side has room, level with ⋯. Clamped inside the
 * viewport either way (the menu scrolls when it is taller than that).
 */
export function placeOverflowMenu(
  trigger: Rect,
  bar: Rect,
  menu: { width: number; height: number },
  vertical: boolean,
  viewport: { width: number; height: number } = { width: window.innerWidth, height: window.innerHeight }
): { left: number; top: number } {
  // Whole pixels, like MeetingChrome's placement: no blurry sub-pixel text.
  const clamp = (start: number, size: number, extent: number) =>
    Math.round(Math.min(Math.max(start, VIEWPORT_PAD), Math.max(VIEWPORT_PAD, extent - size - VIEWPORT_PAD)));
  if (vertical) {
    let left = bar.left - MENU_GAP - menu.width;
    if (left < VIEWPORT_PAD && bar.right + MENU_GAP + menu.width <= viewport.width - VIEWPORT_PAD) {
      left = bar.right + MENU_GAP;
    }
    return { left: clamp(left, menu.width, viewport.width), top: clamp(trigger.top, menu.height, viewport.height) };
  }
  let top = bar.top - MENU_GAP - menu.height;
  if (top < VIEWPORT_PAD && bar.bottom + MENU_GAP + menu.height <= viewport.height - VIEWPORT_PAD) {
    top = bar.bottom + MENU_GAP;
  }
  return { left: clamp(trigger.right - menu.width, menu.width, viewport.width), top: clamp(top, menu.height, viewport.height) };
}

/**
 * The element a popup opened by `control` should anchor to: the control
 * itself, or ⋯ while the control is in the overflow menu (a hidden button has
 * no box, and focus cannot return to it).
 */
export function controlAnchor(control: HTMLElement): HTMLElement {
  if (!control.closest('.control-cell')?.classList.contains('overflowed')) return control;
  return control.ownerDocument.getElementById('ctl-more') ?? control;
}

function controlOf(cell: HTMLElement): HTMLButtonElement | null {
  return cell.querySelector<HTMLButtonElement>('button.control-button');
}

function isRendered(el: HTMLElement): boolean {
  if (el.hidden) return false;
  const style = getComputedStyle(el);
  return style.display !== 'none' && style.position !== 'absolute' && style.position !== 'fixed';
}

function measureLine(container: HTMLElement, vertical: boolean): BarLine<HTMLElement> {
  const style = getComputedStyle(container);
  const px = (value: string) => parseFloat(value) || 0;
  const inset = vertical
    ? px(style.paddingTop) + px(style.paddingBottom) + px(style.borderTopWidth) + px(style.borderBottomWidth)
    : px(style.paddingLeft) + px(style.paddingRight) + px(style.borderLeftWidth) + px(style.borderRightWidth);
  const items: Array<{ item: BarLine<HTMLElement>['items'][number]; start: number }> = [];
  for (const child of Array.from(container.children)) {
    if (!(child instanceof HTMLElement) || !isRendered(child)) continue;
    const rect = child.getBoundingClientRect();
    const start = vertical ? rect.top : rect.left;
    if (!child.classList.contains('control-cell') && child.querySelector('.control-cell')) {
      items.push({ item: measureLine(child, vertical), start });
    } else {
      items.push({ item: { cell: child, size: vertical ? rect.height : rect.width }, start });
    }
  }
  // In the order the line shows them, which a layout may change with CSS
  // `order`. Compared among siblings only: while everything is laid out, a
  // nested line squeezed smaller than its cells lets them run past its
  // neighbours.
  items.sort((a, b) => a.start - b.start);
  return { gap: px(vertical ? style.rowGap : style.columnGap), inset, items: items.map(({ item }) => item) };
}

/** The control cells of a measured line, in the order the bar shows them;
 * anything else in it only takes room. */
function cellsOf(line: BarLine<HTMLElement>): HTMLElement[] {
  return line.items.flatMap((item) =>
    'cell' in item ? (item.cell.classList.contains('control-cell') ? [item.cell] : []) : cellsOf(item)
  );
}

function isVertical(bar: HTMLElement): boolean {
  return getComputedStyle(bar).flexDirection.startsWith('column');
}

/**
 * The bar's own length along its main axis, padding and border included (its
 * measured line counts those too). Set by the meeting layout, never by the
 * cells: the bar spans the meeting's width, or its height as a rail.
 */
function availableLength(bar: HTMLElement, vertical: boolean): number {
  const box = bar.getBoundingClientRect();
  return vertical ? box.height : box.width;
}

function visibleBadge(cell: HTMLElement): HTMLElement | null {
  const badge = cell.querySelector<HTMLElement>(ATTENTION_BADGE_SELECTOR);
  return badge && !badge.hidden && badge.textContent?.trim() ? badge : null;
}

/**
 * A row the menu offers of its own, not a control that left the bar -- the
 * phone layout's "Developer & test tools" drawer (#239). While `available`,
 * ⋯ shows even if every control fits.
 */
export interface OverflowMenuItem {
  label: string;
  /** Inline SVG markup, in the controls' 24px stroke style. */
  icon: string;
  available: () => boolean;
  run: () => void;
}

export interface ControlOverflowHook {
  /** Re-fit now. The observers do this on their own; this is for when a
   * menu item's `available` flips. */
  update(): void;
  addMenuItem(item: OverflowMenuItem): void;
  isMenuOpen(): boolean;
}

export function setupControlOverflow(doc: Document = document): ControlOverflowHook {
  function required<T extends HTMLElement>(el: Element | null, what: string): T {
    if (!el) throw new Error(`control overflow: ${what} must exist in index.html`);
    return el as T;
  }
  const bar = required<HTMLElement>(doc.querySelector('.controlbar'), '.controlbar');
  const moreButton = required<HTMLButtonElement>(doc.getElementById('ctl-more'), '#ctl-more');
  const moreCell = required<HTMLElement>(moreButton.closest('.control-cell'), '#ctl-more .control-cell');
  const dot = required<HTMLElement>(moreCell.querySelector('.overflow-dot'), '.overflow-dot');
  const menu = required<HTMLElement>(doc.getElementById('overflow-menu'), '#overflow-menu');

  /** The cells in the menu right now, in the order the bar shows them. */
  let overflowed: HTMLElement[] = [];
  const menuItems: OverflowMenuItem[] = [];
  /** The menu's own items that are available right now. */
  let items: OverflowMenuItem[] = [];
  let frame: number | null = null;
  const rows = new Map<HTMLElement | OverflowMenuItem, HTMLButtonElement>();
  const divider = doc.createElement('div');
  divider.className = 'overflow-menu-divider';
  divider.setAttribute('role', 'separator');
  let menuCleanup: (() => void) | null = null;

  function update(): void {
    if (frame !== null) cancelAnimationFrame(frame);
    frame = null;
    // Off screen (the meeting is hidden): keep the last plan for when it
    // comes back, and do not leave a menu floating over the join screen.
    if (bar.getClientRects().length === 0) {
      closeMenu(false);
      mutations.takeRecords();
      return;
    }
    const vertical = isVertical(bar);
    // Lay every control out at its natural size, measure, then hide what
    // does not fit. All in one synchronous pass, so the browser never paints
    // the in-between state.
    for (const cell of Array.from(bar.querySelectorAll<HTMLElement>('.control-cell.overflowed'))) {
      cell.classList.remove('overflowed');
    }
    moreCell.hidden = false;
    const line = measureLine(bar, vertical);
    // A cell that is not rendered even now -- hidden as unsupported (Share
    // without getDisplayMedia, #240), or shown only in another layout -- is
    // in neither the bar nor the menu: `measureLine` skipped it.
    const cells = cellsOf(line).filter((cell) => cell !== moreCell);
    const candidates = collapseCandidates(cells.map((cell) => ({ cell, controlId: controlOf(cell)?.id ?? '' })));
    items = menuItems.filter((item) => item.available());
    const collapsed = planOverflow(line, availableLength(bar, vertical), candidates, moreCell, items.length > 0);
    const focused = doc.activeElement;
    for (const cell of candidates) cell.classList.toggle('overflowed', collapsed.has(cell));
    moreCell.hidden = collapsed.size === 0 && items.length === 0;
    overflowed = cells.filter((cell) => collapsed.has(cell));
    // A control that leaves the bar under the keyboard hands focus to ⋯.
    if (focused instanceof HTMLElement && overflowed.some((cell) => cell.contains(focused))) moreButton.focus();
    // A cell's size can change without the bar's (a label, a media query on
    // the other axis): every cell is watched too.
    watchCells();

    // The dot on ⋯: a hidden control is live (a screen share must never leave
    // the bar without a trace -- `.live` is only ever Share's), or shows a
    // badge (unread chat, a plugin's). Live gives the dot the sharer's color.
    const live = overflowed.map(controlOf).find((control) => control?.classList.contains('live')) ?? null;
    const activity = overflowed.some((cell) => visibleBadge(cell) !== null);
    dot.hidden = !live && !activity;
    dot.classList.toggle('live', live !== null);
    const liveColor = live?.style.getPropertyValue('--control-live-bg');
    if (liveColor) dot.style.setProperty('--control-live-bg', liveColor);
    else dot.style.removeProperty('--control-live-bg');
    moreButton.setAttribute(
      'aria-label',
      ['More controls', live && 'sharing your screen', activity && 'new activity'].filter(Boolean).join(', ')
    );
    if (menuCleanup) {
      if (moreCell.hidden) {
        // Everything fits again under the keyboard: focus goes to the control
        // that just came back to the bar, not to <body> with the menu.
        const key = [...rows].find(([, row]) => row === doc.activeElement)?.[0];
        if (key) {
          const back = key instanceof HTMLElement ? controlOf(key) : null;
          (back && focusable(back) ? back : firstBarControl())?.focus();
        }
        closeMenu(false);
      } else {
        renderMenu(vertical);
      }
    }
    // Everything above was this module's own doing; do not answer it.
    mutations.takeRecords();
  }

  function schedule(): void {
    if (frame === null) frame = requestAnimationFrame(update);
  }

  function focusable(button: HTMLButtonElement): boolean {
    return !button.disabled && button.getClientRects().length > 0;
  }

  /** Where focus goes when the control it was on cannot take it. */
  function firstBarControl(): HTMLButtonElement | undefined {
    return Array.from(bar.querySelectorAll<HTMLButtonElement>('button')).find(focusable);
  }

  function syncRow(row: HTMLButtonElement, cell: HTMLElement): void {
    const control = controlOf(cell);
    const label = cell.querySelector('.meeting-control-label')?.textContent?.trim() ?? '';
    const ariaLabel = control?.getAttribute('aria-label') ?? label;
    const pressed = control?.getAttribute('aria-pressed') ?? null;
    const disabled = control?.disabled ?? true;
    // Why a control is unavailable -- Draw's "Nothing to draw on": its own
    // tooltip, else its title or name.
    const reason = disabled ? cell.querySelector('.control-tooltip')?.textContent?.trim() || control?.title || ariaLabel : '';
    // Named the way the control announces itself ("Open chat, 3 unread"), led
    // by the feature's name where that does not mention it ("Share, Stop
    // sharing your screen"). A disabled one is named for why, always after
    // its name ("Draw, Nothing to draw on"): a reason is about the meeting,
    // not the control. The row shows the name, like the desktop's More rows.
    const said = disabled ? reason : ariaLabel;
    const lead = label.toLowerCase();
    const named = disabled ? said.toLowerCase().startsWith(lead) : said.toLowerCase().includes(lead);
    row.setAttribute('aria-label', !label || named ? said : `${label}, ${said}`);
    row.setAttribute('role', pressed === null ? 'menuitem' : 'menuitemcheckbox');
    if (pressed === null) row.removeAttribute('aria-checked');
    else row.setAttribute('aria-checked', pressed === 'true' ? 'true' : 'false');
    // A plugin button that opens a popover says so; so does its row.
    const popup = control?.getAttribute('aria-haspopup');
    if (popup) row.setAttribute('aria-haspopup', popup);
    else row.removeAttribute('aria-haspopup');
    // aria-disabled, not `disabled`: a disabled item stays focusable so the
    // arrow keys still reach it and its reason is read out.
    if (disabled) row.setAttribute('aria-disabled', 'true');
    else row.removeAttribute('aria-disabled');

    const leading = rowLeading(control?.querySelector('svg')?.cloneNode(true) ?? null, label || ariaLabel);
    if (disabled) {
      const note = doc.createElement('span');
      note.className = 'overflow-menu-note';
      note.textContent = reason;
      leading.querySelector('.meeting-menu-row-copy')!.append(note);
    }
    const parts: Node[] = [leading];

    // The state the bar showed on the button, as a trailing chip: an unread
    // or plugin badge, a live share (in the sharer's identity color), or on.
    const badge = visibleBadge(cell);
    const live = control?.classList.contains('live') ?? false;
    if (badge || live || pressed === 'true') {
      const chip = doc.createElement('span');
      chip.setAttribute('aria-hidden', 'true');
      if (badge) {
        chip.className = 'overflow-menu-state overflow-menu-badge';
        chip.textContent = badge.textContent!.trim();
      } else if (live) {
        chip.className = 'overflow-menu-state overflow-menu-live';
        chip.textContent = 'Live';
        for (const prop of ['--control-live-bg', '--control-live-fg']) {
          const value = control!.style.getPropertyValue(prop);
          if (value) chip.style.setProperty(prop, value);
        }
      } else {
        chip.className = 'overflow-menu-state';
        chip.textContent = 'On';
      }
      parts.push(chip);
    }
    row.replaceChildren(...parts);
  }

  function rowLeading(glyph: Node | null, text: string): HTMLElement {
    const leading = doc.createElement('span');
    leading.className = 'overflow-menu-leading';
    const icon = doc.createElement('span');
    icon.className = 'overflow-menu-icon';
    icon.setAttribute('aria-hidden', 'true');
    if (glyph) icon.append(glyph);
    const copy = doc.createElement('span');
    copy.className = 'meeting-menu-row-copy';
    copy.textContent = text;
    leading.append(icon, copy);
    return leading;
  }

  function rowFor(key: HTMLElement | OverflowMenuItem, onClick: () => void): HTMLButtonElement {
    let row = rows.get(key);
    if (!row) {
      row = doc.createElement('button');
      row.type = 'button';
      row.className = 'meeting-menu-row overflow-menu-row';
      row.addEventListener('click', onClick);
      rows.set(key, row);
    }
    return row;
  }

  function renderMenu(vertical: boolean): void {
    const focusedKey = [...rows].find(([, row]) => row === doc.activeElement)?.[0];
    const keep = new Set<HTMLElement | OverflowMenuItem>([...overflowed, ...items]);
    for (const [key, row] of rows) {
      if (keep.has(key)) continue;
      row.remove();
      rows.delete(key);
    }
    const controlRows = overflowed.map((cell) => {
      const row = rowFor(cell, () => activate(cell));
      syncRow(row, cell);
      return row;
    });
    const itemRows = items.map((item) => {
      const row = rowFor(item, () => {
        closeMenu(true);
        item.run();
      });
      if (!row.firstChild) {
        row.setAttribute('role', 'menuitem');
        const glyph = doc.createElement('template');
        glyph.innerHTML = item.icon;
        row.append(rowLeading(glyph.content.firstElementChild, item.label));
      }
      return row;
    });
    const ordered: Element[] = [...controlRows, ...(controlRows.length && itemRows.length ? [divider] : []), ...itemRows];
    // Re-insert only when the order changed: moving the focused row would blur
    // it. A focused row whose control went back to the bar hands focus on.
    if (ordered.length !== menu.children.length || ordered.some((el, index) => menu.children[index] !== el)) {
      menu.replaceChildren(...ordered);
      if (focusedKey) (rows.get(focusedKey) ?? menuRows()[0])?.focus();
    }
    placeMenu(vertical);
  }

  function placeMenu(vertical: boolean): void {
    const { left, top } = placeOverflowMenu(
      moreButton.getBoundingClientRect(),
      bar.getBoundingClientRect(),
      { width: menu.offsetWidth, height: menu.offsetHeight },
      vertical
    );
    menu.style.left = `${left}px`;
    menu.style.top = `${top}px`;
    menu.classList.add('placed');
  }

  function activate(cell: HTMLElement): void {
    const control = controlOf(cell);
    if (!control || control.disabled) return;
    // Focus goes back to ⋯ first, so a control that moves focus itself (the
    // chat drawer) still wins. Synchronous: the click keeps the user
    // activation that getDisplayMedia and full screen require.
    closeMenu(true);
    control.click();
  }

  function menuRows(): HTMLButtonElement[] {
    return Array.from(menu.querySelectorAll<HTMLButtonElement>('.overflow-menu-row'));
  }

  function openMenu(): void {
    if (menuCleanup || moreCell.hidden) return;
    menu.hidden = false;
    menu.classList.remove('placed');
    moreButton.setAttribute('aria-expanded', 'true');
    renderMenu(isVertical(bar));
    menuRows()[0]?.focus();

    // Escape closes and returns focus to ⋯; the arrows (and Home/End) move
    // between rows, wrapping. Rows are real buttons, so Tab reaches them too.
    const onKeydown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        closeMenu(true);
        return;
      }
      if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
      const all = menuRows();
      if (all.length === 0) return;
      event.preventDefault();
      const current = all.indexOf(doc.activeElement as HTMLButtonElement);
      const next =
        event.key === 'Home'
          ? 0
          : event.key === 'End'
            ? all.length - 1
            : current < 0
              ? event.key === 'ArrowDown'
                ? 0
                : all.length - 1
              : (current + (event.key === 'ArrowDown' ? 1 : -1) + all.length) % all.length;
      all[next]?.focus();
    };
    // Tabbing out of the menu closes it, as leaving any menu does.
    const onFocusOut = (event: FocusEvent) => {
      const next = event.relatedTarget as Node | null;
      if (next && !menu.contains(next) && next !== moreButton) closeMenu(false);
    };
    // On a touch screen the tap that dismisses the menu must not also land on
    // what is under it: beside the menu is right by Leave. A transparent
    // backdrop under the menu takes that whole tap.
    const view = doc.defaultView;
    let backdrop: HTMLElement | null = null;
    if (view?.matchMedia?.('(pointer: coarse)').matches) {
      backdrop = doc.createElement('div');
      backdrop.className = 'overflow-menu-backdrop';
      backdrop.addEventListener('click', () => closeMenu(true));
      menu.before(backdrop);
    }
    const dismiss: DismissibleLayerCleanup = installDismissibleLayer({
      isOpen: () => menuCleanup !== null,
      // The backdrop is "inside": it closes on its own click, after the whole
      // tap, so nothing is left for the tap to land on.
      getInsideNodes: () => [menu, moreButton, backdrop],
      getPopupNodes: () => [menu],
      getOpener: () => moreButton,
      onDismiss: () => closeMenu(false),
      document: doc,
    });
    // A phone's URL bar collapsing moves the bar without resizing the layout
    // viewport; only the visual viewport reports it. (A window resize re-fits,
    // which places the menu again.)
    const place = () => placeMenu(isVertical(bar));
    view?.visualViewport?.addEventListener('resize', place);
    doc.addEventListener('keydown', onKeydown);
    menu.addEventListener('focusout', onFocusOut);
    menuCleanup = () => {
      dismiss();
      backdrop?.remove();
      view?.visualViewport?.removeEventListener('resize', place);
      doc.removeEventListener('keydown', onKeydown);
      menu.removeEventListener('focusout', onFocusOut);
    };
  }

  function closeMenu(restoreFocus: boolean): void {
    if (!menuCleanup) return;
    menuCleanup();
    menuCleanup = null;
    menu.hidden = true;
    menu.classList.remove('placed');
    moreButton.setAttribute('aria-expanded', 'false');
    menu.replaceChildren();
    rows.clear();
    if (restoreFocus) moreButton.focus();
  }

  moreButton.addEventListener('click', () => {
    if (menuCleanup) closeMenu(false);
    else openMenu();
  });

  // Re-fit on a window resize or rotation right in the event, which the
  // browser runs before it lays out and paints the frame: no frame of the
  // unfitted bar.
  doc.defaultView?.addEventListener('resize', update);
  // And whenever the bar or any cell changes size for another reason (a
  // label that grows, a stylesheet), deferred a frame: hiding cells changes
  // their size (and can change the bar's other dimension), and answering that
  // inside this callback would be a ResizeObserver loop. The re-fit that
  // follows our own change finds the same plan and changes nothing, so it
  // settles there.
  const resizes = new ResizeObserver(schedule);
  // The bar's content box: it moves with the bar's length and its padding
  // alike, and the plan counts both.
  resizes.observe(bar);
  const watched = new Set<Element>();
  function watchCells(): void {
    const current = new Set(bar.querySelectorAll('.control-cell'));
    for (const cell of watched) {
      if (current.has(cell)) continue;
      resizes.unobserve(cell);
      watched.delete(cell);
    }
    for (const cell of current) {
      // Observing again would re-deliver the current size: a re-fit per frame.
      if (watched.has(cell)) continue;
      // A cell's border box is what it takes from the bar; padding alone
      // would leave its content box unchanged.
      resizes.observe(cell, { box: 'border-box' });
      watched.add(cell);
    }
  }
  // ...and when its contents change: a plugin adds or drops a button, a label
  // or badge changes, a control is disabled or hidden. Cells, text and
  // IMMEDIATE_ATTRIBUTES are answered in the mutation's own microtask, so
  // the new state is never painted unfitted; other attributes in the next
  // frame. A rewrite with the same value (plugins re-render every button,
  // Draw re-applies its copy on every tile change) is no change at all.
  const mutations = new MutationObserver((records) => {
    let now = false;
    let later = false;
    for (const record of records) {
      if (record.type !== 'attributes') now = true;
      else if ((record.target as Element).getAttribute(record.attributeName!) === record.oldValue) continue;
      else if (IMMEDIATE_ATTRIBUTES.has(record.attributeName!)) now = true;
      else later = true;
    }
    if (now) update();
    else if (later) schedule();
  });
  mutations.observe(bar, {
    subtree: true,
    childList: true,
    characterData: true,
    attributes: true,
    attributeOldValue: true,
    attributeFilter: ['class', 'hidden', 'disabled', 'aria-pressed', 'aria-label', 'title'],
  });
  // The same for the meeting screen appearing, so joining never paints one
  // frame of the unfitted bar. Only its `.hidden` matters: other classes on
  // it (`.chat-open`) change nothing here.
  const meetingScreen = bar.closest<HTMLElement>('#meeting-screen');
  if (meetingScreen) {
    let meetingHidden = meetingScreen.classList.contains('hidden');
    new MutationObserver(() => {
      if (meetingScreen.classList.contains('hidden') === meetingHidden) return;
      meetingHidden = !meetingHidden;
      update();
    }).observe(meetingScreen, { attributes: true, attributeFilter: ['class'] });
  }
  // Labels are measured in the UI font; re-fit once it has loaded.
  doc.fonts?.addEventListener('loadingdone', schedule);

  update();
  return {
    update,
    addMenuItem(item) {
      menuItems.push(item);
      update();
    },
    isMenuOpen: () => menuCleanup !== null,
  };
}
