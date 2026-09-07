// One place that routes every inbound LiveKit data packet by topic. Exact
// topics register with `on`; the plugin bus registers the `plugin/` prefix.
// Replaces the if/else chain connection.ts carried for each feature, so a new
// consumer (or a plugin) never edits the connection seam again.
// Design: plugins/README.md §2.6.

import type { RemoteParticipant } from 'livekit-client';

export type TopicHandler = (
  payload: Uint8Array,
  participant: RemoteParticipant | undefined,
  topic: string,
  /** `participant?.identity`, or the SFU-attributed sender when the participant object is absent. */
  senderIdentity: string | undefined,
) => void;

export interface TopicDispatcher {
  on(topic: string, handler: TopicHandler): void;
  onPrefix(prefix: string, handler: TopicHandler): void;
  dispatch(payload: Uint8Array, participant: RemoteParticipant | undefined, topic: string | undefined, senderIdentity?: string): boolean;
}

export function createTopicDispatcher(onUnknown?: (topic: string) => void): TopicDispatcher {
  const exact = new Map<string, TopicHandler[]>();
  const prefixes: Array<{ prefix: string; handler: TopicHandler }> = [];
  const warned = new Set<string>();
  return {
    on(topic, handler) {
      const list = exact.get(topic) ?? [];
      list.push(handler);
      exact.set(topic, list);
    },
    onPrefix(prefix, handler) {
      prefixes.push({ prefix, handler });
    },
    dispatch(payload, participant, topic, senderIdentity = participant?.identity) {
      if (topic === undefined) return false;
      let handled = false;
      for (const handler of exact.get(topic) ?? []) {
        handled = true;
        handler(payload, participant, topic, senderIdentity);
      }
      for (const entry of prefixes) {
        if (topic.startsWith(entry.prefix)) {
          handled = true;
          entry.handler(payload, participant, topic, senderIdentity);
        }
      }
      if (!handled && onUnknown && !warned.has(topic)) {
        warned.add(topic);
        onUnknown(topic);
      }
      return handled;
    },
  };
}
