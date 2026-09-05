---
title: Invite links and the petal:// scheme
description: How to construct Petal invite links and petal:// deep links from outside the app.
---

Petal exposes two ways to point someone at a specific meeting room: a
`https://meet.petal.live/...` web link, and a `petal://join/...` desktop deep
link. Both are driven by the same **access code** — a short, human-typeable
string that is the only thing that identifies a room to the outside world.

This page documents both formats so you can generate them yourself — for a
calendar template, a Slack workflow, an internal launcher, or anything else
that needs to hand someone a working Petal link without going through the app.

## The access code

An access code is **10 lowercase letters**, conventionally written and
displayed as three hyphen-separated groups of 3-4-3:

```
abc-defg-hjk
```

Rules, as enforced by the room-credential parser (`rooms.rs::normalize_access_code`
on desktop, mirrored in `shared/logic/meetingCode.ts::normalizeAccessCode`):

- **Letters only, `a`–`z`.** No digits, no other punctuation. Case doesn't
  matter — codes are lowercased before use, so `ABC-DEFG-HJK` and
  `abc-defg-hjk` resolve to the same room.
- **Exactly 10 letters.** Hyphens are optional on input and are stripped
  before validation, so `abcdefghjk` and `abc-defg-hjk` are equivalent. When
  Petal generates or displays a code, it always uses the 3-4-3 hyphenated
  form.
- **Petal-generated codes never contain `i` or `l`** (the alphabet it draws
  from is `abcdefghjkmnopqrstuvwxyz` — 24 letters, `i` and `l` excluded as
  easy to misread). This is only a *generation-time* restriction: the
  parser itself still accepts `i`/`l` in a code someone typed by hand, it just
  never mints them. Don't rely on their absence if you're validating codes
  from an untrusted source.

An access code is a **bearer capability** — anyone who has it can join the
room, the same way anyone with a meeting password could. Petal has no
account/identity layer behind it (see the
[Backend API reference](/docs/customizing/backend-api/)), so treat
invite links and raw access codes the same way you'd treat a meeting link
with a password baked in: don't post them somewhere public, and don't log
them in plaintext analytics.

Never construct a link from Petal's *internal* room credential (the
`room-<32 hex chars>` string you might see in local storage or logs) — only
the access code is a supported, public join input. The internal credential is
explicitly rejected if you try to pass it through the same paths.

## Web invite link (`https://meet.petal.live/...`)

This is the link Petal's own "Copy invite link" button generates, and it's
the right default for anything you don't control the audience of (a calendar
invite, a public-ish channel, a link a bot posts) because it works whether or
not the recipient has Petal installed.

Format:

```
https://meet.petal.live/<label>/<access-code>
https://meet.petal.live/<access-code>
```

- `<access-code>` is the only segment that matters for routing. It must be a
  valid 10-letter code (hyphens optional in the URL, same rules as above).
- `<label>` is optional and purely cosmetic — a slugified version of the
  room's display name (lowercased, non-alphanumeric runs collapsed to a
  single `-`). It's ignored server-side except to render in the page title;
  two links that differ only in label resolve to the exact same room. If you
  don't have a meaningful label, omit it and use the one-segment form.

Example, with a display name of "Eng Sync":

```
https://meet.petal.live/eng-sync/abc-defg-hjk
```

### What happens when it's opened

The URL resolves to a server-rendered interstitial page (`web-harness/api/j.ts`),
not directly into the meeting. That page:

1. Renders immediately with **Open Petal**, platform-specific download
   links for **macOS** and **Windows**, and **Join in browser**. The primary
   download is selected from the visitor's User-Agent; Windows is currently
   unsigned for Authenticode and may show a SmartScreen warning.
2. After a 150ms delay, it also fires a `window.location` redirect to
   `petal://join/<access-code>` automatically — so on a machine with Petal
   installed, most browsers hand off to the app before or shortly after the
   buttons finish rendering, and the visitor never has to click anything.
