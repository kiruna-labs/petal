// #613: captureStream must read the real test canvas from a top-level, visible box.
export const PRESENTATION_SOURCE_HOST_ID = 'p613-presentation-source-host';
export const PRESENTATION_SOURCE_CLASS = 'p613-presentation-source';

export class PresentationSourceHost {
  private host: HTMLDivElement | null = null;
  private originalParent: Node | null = null;
  private originalNextSibling: Node | null = null;
  private readonly canvas: HTMLCanvasElement;
  private readonly document: Document;

  constructor(canvas: HTMLCanvasElement, document: Document = window.document) {
    this.canvas = canvas;
    this.document = document;
  }

  mount(): HTMLDivElement {
    this.unmount();
    if (!this.canvas.parentNode) throw new Error('test-pattern canvas has no parent to restore');
    if (this.document.getElementById(PRESENTATION_SOURCE_HOST_ID)) throw new Error('presentation source host already exists');

    this.originalParent = this.canvas.parentNode;
    this.originalNextSibling = this.canvas.nextSibling;
    const host = this.document.createElement('div');
    host.id = PRESENTATION_SOURCE_HOST_ID;
    Object.assign(host.style, {
      position: 'fixed', left: '0px', top: '0px', zIndex: '2147483647',
      display: 'block', visibility: 'visible', opacity: '1', width: '640px', height: '360px',
      overflow: 'visible', pointerEvents: 'none',
    });
    this.document.body.appendChild(host);
    host.appendChild(this.canvas);
    this.canvas.classList.add(PRESENTATION_SOURCE_CLASS);
    this.host = host;
    return host;
  }

  unmount(): void {
    this.canvas.classList.remove(PRESENTATION_SOURCE_CLASS);
    if (this.host && this.originalParent?.isConnected) {
      const nextSibling = this.originalNextSibling?.parentNode === this.originalParent ? this.originalNextSibling : null;
      this.originalParent.insertBefore(this.canvas, nextSibling);
    } else if (this.host?.contains(this.canvas)) {
      this.canvas.remove();
    }
    this.host?.remove();
    this.host = null;
    this.originalParent = null;
    this.originalNextSibling = null;
  }
}
