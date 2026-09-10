// #125's failure branch, driven end to end through the REAL frontend path.
//
// Nothing here fakes the toast state: the fixture mounts the real ToastHost,
// calls the real `installUpdateAndRelaunch()`, and makes the real Tauri
// command reject with the exact string `updater.rs`'s archive guard produces.
// The recovery action, its label, the URL it opens, and the rendered pixel
// fit are all consequences of that one rejection -- which is the point, since
// this failure branch shipped unobserved through seven releases.
import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';
import { mockIPC } from '@tauri-apps/api/mocks';

// The real Sentry PETAL-DESKTOP-2G shape, as `incompatible_update_message`
// renders it on Windows after #116's device-wording fix.
const REJECTION =
  'update is incompatible with this PC: update archive is not a Windows executable';

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
  const invoked = [];
  const openedUrls = [];
  try {
    // `isTauri()` reads `globalThis.isTauri`, which the real Tauri runtime
    // sets and `mockIPC` does not. Without it `installUpdateAndRelaunch`
    // short-circuits to 'unavailable' and never reaches the failure branch.
    globalThis.isTauri = true;
    mockIPC((command, payload) => {
      invoked.push(command);
      if (command === 'plugin:event|listen') return 1;
      // The archive guard's rejection. Tauri rejects with a plain string, so
      // this reproduces the real shape `updateErrorMessage` has to handle.
      if (command === 'download_and_install_compatible_update') {
        return Promise.reject(REJECTION);
      }
      if (command === 'plugin:opener|open_url') {
        openedUrls.push(payload?.url ?? payload?.path ?? null);
        return null;
      }
      return null;
    });

    const [{ mount }, { default: ToastHost }, { installUpdateAndRelaunch }, { updateStatus }] =
      await Promise.all([
        import('svelte'),
        import('$lib/components/ToastHost.svelte'),
        import('$lib/updater'),
        import('$lib/stores/updateStatus.svelte')
      ]);

    mount(ToastHost, { target: document.querySelector('#app') });

    // The real install path -- the one a stranded client takes every time it
    // presses "Restart now".
    const result = await installUpdateAndRelaunch('toast');

    await document.fonts.ready;
    const deadline = performance.now() + 3000;
    while (!document.querySelector('button.action') || !document.querySelector('button.dismiss')) {
      if (performance.now() >= deadline) {
        throw new Error('a rejected update archive rendered no recovery action');
      }
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
      throw new Error('recovery toast must render icon, message, action, dismiss, and pill');
    }
    const [messageFonts, actionFonts] = await Promise.all([
      document.fonts.load('500 12.5px "Albert Sans"', message.textContent ?? ''),
      document.fonts.load('600 12.5px "Albert Sans"', action.textContent ?? '')
    ]);
    await document.fonts.ready;

    const htmlElements = [host, ...host.querySelectorAll('*')].filter(
      (element) =>
        element instanceof HTMLElement &&
        // #422: the dismiss button deliberately has a 40px ::after hit target
        // around its 20px visual box, which Chromium counts in scrollWidth.
        // Its rect and SVG are checked separately.
        !element.matches('button.dismiss')
    );
    const overflow = [...htmlElements, dismissIcon]
      .filter((element) => element.scrollWidth > element.clientWidth)
      .map((element) => ({
        selector: selectorFor(element),
        text: element.textContent,
        scrollWidth: element.scrollWidth,
        clientWidth: element.clientWidth
      }));

    // The affordance has to actually DO something: click it and record the URL
    // the opener plugin was handed.
    action.click();
    await nextFrame();
    await new Promise((resolve) => setTimeout(resolve, 50));

    const measurement = {
      viewport: { width: window.innerWidth, deviceScaleFactor: window.devicePixelRatio },
      fonts: {
        status: document.fonts.status,
        message: messageFonts.length > 0 && document.fonts.check('500 12.5px "Albert Sans"'),
        action: actionFonts.length > 0 && document.fonts.check('600 12.5px "Albert Sans"'),
        computedMessageFamily: getComputedStyle(message).fontFamily,
        computedActionFamily: getComputedStyle(action).fontFamily
      },
      result,
      statusKind: updateStatus.kind,
      statusRecovery: updateStatus.recovery ?? null,
      invoked,
      openedUrls,
      documentScrollWidth: document.documentElement.scrollWidth,
      host: rect(host),
      pill: rect(pill),
      icon: rect(icon),
      message: {
        ...rect(message),
        text: message.textContent,
        scrollWidth: message.scrollWidth,
        clientWidth: message.clientWidth,
        textOverflow: getComputedStyle(message).textOverflow
      },
      action: {
        ...rect(action),
        text: action.textContent,
        scrollWidth: action.scrollWidth,
        clientWidth: action.clientWidth,
        textOverflow: getComputedStyle(action).textOverflow,
        whiteSpace: getComputedStyle(action).whiteSpace
      },
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
