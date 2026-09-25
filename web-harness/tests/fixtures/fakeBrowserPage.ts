// A just-real-enough browser page for #244's history/lifecycle tests: a
// session history list with same-document traversal (popstate), page
// lifecycle events on window/document, sessionStorage, and a
// `<dialog method="dialog">` stand-in. Everything asynchronous in a real
// browser (history.back(), a dialog's close event) is a setTimeout here too.

type Listener = (event: Record<string, unknown>) => void;

class FakeTarget {
  private listeners = new Map<string, Listener[]>();

  addEventListener(type: string, listener: Listener) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }

  dispatch(type: string, event: Record<string, unknown> = {}) {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

export type HistoryCall = ['push' | 'replace', unknown, string] | ['back'];

export class FakeHistory {
  entries: Array<{ state: unknown; url: string }>;
  index = 0;
  calls: HistoryCall[] = [];
  private readonly page: FakeBrowserPage;

  constructor(page: FakeBrowserPage, url: string) {
    this.page = page;
    this.entries = [{ state: null, url }];
  }

  get state() {
    return this.entries[this.index]!.state;
  }

  get length() {
    return this.entries.length;
  }

  pushState(state: unknown, _title: string, url: string) {
    this.calls.push(['push', state, url]);
    this.entries.splice(this.index + 1);
    this.entries.push({ state: structuredClone(state), url: this.page.resolve(url) });
    this.index += 1;
    this.page.location.href = this.entries[this.index]!.url;
  }

  replaceState(state: unknown, _title: string, url: string) {
    this.calls.push(['replace', state, url]);
    this.entries[this.index] = { state: structuredClone(state), url: this.page.resolve(url) };
    this.page.location.href = this.entries[this.index]!.url;
  }

  back() {
    this.calls.push(['back']);
    this.traverse(-1);
  }

  /** The user's Back button / gesture (not a call the page made). */
  userBack() {
    this.traverse(-1);
  }

  private traverse(delta: number) {
    setTimeout(() => {
      const next = this.index + delta;
      if (next < 0 || next >= this.entries.length) return;
      this.index = next;
      this.page.location.href = this.entries[next]!.url;
      this.page.window.dispatch('popstate', { state: this.state });
    }, 0);
  }

  urls() {
    return this.entries.map((entry) => entry.url);
  }
}

export class FakeDocument extends FakeTarget {
  visibilityState: 'visible' | 'hidden' = 'visible';
}

export class MemoryStorage implements Storage {
  private values = new Map<string, string>();
  blocked = false;

  get length() {
    return this.values.size;
  }

  clear() {
    this.values.clear();
  }

  getItem(key: string) {
    return this.values.get(key) ?? null;
  }

  key(index: number) {
    return Array.from(this.values.keys())[index] ?? null;
  }

  removeItem(key: string) {
    if (this.blocked) throw new Error('SecurityError: storage is disabled');
    this.values.delete(key);
  }

  setItem(key: string, value: string) {
    if (this.blocked) throw new Error('SecurityError: storage is disabled');
    this.values.set(key, value);
  }
}

export class FakeWindow extends FakeTarget {
  readonly document: FakeDocument;
  readonly history: FakeHistory;
  readonly location: URL;
  readonly navigator: { userActivation?: { isActive: boolean } };
  readonly sessionStorage: MemoryStorage;

  constructor(page: FakeBrowserPage) {
    super();
    this.document = page.document;
    this.history = page.history;
    this.location = page.location;
    this.navigator = page.navigator;
    this.sessionStorage = page.sessionStorage;
  }
}

export class FakeBrowserPage {
  readonly location: URL;
  readonly document = new FakeDocument();
  readonly history: FakeHistory;
  readonly navigator: { userActivation?: { isActive: boolean } } = {};
  readonly sessionStorage = new MemoryStorage();
  readonly window: FakeWindow;

  constructor(url: string) {
    this.location = new URL(url);
    this.history = new FakeHistory(this, this.location.href);
    this.window = new FakeWindow(this);
  }

  resolve(url: string) {
    return new URL(url, this.location.href).href;
  }

  /** Put this page's location/history where connection.ts reads them. */
  installGlobals() {
    Object.defineProperty(globalThis, 'location', { configurable: true, value: this.location });
    Object.defineProperty(globalThis, 'history', { configurable: true, value: this.history });
  }

  hide() {
    this.document.visibilityState = 'hidden';
    this.document.dispatch('visibilitychange');
  }

  show() {
    this.document.visibilityState = 'visible';
    this.document.dispatch('visibilitychange');
  }
}

export class FakeDialog extends FakeTarget {
  open = false;
  returnValue = '';

  showModal() {
    if (this.open) throw new Error('dialog already open');
    this.open = true;
  }

  close(returnValue?: string) {
    if (!this.open) return;
    this.open = false;
    if (returnValue !== undefined) this.returnValue = returnValue;
    setTimeout(() => this.dispatch('close'), 0);
  }

  /** A click on a `method="dialog"` form button with this value. */
  answer(value: string) {
    this.close(value);
  }

  /** Escape, or the Android back gesture closing the modal. */
  cancel() {
    this.close();
  }
}

export class FakeElement extends FakeTarget {
  textContent = '';
  hidden = false;
  focused = false;
  readonly classes: Set<string>;
  readonly classList = {
    add: (name: string) => this.classes.add(name),
    remove: (name: string) => this.classes.delete(name),
    contains: (name: string) => this.classes.has(name),
    replace: (from: string, to: string) => {
      if (!this.classes.delete(from)) return false;
      this.classes.add(to);
      return true;
    },
  };

  constructor(className = '') {
    super();
    this.classes = new Set(className.split(' ').filter(Boolean));
  }

  click() {
    this.dispatch('click');
  }

  focus() {
    this.focused = true;
  }

  blur() {
    this.focused = false;
  }
}

/** Let queued history traversals, dialog closes and deferred rejoins run. */
export async function settle(rounds = 5) {
  for (let i = 0; i < rounds; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
}
