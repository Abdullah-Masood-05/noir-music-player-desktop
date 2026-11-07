/**
 * GET /api/resolve → 410 Gone.
 *
 * Noir Player downloads songs on the listener's own device, so only older
 * copies of the app call this. They get a message that tells them what to do.
 */

import { retired } from "../lib/upstream.js";

export function GET(): Response {
  return retired();
}
