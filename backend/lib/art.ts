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
  "slowed",
  "reverb",
  "acoustic",
  "nightcore",
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

/** A title split before its "(feat. …)", "[Remastered]", " - Single Version" or " feat. …" tail. */
function splitTitle(title: string): { base: string; tail: string } {
  const cut = title.search(/\s*[([]|\s+-\s+|\s+(?:feat\.?|ft\.?|featuring)\s/iu);
  const base = normalize(cut === -1 ? title : title.slice(0, cut));
  if (base === "") {
    return { base: normalize(title), tail: "" };
  }
  return { base, tail: cut === -1 ? "" : normalize(title.slice(cut)) };
}

/** Where a credit like "A & B", "A, B", "A feat. B" or "A x B" splits into artists. */
const CREDIT_SEPARATORS = /\s*[&,+]\s*|\s+(?:feat\.?|ft\.?|featuring|with|x|and)(?=\s)/giu;

/** The requested artist is the whole credit or one or more of its whole names. */
export function artistMatches(requested: string, candidate: string): boolean {
  const wanted = normalize(requested);
  if (wanted === "") {
    return false;
  }
  const starts = [0];
  const ends: number[] = [];
  for (const separator of candidate.matchAll(CREDIT_SEPARATORS)) {
    ends.push(separator.index);
    starts.push(separator.index + separator[0].length);
  }
  ends.push(candidate.length);
  // Runs of names too, so "Simon & Garfunkel" or "Lil Nas X" survive the split.
  return starts.some((start, i) =>
    ends.slice(i).some((end) => end > start && normalize(candidate.slice(start, end)) === wanted),
  );
}

/** Same title, and the result's tail doesn't make it a remix, live take or the like. */
export function titleMatches(requested: string, candidate: string): boolean {
  const asked = splitTitle(requested);
  const got = splitTitle(candidate);
  if (asked.base === "" || asked.base !== got.base) {
    return false;
  }
  return !VARIANT_WORDS.some(
    (word) => containsWords(got.tail, word) && !containsWords(asked.tail, word),
  );
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
