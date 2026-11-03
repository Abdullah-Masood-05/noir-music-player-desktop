/**
 * GET /api/art?artist=<name>&track=<title> → `{ image: <https url> | null }`
 *
 * Cover art for a Discover row, from Last.fm, then iTunes, then Deezer; the
 * first that has it wins. Only HTTPS URLs on the services' image CDNs are
 * answered. A song's art doesn't change, so answers — including "none" —
 * are cached for a week; an answer that a failed service may have cut short
 * is not cached at all.
 */

import { deezerImage, itunesImage, lastfmImage } from "../lib/art.js";
import { fail, fetchJson, json, leaksKey, serviceKey, type Upstream } from "../lib/upstream.js";
import { searchQuery } from "../lib/validate.js";

const CACHE_SECONDS = 60 * 60 * 24 * 7;

/** All three services together; a thumbnail isn't worth a longer wait. */
const BUDGET_MS = 7_500;
const SOURCE_TIMEOUT_MS = 3_000;

type Source = (artist: string, track: string, timeoutMs: number) => Promise<Lookup>;

/** `failed` means the service couldn't be asked, so "no image" isn't final. */
type Lookup = { image: string | null; failed: boolean };

function answered(
  upstream: Upstream,
  pick: (body: Record<string, unknown>) => string | null,
): Lookup {
  if (!upstream.ok || upstream.status !== 200) {
    return { image: null, failed: true };
  }
  return { image: pick(upstream.body), failed: false };
}

const lastfm: Source = async (artist, track, timeoutMs) => {
  const key = serviceKey("LASTFM_API_KEY");
  if (key === null) {
    return { image: null, failed: false };
  }
  const params = new URLSearchParams({
    method: "track.getInfo",
    api_key: key,
    artist,
    track,
    autocorrect: "1",
    format: "json",
  });
  const upstream = await fetchJson(`https://ws.audioscrobbler.com/2.0/?${params}`, {}, timeoutMs);
  // Error 6, "track not found", is the only real miss, whatever status it comes with.
  if (upstream.ok && upstream.body.error === 6) {
    return { image: null, failed: false };
  }
  if (upstream.ok && "error" in upstream.body) {
    return { image: null, failed: true };
  }
  return answered(upstream, lastfmImage);
};

const itunes: Source = async (artist, track, timeoutMs) => {
  const params = new URLSearchParams({
    term: `${artist} ${track}`,
    entity: "song",
    limit: "3",
    country: "US",
  });
  const upstream = await fetchJson(`https://itunes.apple.com/search?${params}`, {}, timeoutMs);
  return answered(upstream, (body) => itunesImage(body, artist, track));
};

const deezer: Source = async (artist, track, timeoutMs) => {
  const params = new URLSearchParams({ q: `${track} ${artist}`, limit: "5" });
  const upstream = await fetchJson(`https://api.deezer.com/search?${params}`, {}, timeoutMs);
  // Deezer sends quota and rate-limit errors with a 200.
  if (upstream.ok && "error" in upstream.body) {
    return { image: null, failed: true };
  }
  return answered(upstream, (body) => deezerImage(body, artist, track));
};

export async function GET(request: Request): Promise<Response> {
  const params = new URL(request.url).searchParams;
  const artist = searchQuery(params.get("artist"));
  const track = searchQuery(params.get("track"));
  if (artist === null || track === null) {
    return fail(400, "That song or artist is empty, too long or contains characters it can't use.");
  }

  const deadline = Date.now() + BUDGET_MS;
  let complete = true;
  for (const source of [lastfm, itunes, deezer]) {
    const remaining = deadline - Date.now();
    if (remaining < 250) {
      complete = false;
      break;
    }
    const found = await source(artist, track, Math.min(SOURCE_TIMEOUT_MS, remaining));
    if (found.image !== null && !leaksKey(found.image)) {
      return json({ image: found.image }, CACHE_SECONDS);
    }
    complete &&= !found.failed;
  }
  return json({ image: null }, complete ? CACHE_SECONDS : 0);
}
