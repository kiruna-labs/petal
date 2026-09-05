// Render the real MeetingChrome so selector tests exercise the expanded-gallery
// reactive lifecycle instead of a copied button/menu fragment.
// The long deterministic lists make the narrow viewport case exercise the
// picker’s real constrained-scroll path rather than its loading/error state.
const audioDevices = {
  recording: Array.from({ length: 8 }, (_, index) => ({
    id: `microphone-${index}`,
    name: `Microphone ${index + 1}`
  })),
  playout: Array.from({ length: 8 }, (_, index) => ({
    id: `speaker-${index}`,
    name: `Speaker ${index + 1}`
  }))
};
const cameraDevices = Array.from({ length: 8 }, (_, index) => ({
  id: `camera-${index}`,
  name: `Camera ${index + 1}`
}));

// @tauri-apps/api/core delegates directly to this bridge. Keep the fixture
// browser-only while exercising the same data-client path as the app.
window.__TAURI_INTERNALS__ = {
  invoke: async (command) => {
    if (command === 'list_audio_devices') return audioDevices;
    if (command === 'list_camera_devices') return cameraDevices;
    if (command === 'set_audio_devices') {
      return { micApplied: false, speakerApplied: false, inRoom: false };
    }
    if (command === 'set_camera_device') return { applied: false, inRoom: false };
    return null;
  },
  transformCallback: () => 0,
  unregisterCallback: () => {}
};

import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';

async function renderFixture() {
  try {
    const [{ mount }, { default: MeetingChrome }] = await Promise.all([
      import('svelte'),
      import('$lib/components/MeetingChrome.svelte')
    ]);

    const host = document.querySelector('#app');
    host.style.width = '100%';
    host.style.height = '100vh';

    window.__meetingControlActions = [];
    mount(MeetingChrome, {
      target: host,
      props: {
        roomName: 'meeting-controls-fixture',
        elapsed: '24:18',
        participants: [],
        expanded: true,
        micMuted: false,
        cameraOn: false,
        sharingActive: false,
        sharingPickerOpen: false,
        remoteControlAllowed: false,
        frameless: true,
        onControl: (icon) => window.__meetingControlActions.push(icon),
        onInviteLinkCopy: () => {},
        onOpenNetwork: () => {}
      }
    });

    await document.fonts.ready;
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    document.body.dataset.fixtureReady = 'true';
  } catch (error) {
    document.body.dataset.fixtureError = encodeURIComponent(
      error instanceof Error ? error.message : String(error)
    );
  }
}

void renderFixture();
