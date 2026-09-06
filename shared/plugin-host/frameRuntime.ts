// The JavaScript that boots inside every plugin frame BEFORE the plugin's own
// module. It implements the `Petal` object (api.ts) over the postMessage
// envelope (protocol.ts). It is shipped as a string so the host can inline it
// into a sandboxed srcdoc with a `connect-src 'none'` CSP: the frame has no
// way to load anything, so everything it runs must arrive in the document.
//
// Rules for editing this file:
// - Plain JS only, no TypeScript, no template literals (it lives in one).
// - Nothing here may trust the host less than it trusts the plugin: the
//   host is the trusted side. But it MUST ignore messages whose source is
//   not `window.parent` so a plugin's own popups cannot spoof host events.
// - Keep it small; it is re-parsed for every plugin frame.
//
// Tested by web-harness/tests/pluginFrame.test.ts under node:vm.

export const FRAME_RUNTIME_SOURCE = String.raw`
(function petalFrameRuntime() {
  'use strict';
  var PROTOCOL = 1;
  var hostOrigin = null;
  var nextId = 1;
  var pending = new Map();
  var listeners = new Map();
  var definition = null;
  var init = null;
  var activated = false;
  var surfacePort = null;
  var channels = new Map();
  var snapshot = { self: null, participants: [], room: { label: '', phase: 'connecting' }, state: {}, shares: [] };
  var encoder = new TextEncoder();
  var decoder = new TextDecoder();

  function send(env, transfer) {
    window.parent.postMessage(env, hostOrigin || '*', transfer || []);
  }
  function request(method, params) {
    return new Promise(function (resolve, reject) {
      var id = nextId++;
      pending.set(id, { resolve: resolve, reject: reject });
      send({ v: PROTOCOL, kind: 'req', id: id, method: method, params: params });
    });
  }
  function fireAndForget(method, params) {
    request(method, params).catch(function () {});
  }
  function on(event, cb) {
    var set = listeners.get(event);
    if (!set) { set = new Set(); listeners.set(event, set); }
    set.add(cb);
    return function () { set.delete(cb); };
  }
  function emitLocal(event, args) {
    var set = listeners.get(event);
    if (!set) return;
    Array.from(set).forEach(function (cb) {
      try { cb.apply(null, args); } catch (e) {
        fireAndForget('log', { level: 'error', args: ['listener for ' + event + ' threw', String((e && e.stack) || e)] });
      }
    });
  }
  function toBytes(payload) {
    if (payload instanceof Uint8Array) return payload;
    return encoder.encode(JSON.stringify(payload === undefined ? null : payload));
  }
  function stringifyArg(a) {
    if (typeof a === 'string') return a;
    if (a instanceof Error) return String(a.stack || a.message || a);
    try { return JSON.stringify(a); } catch (e) { return String(a); }
  }
  function lazyChannel(surfaceId) {
    var existing = channels.get(surfaceId);
    if (existing) return existing;
    var port = null;
    var queue = [];
    var subs = new Set();
    function attach(p) {
      if (port) { try { port.close(); } catch (e) {} }
      port = p;
      port.onmessage = function (ev) {
        Array.from(subs).forEach(function (cb) { try { cb(ev.data); } catch (e) {} });
      };
      queue.forEach(function (m) { port.postMessage(m); });
      queue = [];
    }
    var channel = {
      postMessage: function (message) { if (port) port.postMessage(message); else queue.push(message); },
      onMessage: function (cb) { subs.add(cb); return function () { subs.delete(cb); }; },
      __attach: attach
    };
    channels.set(surfaceId, channel);
    return channel;
  }

  var petal = {
    get apiVersion() { return init ? init.apiVersion : 1; },
    get hostVersion() { return init ? init.hostVersion : ''; },
    get hostSupports() { return init ? init.hostSupports : { native: false, frames: false }; },
    plugin: { id: '', version: '', scope: 'local', permissions: [] },
    meeting: {
      self: function () { return snapshot.self; },
      participants: function () { return snapshot.participants.slice(); },
      room: function () { return { label: snapshot.room.label, phase: snapshot.room.phase }; },
      on: function (event, cb) { return on('meeting.' + event, cb); }
    },
    data: {
      publish: function (sub, payload, opts) {
        opts = opts || {};
        var bytes = toBytes(payload);
        var params = { sub: sub === undefined ? null : sub, payload: bytes, reliable: opts.reliable !== false };
        if (opts.to) params.to = opts.to.slice();
        return request('data.publish', params);
      },
      on: function (sub, cb) {
        return on('data.message', function (m) {
          if (sub !== null && sub !== undefined && m.sub !== sub) return;
          cb(m);
        });
      }
    },
    state: {
      set: function (value) { return request('state.set', { value: value === undefined ? null : value }); },
      get: function (identity) { return snapshot.state[identity]; },
      on: function (cb) { return on('state.changed', cb); }
    },
    storage: {
      get: function (key) { return request('storage.get', { key: key }); },
      set: function (key, value) { return request('storage.set', { key: key, value: value }); },
      delete: function (key) { return request('storage.delete', { key: key }); },
      keys: function () { return request('storage.keys', {}); }
    },
    ui: {
      channel: function (surfaceId) { return lazyChannel(surfaceId); },
      onAction: function (cb) { return on('ui.action', cb); },
      setButton: function (buttonId, patch) { return request('ui.setButton', { buttonId: buttonId, patch: patch || {} }); },
      openSurface: function (surfaceId) { return request('ui.openSurface', { surfaceId: surfaceId }); },
      closeSurface: function (surfaceId) { return request('ui.closeSurface', { surfaceId: surfaceId }); },
      toast: function (text, opts) { return request('ui.toast', { text: text, variant: (opts && opts.variant) || 'info' }); }
    },
    shares: {
      list: function () { return snapshot.shares.slice(); },
      on: function (cb) { return on('shares.changed', cb); }
    },
    net: {
      fetch: function (url, opts) {
        opts = opts || {};
        return request('net.fetch', { url: url, method: opts.method || 'GET', headers: opts.headers || {}, body: opts.body })
          .then(function (res) {
            return {
              status: res.status,
              ok: res.status >= 200 && res.status < 300,
              headers: res.headers || {},
              text: function () { return res.body; },
              json: function () { return JSON.parse(res.body); }
            };
          });
      }
    },
    clipboard: {
      writeText: function (text) { return request('clipboard.writeText', { text: text }); }
    },
    log: {}
  };
  ['debug', 'info', 'warn', 'error'].forEach(function (level) {
    petal.log[level] = function () {
      fireAndForget('log', { level: level, args: Array.prototype.map.call(arguments, stringifyArg) });
    };
  });

  function applyInit(payload) {
    init = payload;
    petal.plugin = {
      id: payload.pluginId,
      version: payload.version,
      scope: payload.scope,
      permissions: (payload.grantedPermissions || []).slice()
    };
    if (payload.meeting) {
      snapshot.self = payload.meeting.self;
      snapshot.participants = payload.meeting.participants || [];
      snapshot.room = payload.meeting.room || snapshot.room;
    }
    if (payload.state) snapshot.state = payload.state;
    if (payload.shares) snapshot.shares = payload.shares;
  }
  function applyEvent(event, payload) {
    if (event === 'meeting.participant-joined') {
      snapshot.participants = snapshot.participants.filter(function (p) { return p.identity !== payload.identity; }).concat([payload]);
      return [payload];
    }
    if (event === 'meeting.participant-left') {
      snapshot.participants = snapshot.participants.filter(function (p) { return p.identity !== payload.identity; });
      return [payload];
    }
    if (event === 'meeting.participant-changed') {
      snapshot.participants = snapshot.participants.map(function (p) { return p.identity === payload.identity ? payload : p; });
      if (snapshot.self && snapshot.self.identity === payload.identity) snapshot.self = payload;
      return [payload];
    }
    if (event === 'meeting.phase') { snapshot.room = payload; return [payload]; }
    if (event === 'state.changed') {
      if (payload.value === undefined || payload.value === null) delete snapshot.state[payload.identity];
      else snapshot.state[payload.identity] = payload.value;
      return [payload.identity, payload.value];
    }
    if (event === 'shares.changed') { snapshot.shares = payload || []; return [snapshot.shares.slice()]; }
    if (event === 'data.message') {
      var bytes = payload.payload instanceof Uint8Array ? payload.payload : new Uint8Array(payload.payload || []);
      return [{
        sub: payload.sub === undefined ? null : payload.sub,
        sender: payload.sender,
        payload: bytes,
        json: function () { return JSON.parse(decoder.decode(bytes)); }
      }];
    }
    return [payload];
  }
  function maybeActivate() {
    if (activated || !definition || !init) return;
    activated = true;
    Promise.resolve().then(function () {
      if (init.surface) {
        if (typeof definition.mountSurface !== 'function') throw new Error('plugin declares surfaces but has no mountSurface');
        return definition.mountSurface(petal, {
          id: init.surface.id,
          kind: init.surface.kind,
          root: document.body,
          channel: (function () { var c = lazyChannel(init.surface.id); if (surfacePort) c.__attach(surfacePort); return c; })()
        });
      }
      if (typeof definition.activate === 'function') return definition.activate(petal);
    }).then(function () {
      send({ v: PROTOCOL, kind: 'evt', event: 'activated', payload: {} });
    }, function (e) {
      send({ v: PROTOCOL, kind: 'evt', event: 'error', payload: { message: String((e && e.message) || e), stack: String((e && e.stack) || '') } });
    });
  }

  window.__petalRegister = function (def) {
    definition = def || {};
    maybeActivate();
    return def;
  };

  window.addEventListener('message', function (event) {
    if (event.source !== window.parent) return;
    var env = event.data;
    if (!env || env.v !== PROTOCOL) return;
    if (env.kind === 'res') {
      var p = pending.get(env.id);
      if (!p) return;
      pending.delete(env.id);
      if (env.ok) p.resolve(env.result);
      else {
        var err = new Error((env.error && env.error.message) || 'request failed');
        err.code = env.error && env.error.code;
        p.reject(err);
      }
      return;
    }
    if (env.kind !== 'evt') return;
    if (env.event === 'init') {
      if (init) return;
      hostOrigin = event.origin && event.origin !== 'null' ? event.origin : '*';
      if (event.ports && event.ports[0]) surfacePort = event.ports[0];
      applyInit(env.payload || {});
      if (init.surface) {
        // Keyboard focus is inside this frame while a popover is open, so the
        // host never sees Escape; forward it as a dismiss request.
        document.addEventListener('keydown', function (e) {
          if (e.key === 'Escape') send({ v: PROTOCOL, kind: 'evt', event: 'dismiss', payload: { surfaceId: init.surface.id } });
        });
      }
      send({ v: PROTOCOL, kind: 'evt', event: 'ready', payload: { pluginId: petal.plugin.id } });
      maybeActivate();
      return;
    }
    if (!init) return;
    if (env.event === 'ui.surface-opened') {
      if (event.ports && event.ports[0]) lazyChannel(env.payload.surfaceId).__attach(event.ports[0]);
      emitLocal(env.event, [env.payload]);
      return;
    }
    emitLocal(env.event, applyEvent(env.event, env.payload));
  });
})();
`;
