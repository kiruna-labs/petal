export type Density = 1 | 2;
export interface FidelityCell { sourceScale: Density; receiverScale: Density; sourcePixels: string; expectedDecodedPixels: string; expectation: string; }
export const cells: FidelityCell[] = [
  { sourceScale: 2, receiverScale: 2, sourcePixels: '1920×1200', expectedDecodedPixels: '1920×1264 with title bar', expectation: 'Full source detail selected.' },
  { sourceScale: 2, receiverScale: 1, sourcePixels: '1920×1200', expectedDecodedPixels: '960×632 with title bar', expectation: 'Half layer; no wasteful full-layer decode.' },
  { sourceScale: 1, receiverScale: 1, sourcePixels: '960×600', expectedDecodedPixels: '960×632 with title bar', expectation: 'Native source pixels preserved.' },
  { sourceScale: 1, receiverScale: 2, sourcePixels: '960×600', expectedDecodedPixels: '960×632 rendered at 1920×1264', expectation: 'Upscaled without inventing source detail.' },
];

export function fixtureManifest(cell: FidelityCell, lockedAt: string | null) {
  return { schemaVersion: 1, fixture: 'petal-window-fidelity-v1', logicalSize: { width: 960, height: 600 }, ...cell, pattern: { checkerCellPx: 4, ruleWidthPx: 1, font: '13px monospace', swatches: ['#ff0000', '#00ff00', '#0000ff'] }, capture: { method: 'macOS OS-compositor screenshot', browserScreenshotIsValidEvidence: false, rescaleBeforeScoring: false }, lockedAt };
}

function cellFromSelect(value: string) { const [source, receiver] = value.split('-').map(Number); return cells.find(c => c.sourceScale === source && c.receiverScale === receiver)!; }
function download(value: object) { const blob = new Blob([`${JSON.stringify(value, null, 2)}\n`], { type: 'application/json' }); const link = document.createElement('a'); link.href = URL.createObjectURL(blob); link.download = `petal-fidelity-${Date.now()}.json`; link.click(); URL.revokeObjectURL(link.href); }

export function mountFidelityFixture(root: HTMLElement) {
  root.innerHTML = `<main class="fidelity-shell"><header><p class="eyebrow">Petal QA fixture</p><h1>Window fidelity matrix</h1><p>Select the real source and receiver density, keep this page visible through the countdown, then capture the receiver through the macOS compositor.</p></header><section class="controls"><label>Matrix cell<select id="cell">${cells.map(c => `<option value="${c.sourceScale}-${c.receiverScale}">${c.sourceScale}× source → ${c.receiverScale}× receiver</option>`).join('')}</select></label><button id="start">Start 15-second countdown</button><button id="manifest" disabled>Download manifest</button></section><section id="status" class="status idle">READY — SELECT A CELL</section><section class="pattern" aria-label="Fidelity reference pattern"><div class="checker"></div><div class="rules"></div><div class="swatches"><i></i><i></i><i></i></div><pre>ABCDEFGHIJKLMNOPQRSTUVWXYZ  abcdefghijklmnopqrstuvwxyz\n0123456789  !@#$%^&amp;*()_+-=[]{};':&quot;,.&lt;&gt;/?</pre><div class="micro">Microtext 13 px · 1 px rules · 4 px checker cells</div></section><aside><h2>Reference metadata</h2><pre id="metadata"></pre><p class="warning"><strong>Evidence rule:</strong> browser screenshots do not include the protected hardware-decoded video overlay and cannot replace an OS-compositor capture.</p></aside></main>`;
  const select = root.querySelector<HTMLSelectElement>('#cell')!, status = root.querySelector<HTMLElement>('#status')!, metadata = root.querySelector<HTMLElement>('#metadata')!, manifest = root.querySelector<HTMLButtonElement>('#manifest')!;
  let lockedAt: string | null = null, timer: number | null = null;
  const render = () => { const cell = cellFromSelect(select.value); metadata.textContent = JSON.stringify(fixtureManifest(cell, lockedAt), null, 2); };
  select.value = '2-2'; select.addEventListener('change', () => { lockedAt = null; manifest.disabled = true; status.textContent = 'READY — SELECT A CELL'; status.className = 'status idle'; render(); });
  root.querySelector('#start')!.addEventListener('click', () => { if (timer) clearInterval(timer); let remaining = 15; lockedAt = null; manifest.disabled = true; status.className = 'status counting'; status.textContent = `KEEP THIS PAGE VISIBLE · ${remaining}s`; timer = window.setInterval(() => { remaining -= 1; if (remaining <= 0) { clearInterval(timer!); timer = null; lockedAt = new Date().toISOString(); status.textContent = 'CAPTURE LOCKED — SAFE TO SWITCH AWAY'; status.className = 'status locked'; manifest.disabled = false; render(); } else status.textContent = `KEEP THIS PAGE VISIBLE · ${remaining}s`; }, 1000); });
  manifest.addEventListener('click', () => download(fixtureManifest(cellFromSelect(select.value), lockedAt))); render();
}
