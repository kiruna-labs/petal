import assert from 'node:assert/strict';
import test from 'node:test';

import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';

class FakeNode {
  readonly children: FakeNode[] = [];
  isConnected = true;
  focused = false;

  readonly owner: FakeDocument;

  constructor(owner: FakeDocument) {
    this.owner = owner;
  }

  append(child: FakeNode): void {
    this.children.push(child);
  }

  contains(target: FakeNode): boolean {
    return target === this || this.children.some((child) => child.contains(target));
  }

  focus(): void {
    this.owner.activeElement = this;
    this.focused = true;
  }
}

class FakeDocument {
  readonly body = new FakeNode(this);
  activeElement: FakeNode | null = this.body;
  private readonly listeners = new Map<string, EventListener[]>();

  node(): FakeNode {
    return new FakeNode(this);
  }

  addEventListener(type: string, listener: EventListener): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }

  removeEventListener(type: string, listener: EventListener): void {
    this.listeners.set(type, (this.listeners.get(type) ?? []).filter((entry) => entry !== listener));
  }

  dispatchPointer(target: FakeNode, composedPath?: FakeNode[]): void {
    const event = {
      target,
      composedPath: composedPath ? () => composedPath : undefined
    } as unknown as PointerEvent;
    for (const listener of this.listeners.get('pointerdown') ?? []) listener(event);
  }
}

const asDocument = (document: FakeDocument): Document => document as unknown as Document;
const asNode = (node: FakeNode): Node => node as unknown as Node;
const asElement = (node: FakeNode): HTMLElement => node as unknown as HTMLElement;

test('dismissible layers ignore popup descendants and current/sibling triggers', () => {
  const document = new FakeDocument();
  const popup = document.node();
  const popupChild = document.node();
  popup.append(popupChild);
  const currentTrigger = document.node();
  const siblingTrigger = document.node();
  let open = true;
  let dismissals = 0;

  const cleanup = installDismissibleLayer({
    document: asDocument(document),
    isOpen: () => open,
    getInsideNodes: () => [asNode(popup), asNode(currentTrigger), asNode(siblingTrigger)],
    getPopupNodes: () => [asNode(popup)],
    onDismiss: () => dismissals += 1,
    getOpener: () => asElement(currentTrigger)
  });

  document.dispatchPointer(popupChild, [popupChild, popup]);
  document.dispatchPointer(siblingTrigger, [siblingTrigger]);
  assert.equal(dismissals, 0);
  cleanup();
});

test('outside action dismissal preserves the clicked action focus', async () => {
  const document = new FakeDocument();
  const popup = document.node();
  const opener = document.node();
  const action = document.node();
  let dismissals = 0;

  const cleanup = installDismissibleLayer({
    document: asDocument(document),
    isOpen: () => true,
    getInsideNodes: () => [asNode(popup), asNode(opener)],
    getPopupNodes: () => [asNode(popup)],
    onDismiss: () => dismissals += 1,
    getOpener: () => asElement(opener)
  });

  document.dispatchPointer(action, [action]);
  action.focus();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(dismissals, 1);
  assert.equal(document.activeElement, action);
  assert.equal(opener.focused, false);
  cleanup();
});

test('blank dismissal restores the opener when popup focus is stranded', async () => {
  const document = new FakeDocument();
  const popup = document.node();
  const popupItem = document.node();
  popup.append(popupItem);
  const opener = document.node();
  const blank = document.node();
  popupItem.focus();

  const cleanup = installDismissibleLayer({
    document: asDocument(document),
    isOpen: () => true,
    getInsideNodes: () => [asNode(popup), asNode(opener)],
    getPopupNodes: () => [asNode(popup)],
    onDismiss: () => {
      document.activeElement = document.body;
    },
    getOpener: () => asElement(opener)
  });

  document.dispatchPointer(blank, [blank]);
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(document.activeElement, opener);
  cleanup();
});

test('dismissible layer supports target fallback, inactive state, and cleanup', async () => {
  const document = new FakeDocument();
  const popup = document.node();
  const child = document.node();
  popup.append(child);
  let open = false;
  let dismissals = 0;
  const cleanup = installDismissibleLayer({
    document: asDocument(document),
    isOpen: () => open,
    getInsideNodes: () => [asNode(popup)],
    onDismiss: () => dismissals += 1
  });

  document.dispatchPointer(child);
  open = true;
  document.dispatchPointer(child);
  assert.equal(dismissals, 0);
  document.dispatchPointer(document.node());
  assert.equal(dismissals, 1);
  cleanup();
  document.dispatchPointer(document.node());
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(dismissals, 1);
});
