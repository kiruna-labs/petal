// Reactions — a built-in Petal plugin. One file, no imports: built-ins are
// buildless so both clients can compile them in with a `?raw` import and the
// browser deploy stays self-contained. Third-party plugins use
// @petal/plugin-sdk + Vite instead; see docs/PLUGINS.md.
//
// Three frames run this same file:
//   logic   -> activate(): owns the wire protocol and fan-out
//   popover -> mountSurface('picker'): the 8-emoji picker
//   overlay -> mountSurface('fx'): draws floating emoji over the meeting
//
// Wire (plugins/README.md §2.11): topic `plugin/petal.reactions/emoji`,
// lossy, `{ e: "<emoji>", t: <ms> }`, at most 4 per second per sender.
// Sender identity comes from the host, never from the payload.

const EMOJI = ['👍', '❤️', '😂', '🎉', '👏', '🤔', '👀', '🔥'];
const MAX_PER_SECOND = 4;
const REACTION_LIFETIME_MS = 2600;

/** @type {import('@petal/plugin-sdk').PluginDefinition} */
const definition = {
  activate(petal) {
    const overlay = petal.ui.channel('fx');
    const picker = petal.ui.channel('picker');
    const sent = [];

    function show(emoji, name) {
      overlay.postMessage({ kind: 'react', emoji, name });
    }

    function throttled() {
      const now = Date.now();
      while (sent.length && now - sent[0] > 1000) sent.shift();
      if (sent.length >= MAX_PER_SECOND) return true;
      sent.push(now);
      return false;
    }

    picker.onMessage((message) => {
      if (!message || message.kind !== 'pick') return;
      const emoji = String(message.emoji);
      if (!EMOJI.includes(emoji) || throttled()) return;
      // Local echo first so the sender sees an instant response even when
      // the meeting-wide publish is unavailable (M1) or slow.
      const self = petal.meeting.self();
      show(emoji, self ? self.name : 'You');
      petal.data
        .publish('emoji', { e: emoji, t: Date.now() }, { reliable: false })
        .catch((error) => {
          if (error && error.code === 'unavailable') return; // not wired yet on this host
          petal.log.warn('publish failed', error && error.message);
        });
    });

    petal.data.on('emoji', (message) => {
      if (message.sender.isLocal) return; // already echoed locally
      let body;
      try {
        body = message.json();
      } catch (_) {
        return;
      }
      if (!body || !EMOJI.includes(body.e)) return;
      show(body.e, message.sender.name);
    });
  },

  mountSurface(petal, surface) {
    if (surface.id === 'picker') return mountPicker(petal, surface);
    if (surface.id === 'fx') return mountOverlay(surface);
  },
};

function mountPicker(petal, surface) {
  const root = surface.root;
  root.innerHTML = '';
  const style = document.createElement('style');
  style.textContent = `
    :root { color-scheme: dark; }
    body { margin: 0; font: 14px system-ui, sans-serif; }
    .picker { display: flex; gap: 4px; padding: 8px; border-radius: 14px;
      background: linear-gradient(180deg, #1c1e21, #121416); border: 1px solid rgba(255,255,255,.12);
      box-shadow: 0 12px 32px rgba(0,0,0,.35); box-sizing: border-box; width: 100%; justify-content: space-between; }
    button { all: unset; cursor: pointer; width: 32px; height: 32px; line-height: 32px; text-align: center;
      font-size: 22px; border-radius: 8px; transition: transform 100ms ease, background 100ms ease; }
    button:hover, button:focus-visible { background: rgba(255,255,255,.1); transform: scale(1.15); outline: none; }
    button:active { transform: scale(0.95); }
  `;
  root.appendChild(style);
  const row = document.createElement('div');
  row.className = 'picker';
  row.setAttribute('role', 'group');
  row.setAttribute('aria-label', 'Pick a reaction');
  for (const emoji of EMOJI) {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = emoji;
    button.setAttribute('aria-label', `React with ${emoji}`);
    button.addEventListener('click', () => surface.channel.postMessage({ kind: 'pick', emoji }));
    row.appendChild(button);
  }
  root.appendChild(row);
  const first = row.querySelector('button');
  if (first) first.focus();
  void petal;
}

function mountOverlay(surface) {
  const root = surface.root;
  root.innerHTML = '';
  const style = document.createElement('style');
  style.textContent = `
    html, body { margin: 0; height: 100%; background: transparent; overflow: hidden; pointer-events: none; }
    .fx { position: fixed; inset: 0; }
    .r { position: absolute; bottom: 96px; display: flex; flex-direction: column; align-items: center; gap: 2px;
      animation: rise ${REACTION_LIFETIME_MS}ms cubic-bezier(.2,.7,.3,1) forwards; will-change: transform, opacity; }
    .r .e { font-size: 34px; line-height: 1; filter: drop-shadow(0 2px 6px rgba(0,0,0,.45)); }
    .r .n { font: 600 11px system-ui, sans-serif; color: rgba(255,255,255,.92); background: rgba(0,0,0,.45);
      padding: 2px 6px; border-radius: 999px; white-space: nowrap; }
    @keyframes rise { 0% { transform: translateY(0) scale(.7); opacity: 0; } 12% { opacity: 1; transform: translateY(-20px) scale(1); }
      100% { transform: translateY(-260px) scale(1.05); opacity: 0; } }
    @media (prefers-reduced-motion: reduce) { .r { animation-duration: 1200ms; } }
  `;
  root.appendChild(style);
  const layer = document.createElement('div');
  layer.className = 'fx';
  layer.setAttribute('aria-live', 'polite');
  root.appendChild(layer);

  surface.channel.onMessage((message) => {
    if (!message || message.kind !== 'react') return;
    const node = document.createElement('div');
    node.className = 'r';
    const lane = 16 + Math.random() * 68; // percent across the width
    node.style.left = `calc(${lane}% - 20px)`;
    node.style.setProperty('--drift', `${(Math.random() - 0.5) * 60}px`);
    const e = document.createElement('span');
    e.className = 'e';
    e.textContent = String(message.emoji);
    const n = document.createElement('span');
    n.className = 'n';
    n.textContent = String(message.name || '').split(' ')[0] || '';
    node.appendChild(e);
    if (n.textContent) node.appendChild(n);
    layer.appendChild(node);
    setTimeout(() => node.remove(), REACTION_LIFETIME_MS + 100);
  });
}

export default (globalThis.__petalRegister ? globalThis.__petalRegister(definition) : definition);
