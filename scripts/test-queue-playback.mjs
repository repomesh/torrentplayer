// Real desktop regression, using generated audio, a loopback seeder and a fresh profile.
// Windows: node scripts/test-queue-playback.mjs <debug TorrentPlayer.exe> <torrentplayer-bench.exe>
// Build with `npm --prefix ui run tauri -- build --debug --no-bundle` first.
// Keeps its isolated profile and logs under target/queue-playback-* for inspection.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createServer as createSocketServer } from 'node:net';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const appPath = path.resolve(process.argv[2] ?? 'target/debug/TorrentPlayer.exe');
const benchPath = path.resolve(process.argv[3] ?? 'target/release/torrentplayer-bench.exe');
assert(
  existsSync(appPath) && existsSync(benchPath),
  'Build the debug app and benchmark seeder first',
);
mkdirSync('target', { recursive: true });
const out = path.resolve(mkdtempSync('target/queue-playback-'));
const profile = path.join(out, 'profile');
for (const dir of ['config', 'data', 'local'])
  mkdirSync(path.join(profile, dir), { recursive: true });
writeFileSync(
  path.join(profile, 'config/settings.json'),
  JSON.stringify({
    version: 3,
    network: { mode: 'direct' },
    search: { preloadTopResult: false },
  }),
);

// Two minutes of silent PCM WAV: actual browser decoding, no codecs or network fixtures needed.
const samples = 120 * 8000;
const wav = Buffer.alloc(44 + samples * 2);
wav.write('RIFF');
wav.writeUInt32LE(wav.length - 8, 4);
wav.write('WAVEfmt ', 8);
wav.writeUInt32LE(16, 16);
wav.writeUInt16LE(1, 20);
wav.writeUInt16LE(1, 22);
wav.writeUInt32LE(8000, 24);
wav.writeUInt32LE(16000, 28);
wav.writeUInt16LE(2, 32);
wav.writeUInt16LE(16, 34);
wav.write('data', 36);
wav.writeUInt32LE(samples * 2, 40);
writeFileSync(path.join(out, 'playing.wav'), wav);
writeFileSync(path.join(out, 'up-next.wav'), wav);

