/**
 * Picking cover art out of the services' answers. Pure, so every rule here
 * can be checked without calling anything.
 */

/** Last.fm's grey star, sent in place of art it doesn't have. */
const LASTFM_PLACEHOLDER = "2a96cbd8b46e442fc41c2b86b821562f";

const IMAGE_HOSTS: ReadonlySet<string> = new Set([
  "lastfm.freetls.fastly.net",
  "cdn-images.dzcdn.net",
  "e-cdns-images.dzcdn.net",
  "is1-ssl.mzstatic.com",
  "is2-ssl.mzstatic.com",
  "is3-ssl.mzstatic.com",
  "is4-ssl.mzstatic.com",
  "is5-ssl.mzstatic.com",
]);

/** Words that make a result a different recording from the one asked for. */
const VARIANT_WORDS: readonly string[] = [
  "remix",
  "cover",
  "instrumental",
  "karaoke",
  "live",
  "sped up",
];

/** Lowercase letters and digits in any script, separated by single spaces. */
export function normalize(value: string): string {
  return value
    .normalize("NFKD")
    .replace(/\p{M}+/gu, "")
    .toLowerCase()
    .replace(/&/g, " and ")
    .replace(/[^\p{L}\p{N}]+/gu, " ")
    .trim();
}

/** Whether `phrase` appears in `text` as whole words; both normalized. */
function containsWords(text: string, phrase: string): boolean {
  return phrase !== "" && ` ${text} `.includes(` ${phrase} `);
}

/** A title without its "(feat. …)", "[Remastered]" or " - Single Version" tail. */
function baseTitle(title: string): string {
  return normalize(title.replace(/\s*[([].*$/, "").replace(/\s+-\s+.*$/, ""));
}

export function artistMatches(requested: string, candidate: string): boolean {
  const wanted = normalize(requested);
  const found = normalize(candidate);
  return containsWords(found, wanted);
}

export function titleMatches(requested: string, candidate: string): boolean {
  const wanted = baseTitle(requested) || normalize(requested);
  const found = baseTitle(candidate) || normalize(candidate);
  if (!containsWords(found, wanted)) {
    return false;
  }
  const asked = normalize(requested);
  const got = normalize(candidate);
  return !VARIANT_WORDS.some((word) => containsWords(got, word) && !containsWords(asked, word));
}

/** True for an HTTPS URL on one of the image CDNs, with nothing but a path and query. */
export function allowedImageUrl(value: unknown): value is string {
  if (typeof value !== "string" || value.length > 2048) {
    return false;
  }
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return false;
  }
  return (
    url.protocol === "https:" &&
    url.username === "" &&
    url.password === "" &&
    url.port === "" &&
    IMAGE_HOSTS.has(url.hostname) &&
    !url.pathname.includes(LASTFM_PLACEHOLDER)
  );
}

type Json = Record<string, unknown>;

function object(value: unknown): Json | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Json)
    : undefined;
}

function list(value: unknown): Json[] {
  return Array.isArray(value)
    ? value.map(object).filter((entry): entry is Json => entry !== undefined)
    : [];
}

const LASTFM_SIZES = ["small", "medium", "large", "extralarge", "mega"];

/** The largest real album image in a `track.getInfo` answer. */
export function lastfmImage(body: Json): string | null {
  const images = list(object(object(body.track)?.album)?.image)
    .filter((image) => typeof image["#text"] === "string" && image["#text"] !== "")
    .sort(
      (a, b) =>
        LASTFM_SIZES.indexOf(String(b.size)) - LASTFM_SIZES.indexOf(String(a.size)),
    );
  const best = images[0]?.["#text"];
  return allowedImageUrl(best) ? best : null;
}

/** The first iTunes song by the requested artist with the requested title. */
export function itunesImage(body: Json, artist: string, track: string): string | null {
  for (const result of list(body.results)) {
    if (
      typeof result.artistName === "string" &&
      typeof result.trackName === "string" &&
      typeof result.artworkUrl100 === "string" &&
      artistMatches(artist, result.artistName) &&
      titleMatches(track, result.trackName)
    ) {
      const url = result.artworkUrl100.replace("/100x100bb.", "/200x200bb.");
      return allowedImageUrl(url) ? url : null;
    }
  }
  return null;
}

/** The first Deezer track by the requested artist with the requested title. */
export function deezerImage(body: Json, artist: string, track: string): string | null {
  for (const result of list(body.data)) {
    const name = object(result.artist)?.name;
    const cover = object(result.album)?.cover_medium;
    if (
      typeof name === "string" &&
      typeof result.title === "string" &&
      artistMatches(artist, name) &&
      titleMatches(track, result.title)
    ) {
      return allowedImageUrl(cover) ? cover : null;
    }
  }
  return null;
}
