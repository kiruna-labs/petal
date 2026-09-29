export interface DeviceNavigator {
  userAgent: string;
  maxTouchPoints?: number;
  /** User-Agent Client Hints (Chromium only). */
  userAgentData?: { mobile?: boolean };
}

/**
 * Phones and tablets: devices that cannot install the Petal desktop app, so
 * the home footer offers them no download (#243).
 * - Chromium's client hints answer directly when they say "mobile". Only a
 *   yes is trusted: an Android tablet reports `mobile: false`.
 * - Otherwise an Android, iPhone, iPad or iPod user agent, or any `Mobile`
 *   token. The server-rendered invite page applies the same rule (#242);
 *   keep the two in step.
 * - iPadOS Safari reports a desktop "Macintosh" user agent, so a Mac with a
 *   touch screen counts too -- no Mac ships with one.
 */
export function isPhoneOrTablet({ userAgent, maxTouchPoints = 0, userAgentData }: DeviceNavigator): boolean {
  if (userAgentData?.mobile === true) return true;
  if (/Android|iPhone|iPad|iPod|Mobile/i.test(userAgent)) return true;
  return /Macintosh/i.test(userAgent) && maxTouchPoints > 1;
}