3. If Petal isn't installed, that automatic redirect has nowhere to go and
   silently does nothing (see the deep-link section below) — the visitor is
   left looking at the same interstitial page with its buttons still there.
   Clicking **Join in browser** takes them straight into the meeting in-tab
   via the web client, no install required. The download links use
   `https://app.petal.live/api/download?platform=macos` and
   `https://app.petal.live/api/download?platform=windows`.

So the interstitial *is* the documented fallback behavior for the web link:
it doesn't detect whether the redirect succeeded (there's no reliable way to
do that for a custom URL scheme), it just shows manual options up front and
races the automatic app redirect against them.

### Skipping the interstitial: direct browser-join link

If you specifically want a link that lands a recipient straight in the
browser client with no interstitial step and no attempt to hand off to the
native app — for example, embedding Petal in a browser-only kiosk or a
web app that shouldn't ever try to open another program — use the query-param
form instead of the path form:

```
https://meet.petal.live/?code=<access-code>
```

This is the same URL the interstitial's own "Join in browser" button points
to; it's handled client-side by the web SPA, which parses `?code=` and
auto-joins. (One caveat for both paths: a visitor who has never used the web
client is asked for a display name before landing in the meeting; returning
visitors join immediately.)

## Desktop deep link (`petal://join/...`)

```
petal://join/<access-code>
```

- `<access-code>` follows the same 10-letter/3-4-3 rules as above. Percent-encoding
  is accepted (e.g. `%2D` for a hyphen) and decoded before validation, but
  since a code only ever contains letters and hyphens — both URL-safe
  unreserved characters — you generally don't need to encode it.
  `encodeURIComponent` in JS leaves such a string unchanged.
- Scheme and the `join` segment are matched case-insensitively; a trailing
  slash and any `?query` or `#fragment` are tolerated and ignored.
- Only this exact shape is accepted: wrong scheme, a different action (e.g.
  `petal://host/...`), a missing/malformed code, or an internal
  `room-<hex>` credential in place of the access code all fail to parse and
  the link is ignored.

### Requires Petal to already be installed — no automatic fallback

`petal://` is only registered with macOS LaunchServices by an **installed,
bundled** `Petal.app` (via its `Info.plist`). There is no way to register the
scheme at runtime, and the app's own dev binary (`tauri dev`) never receives
these links either — only a real install does.

If you hand someone a bare `petal://join/...` link directly (not wrapped in
the `https://meet.petal.live/...` interstitial) and they don't have Petal
installed, **the link just fails** — there's no handler for the scheme, so
the OS/browser either shows its own "can't open this link" error or does
nothing visible at all. Petal provides no fallback of its own at this layer;
the fallback UI (download link, browser-join link) only exists on the web
invite link's interstitial page described above.

**Practical implication:** don't hand out bare `petal://` links to an
audience that might not have Petal installed. Use the `https://meet.petal.live/...`
web link as the primary, shareable link — it degrades gracefully. Reach for
the bare `petal://` form only when you already know the recipient has Petal
installed and want to skip the interstitial (for example, a "Rejoin" button
inside your own internal tool that only Petal users use).

## Choosing a format

| Use case | Link to use |
|---|---|
| Calendar invite, Slack message, anything with a mixed/unknown audience | `https://meet.petal.live/<label>/<access-code>` |
| Embedding in a browser-only surface (kiosk, web dashboard) | `https://meet.petal.live/?code=<access-code>` |
| A control you know only fires for users who already have Petal installed | `petal://join/<access-code>` |

## Example: Slack message

```
Standup is starting — join here:
https://meet.petal.live/eng-sync/abc-defg-hjk
```

Posted as a plain URL (or as `<https://meet.petal.live/eng-sync/abc-defg-hjk|Join eng-sync>`
in Slack's link syntax), this works for every teammate regardless of whether
they have Petal installed yet.

## Example: calendar invite description

```
Join the meeting: https://meet.petal.live/design-review/abc-defg-hjk

If you don't have Petal installed, this link will offer a "Join in browser"
option — no download required.
```
