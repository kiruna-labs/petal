// The popover's utility row went from two items to three ("Open Petal",
// "Settings", "Quit") in a `grid-template-columns: 1fr 1fr` grid, so "Quit"
// wraps onto a second row. Petal's hard rule is that user-facing text never
// truncates, and the row is inside a fixed 280px popover -- so measure the
// real rendered pixels rather than reasoning about them.
import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';

const POPOVER_WIDTH = 280;

async function renderFixture() {
  try {
    const [{ mount }, { default: MenuItem }] = await Promise.all([
      import('svelte'),
      import('$lib/components/MenuItem.svelte')
    ]);

    // Mirror the popover's own shell + .utility-row rules verbatim.
    const host = document.querySelector('#app');
    host.style.width = `${POPOVER_WIDTH}px`;
    const row = document.createElement('div');
    row.className = 'utility-row';
    row.style.display = 'grid';
    row.style.gridTemplateColumns = '1fr 1fr';
    row.style.gap = '6px';
    row.style.padding = '8px';
    host.appendChild(row);

    const items = [
      { label: 'Open Petal', icon: 'window' },
      { label: 'Settings', icon: 'settings' },
      { label: 'Quit', icon: 'quit', tone: 'danger' }
    ];
    for (const props of items) {
      const cell = document.createElement('div');
      row.appendChild(cell);
      mount(MenuItem, { target: cell, props: { ...props, onclick: () => {} } });
    }
    // `.utility-row :global(.menu-item) { justify-content: center; }`
    for (const button of row.querySelectorAll('.menu-item')) {
      button.style.justifyContent = 'center';
    }

    await document.fonts.ready;
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));

    const buttons = [...row.querySelectorAll('.menu-item')];
    const overflow = [];
    for (const button of buttons) {
      for (const element of [button, ...button.querySelectorAll('*')]) {
        if (!(element instanceof HTMLElement)) continue;
        if (element.scrollWidth > element.clientWidth) {
          overflow.push({
            label: button.textContent?.trim(),
            tag: element.tagName.toLowerCase(),
            scrollWidth: element.scrollWidth,
            clientWidth: element.clientWidth
          });
        }
      }
    }
    const rowRect = row.getBoundingClientRect();
    // A grid track's default `min-width: auto` means an over-full row does not
    // CLIP its cells -- it grows past the popover instead, and the popover's
    // fixed 280px is what cuts the text off. Per-element scrollWidth cannot
    // see that, so measure the row against its own container too.
    const shell = document.querySelector('#app');
    document.body.dataset.menubarUtilityRowMeasurement = encodeURIComponent(JSON.stringify({
      viewport: window.innerWidth,
      popoverWidth: POPOVER_WIDTH,
      row: {
        left: rowRect.left,
        right: rowRect.right,
        width: rowRect.width,
        height: rowRect.height,
        scrollWidth: row.scrollWidth,
        clientWidth: row.clientWidth,
        hostScrollWidth: shell.scrollWidth,
        hostClientWidth: shell.clientWidth
      },
      items: buttons.map((button) => {
        const rect = button.getBoundingClientRect();
        const span = button.querySelector('span');
        return {
          label: button.textContent?.trim(),
          top: Math.round(rect.top),
          width: rect.width,
          labelScrollWidth: span?.scrollWidth ?? 0,
          labelClientWidth: span?.clientWidth ?? 0
        };
      }),
      overflow
    }));
  } catch (error) {
    document.body.dataset.menubarUtilityRowMeasurementError = encodeURIComponent(
      error instanceof Error ? error.message : String(error)
    );
  }
}

void renderFixture();
