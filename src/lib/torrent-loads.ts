// SPDX-License-Identifier: MIT OR Apache-2.0

/** Link and hash aliases let views share a torrent's pending metadata request. */
export function torrentLoadKeys(source: string, infoHash: string | null = null): string[] {
  const link = source.trim();
  const keys = [`link:${link}`];
  if (infoHash) {
    keys.push(`hash:${infoHash.toLowerCase()}`);
  }
  if (/^magnet:\?/i.test(link)) {
    const params = new URLSearchParams(link.slice(link.indexOf('?') + 1));
    for (const xt of params.getAll('xt')) {
      const match = /^urn:btih:([a-f\d]{40}|[a-z2-7]{32})$/i.exec(xt);
      if (match?.[1]) {
        keys.push(`hash:${match[1].toLowerCase()}`);
      }
    }
  }
  return [...new Set(keys)];
}

/** Shares duplicate requests, while unrelated torrents can finish independently. */
export class TorrentLoads<T> {
  private readonly pending = new Map<string, Promise<T>>();

  constructor(private readonly changed: (keys: ReadonlySet<string>) => void) {}

  run(keys: readonly string[], load: () => Promise<T>): Promise<T> {
    const existing = keys.map((key) => this.pending.get(key)).find((p) => p !== undefined);
    if (existing) {
      // A later caller may know the hash as well as the original download link.
      this.track(keys, existing);
      return existing;
    }
    const request = Promise.resolve()
      .then(load)
      .finally(() => {
        for (const [key, pending] of this.pending) {
          if (pending === request) {
            this.pending.delete(key);
          }
        }
        this.changed(new Set(this.pending.keys()));
      });
    this.track(keys, request);
    return request;
  }

  private track(keys: readonly string[], request: Promise<T>): void {
    for (const key of keys) {
      if (!this.pending.has(key)) {
        this.pending.set(key, request);
      }
    }
    this.changed(new Set(this.pending.keys()));
  }
}
