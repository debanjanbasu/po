export type ShareRoute =
  | { kind: 'contact'; username: string; dhtKey: string }
  | { kind: 'pairing'; payload: string };

export type ShareRouteLocation = Pick<Location, 'pathname'>;

const CONTACT_ROUTE = 'c';
const PAIRING_ROUTE = 'p';

function safeDecode(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

function parseRouteParts(parts: readonly string[]): ShareRoute | null {
  if (parts[0] === CONTACT_ROUTE && parts.length === 3) {
    return {
      kind: 'contact',
      username: safeDecode(parts[1]),
      dhtKey: safeDecode(parts[2]),
    };
  }
  if (parts[0] === PAIRING_ROUTE && parts.length === 2) {
    return {
      kind: 'pairing',
      payload: safeDecode(parts[1]),
    };
  }
  return null;
}

function base64UrlEncode(value: string): string | null {
  try {
    return btoa(value)
      .replace(/\+/gu, '-')
      .replace(/\//gu, '_')
      .replace(/=+$/u, '');
  } catch {
    // btoa throws on non-Latin-1 input (e.g. a crafted URL with multibyte
    // characters); treat it as "no deep link available".
    return null;
  }
}

export function parseShareRoute(locationLike: ShareRouteLocation): ShareRoute | null {
  return parseRouteParts(locationLike.pathname.split('/').filter(Boolean));
}

export function buildContactIdentity(username: string, dhtKey: string): string {
  return `oo:${username}#${dhtKey}`;
}

export function buildContactDeepLink(username: string, dhtKey: string): string | null {
  const encoded = base64UrlEncode(`${username}|${dhtKey}`);
  return encoded === null ? null : `oo://chat?data=${encoded}`;
}

export function buildPairingDeepLink(payload: string): string {
  return `oo://pair?data=${encodeURIComponent(payload)}`;
}
