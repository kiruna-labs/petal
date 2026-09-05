import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';

export type DeviceMenuKind = 'audio' | 'camera';

export interface DeviceOption {
  id: string;
  label: string;
}

export interface DeviceMenuHandlers {
  list: (kind: MediaDeviceKind) => Promise<MediaDeviceInfo[]>;
  applyAudioInput: (deviceId: string) => Promise<void>;
  applyAudioOutput: (deviceId: string) => Promise<void>;
  applyVideoInput: (deviceId: string) => Promise<void>;
  storedId: (key: 'audioinput' | 'audiooutput' | 'videoinput') => string;
  supportsAudioOutput: () => boolean;
}

const DEVICE_GAP = 8;
const VIEWPORT_PAD = 8;

export function labelForListedDevice(device: MediaDeviceInfo, fallback: string, index: number): string {
  return device.label.trim() || `${fallback} ${index + 1}`;
}

export function optionsFromDevices(devices: MediaDeviceInfo[], fallback: string): DeviceOption[] {
  return devices
    .filter((device) => device.deviceId)
    .map((device, index) => ({
      id: device.deviceId,
      label: labelForListedDevice(device, fallback, index),
    }));
}

export function placeDeviceMenu(
  trigger: DOMRect,
  menuSize: { width: number; height: number },
  viewport: { width: number; height: number } = { width: window.innerWidth, height: window.innerHeight }
): { left: number; top: number } {
  const width = Math.min(menuSize.width, viewport.width - VIEWPORT_PAD * 2);
  let left = trigger.right - width;
  left = Math.min(Math.max(VIEWPORT_PAD, left), viewport.width - width - VIEWPORT_PAD);
  const aboveTop = trigger.top - DEVICE_GAP - menuSize.height;
  const belowTop = trigger.bottom + DEVICE_GAP;
  const top =
    aboveTop >= VIEWPORT_PAD || belowTop + menuSize.height > viewport.height - VIEWPORT_PAD
      ? Math.max(VIEWPORT_PAD, aboveTop)
      : belowTop;
  return { left, top };
}

