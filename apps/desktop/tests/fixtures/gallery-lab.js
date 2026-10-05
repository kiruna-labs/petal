// Mounts the native gallery layout lab ($lib/dev/GalleryLabStage.svelte: the
// real MeetingChrome -> Gallery) for scripts/verify-native-gallery-matrix.mjs
// and tests/galleryLabRendered.test.ts. The scenario comes from the URL hash
// (galleryLab.ts scenarioFromQuery); `window.__galleryLab.set(query)` swaps
// it in place and resolves with the settled reading and the rules it breaks
// (galleryLabMeasure.ts judgeGalleryLab).
import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';
import '@fontsource/albert-sans/700.css';
import '@fontsource/jetbrains-mono/500.css';
import { mount, tick } from 'svelte';
import Lab from './gallery-lab.svelte';
import { scenarioFromQuery } from '$lib/dev/galleryLab';
import { judgeGalleryLab, measureGalleryLab } from '$lib/dev/galleryLabMeasure';

const frames = () => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));

let settledGeneration = -1;
const lab = mount(Lab, {
  target: document.querySelector('#app'),
  props: {
    initial: scenarioFromQuery(location.hash),
    onSettled: (generation) => {
      settledGeneration = generation;
    }
  }
});

async function settle() {
  await tick();
  await document.fonts.ready;
  // Video frames, ResizeObservers and the gallery's own FLIP (<= 400 ms).
  const deadline = performance.now() + 3000;
  for (;;) {
    await frames();
    const pending = Array.from(document.querySelectorAll('video.video-el')).filter((v) => !v.classList.contains('ready'));
    if (pending.length === 0 || performance.now() > deadline) break;
  }
  await new Promise((resolve) => setTimeout(resolve, 450));
  await frames();
}

window.__galleryLab = {
  /** Show `query` and resolve with its reading and the rules it breaks. */
  async set(query) {
    const scenario = scenarioFromQuery(query);
    lab.set(scenario);
    await settle();
    const reading = this.measure();
    return { reading, problems: judgeGalleryLab(scenario, reading) };
  },
  measure() {
    const windowEl = document.querySelector('[data-lab-window]');
    return measureGalleryLab(windowEl);
  },
  settledGeneration: () => settledGeneration
};

settle().then(() => {
  document.body.dataset.ready = 'true';
});
