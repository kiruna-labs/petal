import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';

async function renderFixture() {
  try {
    const [{ mount }, { default: PermissionRow }] = await Promise.all([
      import('svelte'),
      import('$lib/components/PermissionRow.svelte')
    ]);
    mount(PermissionRow, {
      target: document.querySelector('#app'),
      props: {
        icon: 'accessibility',
        title: 'Accessibility',
        required: true,
        status: 'repair',
        actionLabel: 'Open Accessibility Settings',
        repairSettingsOpened: true,
        repairRestartFailed: true,
        onOpenSettings: () => {},
        onConfirmRepairRestart: () => {}
      }
    });
    await document.fonts.ready;
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    const row = document.querySelector('.permission-row.repair');
    if (!row) throw new Error('Accessibility repair row did not render');
    const elements = [row, ...row.querySelectorAll('*')].filter(
      (element) => element instanceof HTMLElement
    );
    const overflow = elements
      .filter((element) => element.scrollWidth > element.clientWidth)
      .map((element) => ({
        tag: element.tagName.toLowerCase(),
        className: element.className,
        scrollWidth: element.scrollWidth,
        clientWidth: element.clientWidth
      }));
    const rect = row.getBoundingClientRect();
    document.body.dataset.accessibilityRepairMeasurement = encodeURIComponent(JSON.stringify({
      viewport: window.innerWidth,
      row: { left: rect.left, right: rect.right, width: rect.width },
      overflow,
      instructions: row.querySelector('.repair-steps')?.textContent?.replace(/\s+/g, ' ').trim(),
      fallback: row.querySelector('.repair-fallback')?.textContent?.replace(/\s+/g, ' ').trim()
    }));
  } catch (error) {
    document.body.dataset.accessibilityRepairMeasurementError = encodeURIComponent(
      error instanceof Error ? error.message : String(error)
    );
  }
}

void renderFixture();
