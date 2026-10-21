// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * The notice about the VPN adapter mode, removed in 0.3.0. Settings that used it load as Direct
 * with `network.vpnAdapterModeRemoved` set; while it is set and the mode is Direct, the backend
 * holds all traffic. The notice shows in Settings → Network and beside the status bar's network
 * indicator until it is dismissed, which clears the flag and lets Direct connect.
 */

import type { NetworkSettings } from './bindings/NetworkSettings';
import { updateSettings } from './ipc';

/** What changed and what to do instead. */
export const REMOVED_MODE_NOTICE =
  "The VPN adapter mode was removed in 0.3.0. TorrentPlayer now connects directly — keep your VPN app connected, or use the built-in WireGuard mode with your provider's WireGuard configuration to get a kill switch.";

/** Added while traffic is held (Direct mode). */
export const REMOVED_MODE_HELD =
  'Torrent traffic and searches stay paused until you dismiss this note.';

/** Whether the notice is shown. */
export function showRemovedModeNotice(
  network: Pick<NetworkSettings, 'vpnAdapterModeRemoved'>,
): boolean {
  return network.vpnAdapterModeRemoved;
}

/** Whether the backend holds traffic because of it: the notice is pending in Direct mode. */
export function removedModeHolds(
  network: Pick<NetworkSettings, 'mode' | 'vpnAdapterModeRemoved'>,
): boolean {
  return network.vpnAdapterModeRemoved && network.mode === 'direct';
}

/** Hides the notice for good (and, in Direct mode, lets traffic flow). Rejects if not saved. */
export function dismissRemovedModeNotice(): Promise<void> {
  return updateSettings({ network: { vpnAdapterModeRemoved: false } }).then(() => undefined);
}
