import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';
import { mockIPC } from '@tauri-apps/api/mocks';

const UPDATE_VERSION = '2.0.0-beta.20260712.123456';

function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()));
}

function selectorFor(element) {
  const classes = [...element.classList].map((name) => `.${name}`).join('');
  return `${element.tagName.toLowerCase()}${classes}`;
}

function rect(element) {
  if (!element) return { present: false, left: -1, right: -1, width: 0, height: 0 };
  const bounds = element.getBoundingClientRect();
  return {
    present: true,
    left: bounds.left,
    right: bounds.right,
    width: bounds.width,
    height: bounds.height
  };
}

async function renderFixture() {
  try {
    // ToastHost registers three Tauri event listeners on mount. The official
    // API mock supplies that boundary while leaving the rendered component,
    // stores, Svelte effects, and component CSS completely real.
    mockIPC((command) => {
      if (command === 'plugin:event|listen') return 1;
      return null;
    });

    const [{ mount }, { default: ToastHost }, { markUpdateAvailable }] = await Promise.all([
      import('svelte'),
      import('$lib/components/ToastHost.svelte'),
      import('$lib/stores/updateStatus.svelte')
    ]);

    markUpdateAvailable(UPDATE_VERSION);
    mount(ToastHost, { target: document.querySelector('#app') });

  await document.fonts.ready;
  const deadline = performance.now() + 3000;
  while (!document.querySelector('button.action') || !document.querySelector('button.dismiss')) {
    if (performance.now() >= deadline) throw new Error('ToastHost did not render the combined available state');
    await nextFrame();
  }
  // Let ToastHost's 180ms entrance transition finish, then give layout two
  // complete frames after the real font metrics have settled.
  await new Promise((resolve) => setTimeout(resolve, 220));
  await nextFrame();
  await nextFrame();

  const host = document.querySelector('.toast-host-anchor');
  if (!host) throw new Error('ToastHost anchor is missing');
  const pill = host.querySelector('.pill');
  const icon = host.querySelector('.icon svg');
  const message = host.querySelector('.message');
  const action = host.querySelector('button.action');
  const dismiss = host.querySelector('button.dismiss');
  const dismissIcon = dismiss?.querySelector('svg');
  if (!pill || !icon || !message || !action || !dismiss || !dismissIcon) {
    throw new Error('available toast must render icon, message, action, dismiss, and pill together');
  }
  const [messageFonts, actionFonts] = await Promise.all([
    document.fonts.load('500 12.5px "Albert Sans"', message.textContent ?? ''),
    document.fonts.load('600 12.5px "Albert Sans"', action.textContent ?? '')
  ]);
  await document.fonts.ready;

  const htmlElements = [host, ...host.querySelectorAll('*')].filter(
    (element) =>
      element instanceof HTMLElement &&
      // #422: the dismiss button deliberately has a 40px ::after hit target around
      // its 20px visual box. Chromium includes that pseudo-element in the
      // button's scrollWidth (30px), even though the visual button and the
      // whole toast remain inside the viewport. Its rect and SVG are checked.
      !element.matches('button.dismiss')
  );
  const overflow = [...htmlElements, dismissIcon]
    .filter((element) => element.scrollWidth > element.clientWidth)
    .map((element) => ({
      selector: selectorFor(element),
      scrollWidth: element.scrollWidth,
      clientWidth: element.clientWidth
    }));

  const measurement = {
    viewport: { width: window.innerWidth, deviceScaleFactor: window.devicePixelRatio },
    fonts: {
      status: document.fonts.status,
      message: messageFonts.length > 0 && document.fonts.check('500 12.5px "Albert Sans"'),
      action: actionFonts.length > 0 && document.fonts.check('600 12.5px "Albert Sans"'),
      computedMessageFamily: getComputedStyle(message).fontFamily,
      computedActionFamily: getComputedStyle(action).fontFamily
    },
    host: rect(host),
    pill: rect(pill),
    icon: rect(icon),
    message: { ...rect(message), text: message.textContent },
    action: { ...rect(action), text: action.textContent },
    dismiss: {
      ...rect(dismiss),
      label: dismiss.getAttribute('aria-label'),
      icon: {
        ...rect(dismissIcon),
        scrollWidth: dismissIcon.scrollWidth,
        clientWidth: dismissIcon.clientWidth
      }
    },
    overflow
  };
    document.body.dataset.toastMeasurement = encodeURIComponent(JSON.stringify(measurement));
  } catch (error) {
    const message = error instanceof Error ? `${error.message}\n${error.stack ?? ''}` : String(error);
    document.body.dataset.toastMeasurementError = encodeURIComponent(message);
  }
}

void renderFixture();
