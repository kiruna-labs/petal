import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';

const search = new URLSearchParams(window.location.search);

function installIpcProbe() {
  const callbacks = new Map();
  const listeners = new Map();
  let nextCallbackId = 1;
  let currentPollBatch = 0;
  const geometry = {
    position: { x: 0, y: 0 },
    size: { width: 640, height: 400 }
  };
  const cursorX = Number(search.get('cursorX'));
  const cursorY = Number(search.get('cursorY'));
  const placementActive = search.get('placing') === '1';
  const probe = {
    delayMs: Number(search.get('ipcDelayMs') || 260),
    shareDelayMs: Number(search.get('shareDelayMs') || 0),
    cursorOverride: Number.isFinite(cursorX) && Number.isFinite(cursorY) ? { x: cursorX, y: cursorY } : null,
    placementActive,
    placementSettled: false,
    earlyClickThroughRequests: 0,
    clickThroughRequests: [],
    placementSettlementLabels: [],
    commandHistory: [],
    shareActive: false,
    shareInvocations: 0,
    inFlight: 0,
    maxInFlight: 0,
    pollCalls: 0,
    pollCallsAfterUnmount: 0,
    unmounted: false,
    pollBatchesInFlight: 0,
    maxPollBatchesInFlight: 0,
    appliedIgnoreStates: [],
    staleApplyCount: 0,
    latestDesiredIgnoreState: false,
    listenerCount: 0,
    eventCounts: {},
    eventDeliveries: {},
    cursorHistory: [],
    geometry,
    emit(event, payload) {
      this.eventCounts[event] = (this.eventCounts[event] || 0) + 1;
      if (event === 'region-placement-settled') {
        const selectorLabel = payload?.selectorLabel ?? null;
        this.placementSettlementLabels.push(selectorLabel);
        if (selectorLabel === 'region-window-1') this.placementSettled = true;
      }
      const eventListeners = listeners.get(event) || [];
      this.eventDeliveries[event] = (this.eventDeliveries[event] || 0) + eventListeners.size;
      for (const callbackId of eventListeners) {
        callbacks.get(callbackId)?.({ event, id: callbackId, payload });
      }
    },
    settlePlacement(selectorLabel = 'region-window-1') {
      // Set the probe's native-state mirror before dispatching callbacks. The
      // real worker has already consumed the left-button edge at this point;
      // callbacks must never observe a pre-settlement indicator state.
      if (selectorLabel === 'region-window-1') {
        this.placementSettled = true;
        this.placementActive = false;
      }
      this.emit('region-placement-settled', { selectorLabel });
    },
    releasePlacement(selectorLabel = 'region-window-1') {
      if (selectorLabel === 'region-window-1') this.placementActive = false;
      this.emit('region-placement-released', { selectorLabel });
    },
    setCursor(position) {
      this.cursorOverride = { ...position };
    },
    setGeometry(position, size) {
      if (position) this.geometry.position = { ...position };
      if (size) this.geometry.size = { ...size };
      this.emit('tauri://move', this.geometry.position);
      this.emit('tauri://resize', this.geometry.size);
    },
    setScale(scaleFactor, size) {
      if (size) this.geometry.size = { ...size };
      this.emit('tauri://scale-change', { scaleFactor, size: this.geometry.size });
    },
    resetTransitions() {
      this.appliedIgnoreStates.length = 0;
      this.staleApplyCount = 0;
      this.earlyClickThroughRequests = 0;
      this.clickThroughRequests.length = 0;
    }
  };
  Object.defineProperty(window, '__regionIpcProbe', { value: probe, configurable: true });

  function registerCallback(callback) {
    const id = nextCallbackId++;
    callbacks.set(id, callback);
    return id;
  }

  function forgetCallback(id) {
    callbacks.delete(id);
    for (const eventListeners of listeners.values()) eventListeners.delete(id);
  }

  window.__TAURI_INTERNALS__ = {
    metadata: {
      currentWindow: { label: 'region-window-1' },
      currentWebview: { windowLabel: 'region-window-1', label: 'region-window-1' }
    },
    transformCallback: registerCallback,
    unregisterCallback: forgetCallback,
    callbacks,
    runCallback(id, data) {
      callbacks.get(id)?.(data);
    },
    async invoke(command, args) {
      probe.commandHistory.push(command);
      if (command === 'plugin:event|listen') {
        const eventListeners = listeners.get(args.event) || new Set();
        eventListeners.add(args.handler);
        listeners.set(args.event, eventListeners);
        probe.listenerCount = [...listeners.values()].reduce((count, set) => count + set.size, 0);
        return args.handler;
      }
      if (command === 'plugin:event|unlisten') {
        const eventListeners = listeners.get(args.event);
        eventListeners?.delete(args.eventId);
        probe.listenerCount = [...listeners.values()].reduce((count, set) => count + set.size, 0);
        return null;
      }
      if (command === 'plugin:window|title') return 'Petal View: Probe #1';
      if (command === 'region_placement_active') return probe.placementActive;
      if (command === 'region_share_state') return { active: probe.shareActive };
      if (command === 'toggle_region_share') {
        probe.shareInvocations += 1;
        await new Promise((resolve) => setTimeout(resolve, probe.shareDelayMs));
        probe.shareActive = !probe.shareActive;
        probe.emit('region-share-state-changed', {
          windowId: 1,
          selectorLabel: 'region-window-1',
          active: probe.shareActive
        });
        return probe.shareActive;
      }
      if (command === 'plugin:window|set_ignore_cursor_events') {
        const applied = Boolean(args.value);
        // Count at request time, not completion time. A delayed native setter
        // must still fail if the route asked for click-through before the
        // placement-settled event arrived.
        const beforePlacementSettlement = probe.placementActive && !probe.placementSettled;
        probe.clickThroughRequests.push({
          applied,
          beforePlacementSettlement,
          placementSettledAtRequest: probe.placementSettled,
          placementSettlementLabelsAtRequest: [...probe.placementSettlementLabels]
        });
        if (beforePlacementSettlement && applied) {
          probe.earlyClickThroughRequests += 1;
        }
        await new Promise((resolve) => setTimeout(resolve, 5));
        if (probe.latestDesiredIgnoreState !== applied) probe.staleApplyCount += 1;
        probe.appliedIgnoreStates.push(applied);
        return null;
      }

      const isPollCommand =
        command === 'plugin:window|cursor_position' ||
        command === 'plugin:window|outer_position' ||
        command === 'plugin:window|outer_size';
      if (!isPollCommand) return null;

      if (command === 'plugin:window|cursor_position') {
        if (probe.unmounted) probe.pollCallsAfterUnmount += 1;
        currentPollBatch = ++probe.pollCalls;
        probe.pollBatchesInFlight += 1;
        probe.maxPollBatchesInFlight = Math.max(
          probe.maxPollBatchesInFlight,
          probe.pollBatchesInFlight
        );
      }
      probe.inFlight += 1;
      probe.maxInFlight = Math.max(probe.maxInFlight, probe.inFlight);
      const pollBatch = currentPollBatch;
      const delay = pollBatch === 1 ? probe.delayMs : 5;
      await new Promise((resolve) => setTimeout(resolve, delay));
      probe.inFlight -= 1;
      if (command === 'plugin:window|cursor_position') {
        probe.pollBatchesInFlight -= 1;
        // The first batch is intentionally slow and says "center"; newer
        // batches say "edge". The old route lets the slow result apply last.
        const cursor = probe.cursorOverride || (pollBatch === 1 ? { x: 320, y: 200 } : { x: 0, y: 200 });
        probe.latestDesiredIgnoreState = probe.placementActive && !probe.placementSettled
          ? false
          : cursor.x !== 0;
        probe.cursorHistory.push({ ...cursor });
        return cursor;
      }
      if (command === 'plugin:window|outer_position') return { ...probe.geometry.position };
      return { ...probe.geometry.size };
    }
  };
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
    unregisterListener(event, eventId) {
      const eventListeners = listeners.get(event);
      eventListeners?.delete(eventId);
      probe.listenerCount = [...listeners.values()].reduce((count, set) => count + set.size, 0);
    }
  };
}

if (search.has('ipcProbe')) installIpcProbe();

async function renderFixture() {
  try {
    const [{ mount, unmount }, { default: RegionWindow }] = await Promise.all([
      import('svelte'),
      import('$lib/../routes/region-window/+page.svelte')
    ]);
    const mounted = mount(RegionWindow, { target: document.querySelector('#app') });
    if (new URLSearchParams(window.location.search).has('ipcProbe')) {
      window.__unmountRegion = () => {
        window.__regionIpcProbe.unmounted = true;
        return unmount(mounted);
      };
    }
    await document.fonts.ready;
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    const frame = document.querySelector('.hollow-frame');
    const close = document.querySelector('.close-button');
    const share = document.querySelector('[data-region-share-control]');
    const backdrop = document.querySelector('#desktop-backdrop');
    if (!frame || !close || !share || !backdrop) {
      throw new Error('Petal View region selector or persistent Share control did not render');
    }
    document.body.dataset.regionRendered = '1';
  } catch (error) {
    document.body.dataset.regionRenderedError = encodeURIComponent(
      error instanceof Error ? `${error.message}\n${error.stack}` : String(error)
    );
  }
}

void renderFixture();
