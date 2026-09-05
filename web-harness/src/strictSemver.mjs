const CORE_VERSION_PATTERN = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const IDENTIFIER_PATTERN = /^[0-9A-Za-z-]+$/;
const NUMERIC_IDENTIFIER_PATTERN = /^(0|[1-9]\d*)$/;

function validIdentifiers(value, numericLeadingZeroesForbidden) {
  const identifiers = value.split('.');
  return identifiers.every((identifier) => {
    if (!IDENTIFIER_PATTERN.test(identifier)) return false;
    return !numericLeadingZeroesForbidden || !/^\d+$/.test(identifier) || NUMERIC_IDENTIFIER_PATTERN.test(identifier);
  });
}

/** Standards-compliant SemVer 2.0.0 validation without a runtime dependency. */
export function isStrictSemVer(value) {
  if (typeof value !== 'string') return false;

  const buildSeparator = value.indexOf('+');
  const withoutBuild = buildSeparator === -1 ? value : value.slice(0, buildSeparator);
  const build = buildSeparator === -1 ? undefined : value.slice(buildSeparator + 1);
  if (build !== undefined && !validIdentifiers(build, false)) return false;

  const prereleaseSeparator = withoutBuild.indexOf('-');
  const core = prereleaseSeparator === -1 ? withoutBuild : withoutBuild.slice(0, prereleaseSeparator);
  const prerelease = prereleaseSeparator === -1 ? undefined : withoutBuild.slice(prereleaseSeparator + 1);

  return CORE_VERSION_PATTERN.test(core)
    && (prerelease === undefined || validIdentifiers(prerelease, true));
}
