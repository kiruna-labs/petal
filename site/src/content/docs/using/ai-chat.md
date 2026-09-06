---
title: AI chat on a shared window
description: Talking to an AI about a window someone is sharing, and what it can see.
---

AI chat lets anyone in a meeting open a live conversation with an AI model
(Google's Gemini Live) about a window that is being shared. The model sees
the shared window and hears the room while a session is live, answers by
voice and text, and — only with the sharer's explicit approval — can act on
the window.

It is **off by default** and is an opt-in feature with real privacy
consequences, so read this page before turning it on.

## Turning it on

**Settings → AI chat → AI chat on shared windows.** Flipping the switch on
shows the two consequences below and asks you to confirm with **Turn on AI
chat**. Once on, every shared window in your meetings — the ones you share
and the ones other people share — gets an **AI chat** button in its header,
and a matching entry in each shared window's hover-tab and Petal View options
menus.

Turning it on also means **anyone in your meetings can start AI chat on a
window you share**. While a session is live, that window's content and the
room's voice are sent to Google.

By default Petal's hosted service supplies the AI credentials, with modest
limits: a session token is single-use, must connect within 30 seconds, and
sessions are capped to a few per person per room per hour. If you'd rather
bill your own Google account, paste a key under **Gemini API key
(optional)** (the field appears once AI chat is on) — roughly 2–4¢ per
minute of AI chat. Free-tier keys may allow Google to use content to improve
their models.

## Starting a session

1. On a shared window, click **AI chat** in its header (viewer side) or
   choose **Start AI chat on this window** from the sharer's hover-tab menu.
2. A floating **AI chat** panel opens beside the window with the transcript,
   a text box (**Type a message…** / **Send**), a push-to-talk button, and
   the time left in the session.
3. Everyone viewing the window sees an **AI chat live** badge on its header
   for the whole session — "AI chat is live. This window and room voice are
   sent to Google." — with their own push-to-talk button, so anyone can
   speak to the model.

Push-to-talk is deliberate: the model only hears the room while someone is
holding the button ("Listening — release to send"), never when humans are
just talking amongst themselves. If someone else already has the floor, your
talk attempt is refused with a note.

The session runs on the **sharer's** machine (that's where the pixels are),
regardless of who started it, and stops when anyone clicks **Stop AI chat on
this window**, the share ends, the time limit is reached, or the sharer
disables AI chat.

## Letting the AI act on the window

Answering questions never touches the window. If the model wants to click,
type, or scroll — for example, "open the second tab" — Petal stops and asks
the **sharer**: **The AI wants to act on this window**, with **Allow once**,
**Allow for this session**, and **Reject**.

- **Allow once** performs that one action.
- **Allow for this session** gives the AI standing access to this window
  until the session ends; the panel shows "The AI has standing access to this
  window for this session" with a **Revoke access** button.
- **Reject** turns window control off for the rest of the session ("Window
  control is off for the rest of this session"); **Allow the AI to ask
  again** re-enables the prompt.

Only the sharer ever sees this prompt, and nothing the AI does bypasses the
same Accessibility-based injection that human remote control uses.

## If something goes wrong

The panel and header tell you plainly: "AI chat reached its time limit",
"Too many AI chat sessions just now. Try again shortly.", "The AI chat quota
for this key is used up", "This AI model is unavailable — update Petal", or
"Could not reach the AI chat service." A message that fails to send stays in
the box so you can retry.

## Browser participants

The browser client shows the **AI chat** button and the live badge on shared
tiles too, so browser participants can start sessions and talk to the model.
The session itself always runs on the desktop app that is sharing the window.
