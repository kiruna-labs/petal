/** The package version is immutable; only non-release displays carry this marker. */
export function displayBuildVersion(buildInfo: {
  version: string;
  isReleaseBuild: boolean;
}): string {
  return buildInfo.isReleaseBuild ? buildInfo.version : `${buildInfo.version}-dev`;
}
