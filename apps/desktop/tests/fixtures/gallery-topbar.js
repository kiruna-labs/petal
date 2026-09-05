// #786 added a third control cell to the gallery topbar's right cluster (the
// bug-report button, between the layout toggle and Connection stats), making
// that cluster ~40px wider. Petal's hard rule is that user-facing text never
// truncates, and the topbar competes for a main window that goes down to
// 380px -- so measure the REAL rendered pixels of the REAL component rather
// than reasoning about the CSS.
//
// Mounts `Gallery.svelte` itself (it is presentational -- no Tauri imports),
// so this measures what actually ships, not a replica of it.
import '../../src/styles/app.css';
import '@fontsource/albert-sans/400.css';
import '@fontsource/albert-sans/500.css';
import '@fontsource/albert-sans/600.css';

async function renderFixture() {
  try {
    const [{ mount }, { default: Gallery }] = await Promise.all([
      import('svelte'),
      import('$lib/components/Gallery.svelte')
    ]);

    const host = document.querySelector('#app');
    host.style.width = '100%';
    host.style.height = '100vh';

    const motionParticipants = window.location.hash === '#motion'
      ? [
          { id: 'ada', name: 'Ada Lovelace', videoOn: true, videoStream: new MediaStream() },
          { id: 'grace', name: 'Grace Hopper', videoOn: true, videoStream: new MediaStream() },
          { id: 'alan', name: 'Alan Turing', videoOn: true, videoStream: new MediaStream() }
        ]
      : [];

    mount(Gallery, {
      target: host,
      props: {
        // A long-but-plausible room name: the room title is the element that
        // yields space to the right cluster, so a short name would hide the
        // very regression this fixture exists to catch.
        roomName: 'cs-allstars-weekly-sync',
        elapsed: '24:18',
        participants: motionParticipants,
        frameless: true,
        // Providing the handler is what makes the #786 cell render at all.
        onReportBug: () => {},
        onOpenNetwork: () => {},
        onControl: () => {}
      }
    });

    await document.fonts.ready;
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));

    const topbar = document.querySelector('.topbar');
    if (!topbar) throw new Error('gallery topbar did not render');
    const right = document.querySelector('.topbar-right');
    if (!right) throw new Error('gallery topbar-right did not render');
    const reportBug = document.querySelector('.report-bug');
    if (!reportBug) throw new Error('#786 report-bug control did not render');

    // Truncation is content overflowing a box that ACTUALLY CLIPS. A bare
    // `scrollWidth > clientWidth` is not sufficient and produces false
    // positives here: `.topbar-tooltip` is `position: absolute` and is
    // designed to exceed its 32px `.topbar-control-cell`, and neither sets
    // `overflow: hidden`, so the content paints outside and stays fully
    // readable. Only flag an element that both overflows AND clips (or
    // ellipsizes).
    //
    const roomName = document.querySelector('.room-name');
    const clipped = [];
    for (const element of topbar.querySelectorAll('*')) {
      if (!(element instanceof HTMLElement)) continue;
      if (element.scrollWidth <= element.clientWidth + 1) continue;
      const style = getComputedStyle(element);
      const clipsHorizontally = ['hidden', 'clip', 'auto', 'scroll'].includes(style.overflowX);
      const ellipsizes = style.textOverflow === 'ellipsis';
      if (!clipsHorizontally && !ellipsizes) continue;
      clipped.push({
        selector: element.className?.toString?.() || element.tagName.toLowerCase(),
        text: (element.textContent || '').trim().slice(0, 40),
        overflowX: style.overflowX,
        textOverflow: style.textOverflow,
        scrollWidth: element.scrollWidth,
        clientWidth: element.clientWidth
      });
    }

    const topbarRect = topbar.getBoundingClientRect();
    const rightRect = right.getBoundingClientRect();
    const bugRect = reportBug.getBoundingClientRect();
    const roomRect = roomName?.getBoundingClientRect();

    document.body.dataset.galleryTopbarMeasurement = encodeURIComponent(JSON.stringify({
      viewport: window.innerWidth,
      topbar: {
        width: topbarRect.width,
        scrollWidth: topbar.scrollWidth,
        clientWidth: topbar.clientWidth
      },
      // The authoritative "does the chrome overflow the window" signal: an
      // in-flow cluster grown too wide pushes the document itself wider than
      // the viewport, which no per-element check can see.
      document: {
        scrollWidth: document.documentElement.scrollWidth,
        clientWidth: document.documentElement.clientWidth
      },
      rightCluster: {
        left: rightRect.left,
        right: rightRect.right,
        width: rightRect.width
      },
      reportBug: {
        width: bugRect.width,
        height: bugRect.height,
        left: bugRect.left,
        right: bugRect.right,
        // Inside the viewport on both edges = actually reachable, not merely
        // present in the DOM.
        visible: bugRect.width > 0 && bugRect.height > 0
          && bugRect.left >= 0 && bugRect.right <= window.innerWidth + 1
      },
      roomName: roomRect
        ? {
            width: roomRect.width,
            scrollWidth: roomName.scrollWidth,
            clientWidth: roomName.clientWidth,
            ellipsized: roomName.scrollWidth > roomName.clientWidth + 1
          }
        : null,
      clipped
    }));
  } catch (error) {
    document.body.dataset.galleryTopbarMeasurementError = encodeURIComponent(
      error instanceof Error ? error.message : String(error)
    );
  }
}

void renderFixture();
