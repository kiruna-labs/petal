# Reactions (built-in)

Emoji reactions that float up over the meeting. Meeting-scoped: with M2's
data bus every participant who has the plugin sees them; before that, the
sender sees a local echo only.

Buildless on purpose (`plugin.js` is the entry; no `src/`, no Vite): built-ins
are compiled into both clients with a `?raw` import, so they must not need a
build step. Third-party plugins should use `@petal/plugin-sdk` instead.