export function setupDeviceMenu(
  elements: {
    audioCaret: HTMLButtonElement;
    videoCaret: HTMLButtonElement;
    menu: HTMLElement;
    title: HTMLElement;
    body: HTMLElement;
  },
  handlers: DeviceMenuHandlers
) {
  const { audioCaret, videoCaret, menu, title, body } = elements;
  let openKind: DeviceMenuKind | null = null;
  let activeTrigger: HTMLButtonElement | null = null;

  function setOpen(kind: DeviceMenuKind | null, trigger?: HTMLButtonElement) {
    openKind = kind;
    activeTrigger = kind === null ? null : trigger ?? (kind === 'audio' ? audioCaret : videoCaret);
    const open = kind !== null;
    menu.hidden = !open;
    menu.classList.toggle('placed', false);
    audioCaret.setAttribute('aria-expanded', kind === 'audio' ? 'true' : 'false');
    videoCaret.setAttribute('aria-expanded', kind === 'camera' ? 'true' : 'false');
    if (!open) {
      body.replaceChildren();
      return;
    }
    void render(kind);
  }

  function close(restoreFocus = true) {
    const trigger = activeTrigger;
    setOpen(null);
    if (!restoreFocus) return;
    requestAnimationFrame(() => trigger?.focus());
  }

  const cleanupDismissibleLayer = installDismissibleLayer({
    isOpen: () => openKind !== null,
    getInsideNodes: () => [menu, audioCaret, videoCaret],
    getPopupNodes: () => [menu],
    getOpener: () => activeTrigger,
    onDismiss: () => close(false)
  });

  async function render(kind: DeviceMenuKind) {
    title.textContent = kind === 'audio' ? 'Audio' : 'Camera';
    menu.setAttribute('aria-label', kind === 'audio' ? 'Audio devices' : 'Camera devices');
    body.replaceChildren();
    const loading = document.createElement('p');
    loading.className = 'device-note';
    loading.textContent = 'Loading devices…';
    body.append(loading);

    try {
      if (kind === 'audio') {
        const [mics, speakers] = await Promise.all([
          handlers.list('audioinput'),
          handlers.supportsAudioOutput() ? handlers.list('audiooutput') : Promise.resolve([]),
        ]);
        if (openKind !== 'audio') return;
        body.replaceChildren();
        body.append(
          field('Microphone', optionsFromDevices(mics, 'Microphone'), handlers.storedId('audioinput'), (id) =>
            handlers.applyAudioInput(id)
          )
        );
        if (handlers.supportsAudioOutput()) {
          body.append(
            field('Speaker', optionsFromDevices(speakers, 'Speaker'), handlers.storedId('audiooutput'), (id) =>
              handlers.applyAudioOutput(id)
            )
          );
        }
      } else {
        const cameras = await handlers.list('videoinput');
        if (openKind !== 'camera') return;
        body.replaceChildren();
        body.append(
          field(
            'Camera',
            optionsFromDevices(cameras, 'Camera'),
            handlers.storedId('videoinput'),
            (id) => handlers.applyVideoInput(id),
            false
          )
        );
      }
    } catch (error) {
      if (openKind !== kind) return;
      body.replaceChildren();
      const note = document.createElement('p');
      note.className = 'device-note error';
      note.textContent = `Could not list devices: ${(error as Error).message ?? error}`;
      body.append(note);
    }
    place(kind === 'audio' ? audioCaret : videoCaret);
  }

  function field(
    label: string,
    options: DeviceOption[],
    selectedId: string,
    onSelect: (id: string) => Promise<void>,
    showFieldLabel = true
  ): HTMLElement {
    const wrap = document.createElement('div');
    wrap.className = 'device-field';
    if (showFieldLabel) {
      const heading = document.createElement('span');
      heading.className = 'device-field-label';
      heading.textContent = label;
      wrap.append(heading);
    }
    if (options.length === 0) {
      const empty = document.createElement('p');
      empty.className = 'device-note';
      empty.textContent = `No ${label.toLowerCase()}s found`;
      wrap.append(empty);
      return wrap;
    }
    const list = document.createElement('div');
    list.className = 'device-option-list';
    list.setAttribute('role', 'listbox');
    list.setAttribute('aria-label', `${label} devices`);
    const selected = options.some((option) => option.id === selectedId) ? selectedId : options[0]?.id ?? '';
    const status = document.createElement('p');
    status.className = 'device-note device-status';
    status.setAttribute('role', 'status');
    status.setAttribute('aria-live', 'polite');

    const updateSelected = (id: string) => {
      for (const item of Array.from(list.querySelectorAll<HTMLButtonElement>('.device-option'))) {
        const isSelected = item.dataset.deviceId === id;
        item.classList.toggle('selected', isSelected);
        item.setAttribute('aria-selected', isSelected ? 'true' : 'false');
        const label = item.firstElementChild?.textContent?.trim() ?? '';
        item.setAttribute('aria-label', `${label}${isSelected ? ', selected' : ''}`);
        const check = item.querySelector<HTMLElement>('.device-option-check');
        if (check) check.hidden = !isSelected;
      }
    };

    for (const [index, option] of options.entries()) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'device-option';
      button.dataset.deviceId = option.id;
      button.setAttribute('role', 'option');
      button.setAttribute('aria-selected', option.id === selected ? 'true' : 'false');
      button.setAttribute('aria-label', `${option.label}${option.id === selected ? ', selected' : ''}`);
      if (option.id === selected) button.classList.add('selected');
      const name = document.createElement('span');
      name.textContent = option.label;
      button.append(name);
      const check = document.createElement('span');
      check.className = 'device-option-check';
      check.hidden = option.id !== selected;
      check.setAttribute('aria-hidden', 'true');
      check.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 4 4L19 6"></path></svg>';
      button.append(check);
      button.addEventListener('keydown', (event) => {
        if (event.key === 'Escape') {
          event.preventDefault();
          close();
          return;
        }
        if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
        const buttons = Array.from(list.querySelectorAll<HTMLButtonElement>('.device-option'));
        if (buttons.length === 0) return;
        event.preventDefault();
        const next = event.key === 'Home'
          ? 0
          : event.key === 'End'
            ? buttons.length - 1
            : (index + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length;
        buttons[next]?.focus();
      });
      button.addEventListener('click', () => {
        updateSelected(option.id);
        status.classList.remove('error');
        status.textContent = 'Switching…';
        void onSelect(option.id)
          .then(() => {
            status.textContent = `Switched ${label.toLowerCase()}`;
          })
          .catch(() => {
            status.textContent = `Could not switch ${label.toLowerCase()}`;
            status.classList.add('error');
          });
      });
      list.append(button);
    }
    wrap.append(list, status);
    return wrap;
  }

  function place(trigger: HTMLElement) {
    const rect = trigger.getBoundingClientRect();
    const size = { width: menu.offsetWidth || 240, height: menu.offsetHeight || 160 };
    const { left, top } = placeDeviceMenu(rect, size);
    menu.style.left = `${left}px`;
    menu.style.top = `${top}px`;
    menu.classList.add('placed');
  }

  audioCaret.addEventListener('click', (event) => {
    event.preventDefault();
    event.stopPropagation();
    if (openKind === 'audio') close();
    else setOpen('audio', audioCaret);
  });
  videoCaret.addEventListener('click', (event) => {
    event.preventDefault();
    event.stopPropagation();
    if (openKind === 'camera') close();
    else setOpen('camera', videoCaret);
  });
  window.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && openKind) {
      event.preventDefault();
      close();
    }
  });

  return { close, isOpen: () => openKind, destroy: cleanupDismissibleLayer };
}