const children = [];
function launch(executable, args, name, env = process.env) {
  const child = spawn(executable, args, {
    env,
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  children.push(child);
  let log = '';
  const record = (bytes) => {
    log += bytes.toString();
    writeFileSync(path.join(out, `${name}.log`), log);
  };
  child.stdout.on('data', record);
  child.stderr.on('data', record);
  child.on('error', (error) => record(Buffer.from(error.message)));
  return { child, log: () => log };
}
async function until(check, description, timeout = 20000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const result = await check();
    if (result) return result;
    await delay(100);
  }
  throw new Error(`Timed out: ${description}`);
}
function infoHash(torrent) {
  function end(at) {
    const kind = torrent[at];
    if (kind === 105) return torrent.indexOf(101, at) + 1;
    if (kind === 100 || kind === 108) {
      at += 1;
      while (torrent[at] !== 101) at = end(at);
      return at + 1;
    }
    const colon = torrent.indexOf(58, at);
    return colon + 1 + Number(torrent.toString('ascii', at, colon));
  }
  for (let at = 1; torrent[at] !== 101;) {
    const colon = torrent.indexOf(58, at);
    const start = end(at);
    const key = torrent.toString('ascii', colon + 1, start);
    const next = end(start);
    if (key === 'info')
      return createHash('sha1').update(torrent.subarray(start, next)).digest('hex');
    at = next;
  }
  throw new Error('Seeder did not create an info dictionary');
}
let ws;
const results = [];
let sequence = 0;
const pending = new Map();
async function protocol(method, params) {
  const id = ++sequence;
  let timer;
  const reply = new Promise((resolve, reject) => {
    timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error('WebView did not answer'));
    }, 15000);
    pending.set(id, resolve);
  });
  ws.send(
    JSON.stringify({
      id,
      method,
      params,
    }),
  );
  try {
    const message = await reply;
    assert(!message.error && !message.result?.exceptionDetails, 'WebView command failed');
    return message.result;
  } finally {
    clearTimeout(timer);
  }
}
async function evaluate(expression) {
  const result = await protocol('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
    userGesture: true,
  });
  return result.result.value;
}
const invoke = (cmd, args = {}) =>
  evaluate(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)})`);
const queue = () => invoke('load_queue');
let lastStream = null;
const media = async () => {
  const value = await evaluate(`(() => { const v = document.querySelector('.player video');
    return v ? { paused: v.paused, time: v.currentTime, duration: v.duration, seeking: v.seeking,
      streamId: Number(new URL(v.currentSrc || v.src).pathname.split('/').pop()) } : null; })()`);
  if (value) lastStream = value.streamId;
  return value;
};
const click = (selector) =>
  evaluate(`(() => { const button=document.querySelector(${JSON.stringify(selector)});
  if (!button || button.disabled) return false; button.click(); return true; })()`);
const activeRow = '.leaf[aria-current="true"]';
async function step(name, work) {
  await work();
  results.push(name);
  console.log(`PASS ${name}`);
}
async function stopped() {
  await until(async () => (await media()) === null, 'playback closes');
  await delay(400);
  assert.equal(await media(), null, 'Next file must not autoplay');
  assert(lastStream !== null, 'Observed a real stream before stopping');
  await until(
    () =>
      evaluate(`window.__TAURI_INTERNALS__.invoke('stream_stats', {streamId: ${JSON.stringify(lastStream)}})
    .then(() => false, () => true)`),
    'backend stream unpublished',
  );
}

try {
  const seeds = ['playing', 'up-next'].map((name) =>
    launch(
      benchPath,
      ['seed', path.join(out, `${name}.wav`), path.join(out, `${name}.torrent`)],
      `seed-${name}`,
    ),
  );
  const peer = await until(
    () => seeds[0].log().match(/seeding at (127\.0\.0\.1:\d+)/)?.[1],
    'local seeder',
  );
  await until(() => existsSync(path.join(out, 'up-next.torrent')), 'upcoming torrent');
  const nextHash = infoHash(readFileSync(path.join(out, 'up-next.torrent')));
  const nextSource = `magnet:?xt=urn:btih:${nextHash}`;
  writeFileSync(
    path.join(profile, 'data/queue.json'),
    JSON.stringify({
      version: 1,
      current: null,
      items: [
        {
          id: 'up-next',
          source: nextSource,
          infoHash: null,
          fileIndex: null,
          title: 'up-next.wav',
          torrentName: null,
          sizeBytes: wav.length,
          paused: true,
          played: false,
        },
      ],
    }),
  );
  const portServer = createSocketServer();
  await new Promise((resolve) => portServer.listen(0, '127.0.0.1', resolve));
  const port = portServer.address().port;
  await new Promise((resolve) => portServer.close(resolve));
  launch(appPath, [], 'app', {
    ...process.env,
    TORRENTPLAYER_PROFILE_DIR: profile,
    WEBVIEW2_USER_DATA_FOLDER: path.join(profile, 'webview'),
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${String(port)} --autoplay-policy=no-user-gesture-required`,
    TORRENTPLAYER_DEV_AUTOPLAY: `${path.join(out, 'playing.torrent')}|0||${peer}`,
  });
  const page = await until(
    async () => {
      try {
        return (await (await fetch(`http://127.0.0.1:${String(port)}/json`)).json()).find(
          (p) => p.type === 'page',
        );
      } catch {
        return null;
      }
    },
    'isolated WebView',
    45000,
  );
  ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = reject;
  });
  ws.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (message.id) {
      pending.get(message.id)?.(message);
      pending.delete(message.id);
    }
  };
  await until(
    async () => {
      const v = await media();
      return v && !v.paused && v.time > 1;
    },
    'real audio playing',
    45000,
  );
  await step(
    'direct Play creates a visible current queue entry alongside existing items',
    async () => {
      const q = await until(async () => {
        const q = await queue();
        return q.items.length === 2 && q.current ? q : null;
      }, 'saved playing entry');
      assert.equal(q.items.find((item) => item.id === q.current)?.fileIndex, 0);
      assert(await evaluate(`!!document.querySelector('${activeRow} button.stop')`));
      const shot = await protocol('Page.captureScreenshot', { format: 'png' });
      writeFileSync(path.join(out, 'playing-queue.png'), Buffer.from(shot.data, 'base64'));
    },
  );
  await step('queue Pause and Resume control playback without pausing transfers', async () => {
    assert(await click(`${activeRow} button.pause`));
    const at = await until(async () => {
      const v = await media();
      return v?.paused ? v.time : null;
    }, 'video paused');
    await delay(400);
    assert(Math.abs((await media()).time - at) < 0.1);
    assert.equal((await queue()).items.find((i) => i.id !== 'up-next').paused, false);
    assert(
      await evaluate(
        `document.querySelector('${activeRow} button.pause').getAttribute('aria-label').startsWith('Resume playback')`,
      ),
    );
    assert(await click(`${activeRow} button.pause`));
    await until(async () => {
      const v = await media();
      return v && !v.paused && v.time > at + 0.2;
    }, 'resume');
  });
  await step('10-second buttons seek backward and forward while staying paused', async () => {
    assert(await click(`${activeRow} button.pause`));
    await until(async () => (await media())?.paused, 'pause before skipping');
    await evaluate('document.querySelector(".player video").currentTime = 20');
    await until(async () => {
      const v = await media();
      return v && !v.seeking && Math.abs(v.time - 20) < 0.1;
    }, 'seek to 20 seconds');
    assert(await click('.controls button[aria-label="Rewind 10 seconds"]'));
    await until(async () => {
      const v = await media();
      return v?.paused && !v.seeking && Math.abs(v.time - 10) < 0.1;
    }, 'rewind to 10 seconds');
    assert(await click('.controls button[aria-label="Forward 10 seconds"]'));
    await until(async () => {
      const v = await media();
      return v?.paused && !v.seeking && Math.abs(v.time - 20) < 0.1;
    }, 'forward to 20 seconds');
    await evaluate(`document.querySelector('button[aria-label="Rewind 10 seconds"]').focus()`);
    await protocol('Input.dispatchKeyEvent', {
      type: 'rawKeyDown',
      key: ' ',
      code: 'Space',
      windowsVirtualKeyCode: 32,
    });
    await protocol('Input.dispatchKeyEvent', {
      type: 'keyUp',
      key: ' ',
      code: 'Space',
      windowsVirtualKeyCode: 32,
    });
    await until(async () => {
      const v = await media();
      return v?.paused && !v.seeking && Math.abs(v.time - 10) < 0.1;
    }, 'Space activates rewind without playing');
    await evaluate(`document.querySelector('button[aria-label="Forward 10 seconds"]').focus()`);
    await protocol('Input.dispatchKeyEvent', {
      type: 'keyDown',
      key: 'Enter',
      code: 'Enter',
      windowsVirtualKeyCode: 13,
      text: '\r',
    });
    await protocol('Input.dispatchKeyEvent', {
      type: 'keyUp',
      key: 'Enter',
      code: 'Enter',
      windowsVirtualKeyCode: 13,
    });
    await until(async () => {
      const v = await media();
      return v?.paused && !v.seeking && Math.abs(v.time - 20) < 0.1;
    }, 'Enter activates forward without playing');
  });
  await step('10-second buttons clamp at the start and end', async () => {
    await evaluate('document.querySelector(".player video").currentTime = 3');
    await until(async () => {
      const v = await media();
      return v && !v.seeking && Math.abs(v.time - 3) < 0.1;
    }, 'seek near start');
    assert(await click('.controls button[aria-label="Rewind 10 seconds"]'));
    await until(async () => {
      const v = await media();
      return v?.paused && !v.seeking && v.time === 0;
    }, 'rewind clamps at zero');
    assert(
      await evaluate(`document.querySelector('button[aria-label="Rewind 10 seconds"]').disabled`),
    );
    await evaluate('const v=document.querySelector(".player video"); v.currentTime=v.duration-3');
    await until(async () => {
      const v = await media();
      return v && !v.seeking && v.time > v.duration - 4;
    }, 'seek near end');
    assert(await click('.controls button[aria-label="Forward 10 seconds"]'));
    await until(async () => {
      const v = await media();
      return v?.paused && !v.seeking && Math.abs(v.time - v.duration) < 0.1;
    }, 'forward clamps at duration');
    assert(
      await evaluate(`document.querySelector('button[aria-label="Forward 10 seconds"]').disabled`),
    );
    assert(await click(`${activeRow} button.pause`));
    await until(async () => {
      const v = await media();
      return v && !v.paused && v.time > 0.2 && v.time < 2;
    }, 'replay after reaching end');
  });
  await step('10-second buttons keep active playback playing after a seek', async () => {
    const before = (await media()).time;
    assert(await click('.controls button[aria-label="Forward 10 seconds"]'));
    await until(async () => {
      const v = await media();
      return v && !v.paused && !v.seeking && v.time >= before + 9.9 && v.time < before + 12;
    }, 'forward while playing');
    const after = (await media()).time;
    assert(await click('.controls button[aria-label="Rewind 10 seconds"]'));
    await until(async () => {
      const v = await media();
      return (
        v &&
        !v.paused &&
        !v.seeking &&
        v.time >= Math.max(0, after - 10) - 0.1 &&
        v.time < after - 8
      );
    }, 'rewind while playing');
  });
  await step('Stop closes the stream and keeps its queue entry', async () => {
    const q = await queue();
    assert(await click(`${activeRow} button.stop`));
    await stopped();
    assert.deepEqual(
      (await queue()).items.map((i) => i.id),
      q.items.map((i) => i.id),
    );
  });
  const replay = async () => {
    assert(await click('.leaf:not([data-node="i:up-next"]) button.main'));
    await until(async () => {
      const v = await media();
      return v && !v.paused && v.time > 0.2;
    }, 'replay');
    assert.equal((await queue()).items.length, 2);
  };
  await step('replaying a queued file creates no duplicate', replay);
  await step(
    'Stop & remove closes playback, preserves progress, and does not play next',
    async () => {
      await evaluate('document.querySelector(".player video").currentTime = 15');
      await until(async () => (await media())?.time > 14, 'seek');
      assert(await click(`${activeRow} button.remove`));
      await stopped();
      const q = await until(async () => {
        const q = await queue();
        return q.items.length === 1 ? q : null;
      }, 'removed entry saved');
      assert.equal(q.items[0].id, 'up-next');
      const items = await invoke('library_items');
      assert(
        items.some((item) => item.positionSecs >= 14),
        'Final position saved',
      );
    },
  );
  const undo = async () => {
    assert(
      await evaluate(`(() => { const button=[...document.querySelectorAll('.toast button')].find((b)=>b.textContent.trim()==='Undo');
      if(!button) return false; button.click(); return true; })()`),
    );
    await until(async () => (await queue()).items.length === 2, 'Undo saved');
    await stopped();
  };
  await step('Undo restores the entry without restarting playback', undo);
  await step('unticking the active file stops playback too', async () => {
    await replay();
    assert(await click(`${activeRow} input.tick`));
    await stopped();
    await until(async () => (await queue()).items.length === 1, 'untick saved');
    await undo();
  });
  await step('Clear all stops playback and Undo restores the queue without autoplay', async () => {
    await replay();
    assert(await click('button.more'));
    assert(
      await evaluate(
        `(() => { const b=[...document.querySelectorAll('#queue-menu button')].find(b=>b.textContent.trim()==='Clear all…'); b?.click(); return !!b; })()`,
      ),
    );
    assert(await click('#queue-menu button.danger'));
    await stopped();
    await until(async () => (await queue()).items.length === 0, 'clear saved');
    await undo();
  });
  await step('removing from Files also closes the active backend stream', async () => {
    await replay();
    assert(await click('nav[aria-label="Main"] button[title="Files of the last opened torrent"]'));
    await until(() => evaluate('!!document.querySelector(".explorer")'), 'Files view');
    assert(await click('.explorer button[aria-label^="Stop and remove"]'));
    await until(async () => (await queue()).items.length === 1, 'Files removal saved');
    await stopped();
    assert(await click('nav[aria-label="Main"] button[title="The queue"]'));
    await undo();
  });
  await step('removing an upcoming file leaves current playback running', async () => {
    await replay();
    assert(await click('.leaf[data-node="i:up-next"] button.remove'));
    await until(async () => (await queue()).items.length === 1, 'upcoming removal saved');
    const v = await media();
    assert(v && !v.paused);
  });
  await step('removing a queued file during Play cancels the pending start', async () => {
    assert(await click(`${activeRow} button.stop`));
    await stopped();
    assert(
      await evaluate(`(() => {
      const row=document.querySelector('.leaf');
      if(!row) return false;
      row.querySelector('button.main').click();
      row.querySelector('button.remove').click();
      return true;
    })()`),
    );
    await until(async () => (await queue()).items.length === 0, 'pending play removed');
    await stopped();
    assert(
      await evaluate('!!document.querySelector(".watch") && !document.querySelector(".explorer")'),
    );
    assert.equal((await queue()).items.length, 0, 'Removed file stays removed');
  });
  console.log(`${String(results.length)} desktop regression checks passed. Artifacts: ${out}`);
} catch (error) {
  process.exitCode = 1;
  console.error(error.message);
  if (ws?.readyState === WebSocket.OPEN) {
    try {
      console.error(
        JSON.stringify({
          media: await media(),
          focus: await evaluate('document.activeElement?.getAttribute("aria-label")'),
        }),
      );
    } catch {
      // The WebView may already have closed.
    }
  }
  console.error(`Artifacts: ${out}`);
} finally {
  writeFileSync(
    path.join(out, 'results.json'),
    JSON.stringify({ passed: results, failed: process.exitCode === 1 }, null, 2),
  );
  ws?.close();
  for (const child of children.reverse()) child.kill();
}
