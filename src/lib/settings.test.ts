// SPDX-License-Identifier: MIT OR Apache-2.0

import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { SettingsPatch, Timers } from './settings';

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke }));

const settings = await import('./settings');
const ipc = await import('./ipc');
const {
  availability,
  createSaver,
  describeRate,
  downloadDirProblem,
  fieldError,
  isHost,
  mergePatch,
  patchAt,
  splitRate,
  toKib,
  validate,
} = settings;

beforeEach(() => {
  invoke.mockReset();
});

describe('validate', () => {
  it('mirrors the Rust ranges', () => {
    expect(validate('bandwidth.peerLimit', 40)).toBeNull();
    expect(validate('bandwidth.peerLimit', 4)).toMatch(/5 and 500/);
    expect(validate('bandwidth.peerLimit', 40.5)).not.toBeNull();
    expect(validate('bandwidth.maxDownloads', 0)).toBeNull();
    expect(validate('bandwidth.maxDownloads', 101)).toMatch(/0 for no limit/);
    expect(validate('bandwidth.seedTimeLimitMinutes', 0)).toBeNull();
    expect(validate('bandwidth.seedTimeLimitMinutes', 525_601)).toMatch(/one year/);
    expect(validate('advanced.ffmpegDir', null)).toBeNull();
    expect(validate('advanced.ffmpegDir', 'C:\\ffmpeg\\bin')).toBeNull();
    expect(validate('advanced.ffmpegDir', 'ffmpeg')).toMatch(/full path/);
    expect(validate('playback.defaultSpeed', 1.25)).toBeNull();
    expect(validate('playback.defaultSpeed', 3.5)).not.toBeNull();
    expect(validate('playback.defaultSpeed', Number.NaN)).not.toBeNull();
    expect(validate('storage.cacheLimitGb', null)).toBeNull();
    expect(validate('storage.cacheLimitGb', 0)).not.toBeNull();
    expect(validate('bandwidth.downloadLimitKib', 0)).toBeNull();
    expect(validate('bandwidth.downloadLimitKib', 2 * 1024 * 1024)).not.toBeNull();
    expect(validate('network.socks5.port', 0)).not.toBeNull();
    expect(validate('network.socks5.host', '')).toBeNull();
    expect(validate('network.socks5.username', 'x'.repeat(256))).not.toBeNull();
    expect(validate('network.socks5.proxyDns', false)).toBeNull();
  });

  it('accepts hosts without schemes or ports', () => {
    for (const ok of ['10.0.0.1', 'proxy.example.org', '::1', 'a-b.c']) {
      expect(isHost(ok)).toBe(true);
    }
    for (const bad of ['', 'socks5://x', 'host:1080x', 'a b', '-a.com', 'a..b', '[::1]']) {
      expect(isHost(bad)).toBe(false);
    }
  });

  it('checks download folders', () => {
    expect(downloadDirProblem('D:\\Lectures')).toBeNull();
    expect(downloadDirProblem('/home/me/Lectures')).toBeNull();
    expect(downloadDirProblem('\\\\nas\\share\\lectures')).toBeNull();
    expect(downloadDirProblem('Lectures')).toMatch(/full path/);
    expect(downloadDirProblem('C:\\')).toMatch(/root/);
    expect(downloadDirProblem('/')).toMatch(/root/);
    expect(downloadDirProblem('/a/../b')).toMatch(/"\.\."/);
    expect(downloadDirProblem('\\\\.\\PhysicalDrive0')).toMatch(/Device/);
  });
});

describe('patches', () => {
  it('builds and merges nested patches', () => {
    expect(patchAt('bandwidth.peerLimit', 80)).toEqual({ bandwidth: { peerLimit: 80 } });
    const merged = mergePatch(
      { network: { mode: 'socks5', socks5: { host: 'a' } } },
      { network: { socks5: { port: 1080 } }, storage: { cacheLimitGb: null } },
    );
    expect(merged).toEqual({
      network: { mode: 'socks5', socks5: { host: 'a', port: 1080 } },
      storage: { cacheLimitGb: null },
    });
  });

  it('reads the field from validation errors', () => {
    const error = {
      kind: 'invalid-argument',
      message: 'bandwidth.peerLimit: Choose between 5 and 500 peers.',
    };
    expect(fieldError(error)).toEqual({
      field: 'bandwidth.peerLimit',
      message: 'Choose between 5 and 500 peers.',
    });
    expect(fieldError({ kind: 'internal', message: 'a.b: c' })).toBeNull();
    expect(fieldError({ kind: 'invalid-argument', message: 'Enter a password.' })).toBeNull();
  });
});

describe('rates', () => {
  it('picks readable units and converts back', () => {
    expect(splitRate(512)).toEqual({ value: 512, unit: 'KB/s' });
    expect(splitRate(2048)).toEqual({ value: 2, unit: 'MB/s' });
    expect(splitRate(1536)).toEqual({ value: 1.5, unit: 'MB/s' });
    expect(splitRate(1500)).toEqual({ value: 1500, unit: 'KB/s' });
    expect(toKib(1.5, 'MB/s')).toBe(1536);
    expect(toKib(300, 'KB/s')).toBe(300);
    expect(describeRate(0)).toBe('Unlimited');
    expect(describeRate(5120)).toBe('5 MB/s');
  });
});

describe('availability', () => {
  it('is honest about what applies when', () => {
    expect(availability('advanced.logLevel')).toBe('live');
    expect(availability('advanced.engine')).toBe('restart');
    expect(availability('storage.downloadDir')).toBe('restart');
    expect(availability('bandwidth.peerLimit')).toBe('restart');
    expect(availability('bandwidth.seedAfterWatching')).toBe('live');
    expect(availability('bandwidth.seedRatioTarget')).toBe('live');
    expect(availability('bandwidth.seedTimeLimitMinutes')).toBe('live');
    expect(availability('storage.cacheLimitGb')).toBe('live');
    expect(availability('storage.cleanupAfterDays')).toBe('live');
    expect(availability('search.sources.internetArchive')).toBe('later');
    expect(availability('playback.defaultSpeed')).toBe('live');
    expect(availability('search.hideAdult')).toBe('live');
    expect(availability('network.socks5.proxyDns')).toBe('live');
    expect(availability('network.socks5.proxyDht')).toBe('live');
    expect(availability('network.encryption')).toBe('later');
  });
});

describe('createSaver', () => {
  function fakeTimers(): { timers: Timers; fire: () => void } {
    let pending: (() => void) | null = null;
    return {
      timers: {
        set: (fn: () => void) => {
          pending = fn;
          return 1;
        },
        clear: () => {
          pending = null;
        },
      },
      fire: () => {
        const fn = pending;
        pending = null;
        fn?.();
      },
    };
  }

  it('merges rapid changes into one save', async () => {
    const { timers, fire } = fakeTimers();
    const save = vi.fn().mockResolvedValue({ settings: {} });
    const onSaved = vi.fn();
    const saver = createSaver({ save, onSaved, onError: vi.fn(), timers });
    saver.queue({ bandwidth: { peerLimit: 50 } });
    saver.queue({ bandwidth: { peerLimit: 60 }, search: { hideAdult: false } });
    expect(save).not.toHaveBeenCalled();
    fire();
    await saver.flush();
    expect(save).toHaveBeenCalledTimes(1);
    expect(save).toHaveBeenCalledWith({
      bandwidth: { peerLimit: 60 },
      search: { hideAdult: false },
    });
    expect(onSaved).toHaveBeenCalledWith({ settings: {} }, false);
    expect(saver.pending()).toBeNull();
  });

  it('reports failures with the rejected patch', async () => {
    const { timers } = fakeTimers();
    const onError = vi.fn();
    const saver = createSaver({
      save: () => Promise.reject(new Error('nope')),
      onSaved: vi.fn(),
      onError,
      timers,
    });
    saver.queue({ playback: { defaultSpeed: 2 } });
    await saver.flush();
    expect(onError).toHaveBeenCalledWith(expect.any(Error), { playback: { defaultSpeed: 2 } });
  });

  it('runs saves one at a time', async () => {
    const { timers } = fakeTimers();
    let release: () => void = () => undefined;
    const order: string[] = [];
    const save = vi.fn((patch: SettingsPatch) => {
      order.push(`start ${JSON.stringify(patch)}`);
      return new Promise<never>((resolve) => {
        release = () => {
          order.push('end');
          resolve({ settings: {} } as never);
        };
      });
    });
    const saver = createSaver({ save, onSaved: vi.fn(), onError: vi.fn(), timers });
    saver.queue({ a: 1 });
    void saver.flush();
    saver.queue({ b: 2 });
    const done = saver.flush();
    await vi.waitFor(() => {
      expect(save).toHaveBeenCalledTimes(1);
    });
    expect(order).toEqual(['start {"a":1}']);
    release();
    await vi.waitFor(() => {
      expect(save).toHaveBeenCalledTimes(2);
    });
    release();
    await done;
    expect(order).toEqual(['start {"a":1}', 'end', 'start {"b":2}', 'end']);
  });
});

describe('IPC wrappers', () => {
  it('send the arguments the Rust commands expect', async () => {
    invoke.mockResolvedValue(undefined);
    await ipc.getSettings();
    await ipc.updateSettings({ bandwidth: { peerLimit: 80 } });
    await ipc.setSecret('socks5Password', 'pw');
    await ipc.clearSecret('wireGuardPrivateKey');
    await ipc.secretStatus();
    await ipc.cacheUsage();
    await ipc.clearCache();
    await ipc.chooseDownloadDir();
    expect(invoke.mock.calls).toEqual([
      ['get_settings'],
      ['update_settings', { patch: { bandwidth: { peerLimit: 80 } } }],
      ['set_secret', { key: 'socks5Password', value: 'pw' }],
      ['clear_secret', { key: 'wireGuardPrivateKey' }],
      ['secret_status'],
      ['cache_usage'],
      ['clear_cache'],
      ['choose_download_dir'],
    ]);
  });

  it('call the network commands by their Rust names', async () => {
    invoke.mockResolvedValue(undefined);
    await ipc.networkStatus();
    await ipc.recheckNetwork();
    await ipc.listInterfaces();
    await ipc.checkConnection();
    await ipc.publicIp();
    await ipc.validateWireGuardConfig('[Interface]');
    await ipc.importWireGuardConfig('[Interface]', 'home.conf');
    expect(invoke.mock.calls).toEqual([
      ['network_status'],
      ['recheck_network'],
      ['list_interfaces'],
      ['check_connection'],
      ['public_ip'],
      ['validate_wireguard_config', { text: '[Interface]' }],
      ['import_wireguard_config', { text: '[Interface]', name: 'home.conf' }],
    ]);
  });
});

describe('licenseSummary', () => {
  it('reads the per-license counts', () => {
    const text = [
      '# Third-party notices',
      '### Number of crates per license',
      '',
      '- MIT License: 494',
      '- Apache License 2.0: 21',
      '',
      '### License texts',
      '- Not: 1',
    ].join('\n');
    expect(settings.licenseSummary(text)).toEqual([
      { license: 'MIT License', count: 494 },
      { license: 'Apache License 2.0', count: 21 },
    ]);
    expect(settings.licenseSummary('nothing')).toEqual([]);
  });
});
