// Run with native Windows Node against an app in a disposable profile with one fixture queued.
// Usage: node scripts/benchmark-player.mjs OUTPUT.json [playback|decoder4k]
// LUMEN_BENCH_CDP_PORT overrides the default isolated-app port, 46129.
// Session-bearing stream URLs stay inside the WebView and are never printed or saved.
import assert from 'node:assert/strict';
import { writeFileSync } from 'node:fs';
import { setTimeout as delay } from 'node:timers/promises';

const output = process.argv[2];
const cdpPort = Number(process.env.LUMEN_BENCH_CDP_PORT ?? 46129);
assert(output, 'An output JSON path is required');
const mode = process.argv[3] ?? 'playback';
assert(['playback', 'decoder4k'].includes(mode));
let page;
for (let i = 0; i < 80 && !page; i++) {
  try {
    page = (await (await fetch(`http://127.0.0.1:${cdpPort}/json`)).json()).find(
      (p) => p.type === 'page',
    );
  } catch {
    // The app may still be opening its debugging endpoint.
  }
  if (!page) await delay(250);
}
assert(page, 'Start the disposable app with remote debugging on the selected debugging port');
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  ws.onopen = resolve;
  ws.onerror = reject;
});
let sequence = 0;
const pending = new Map();
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.id) {
    pending.get(m.id)?.(m);
    pending.delete(m.id);
  }
};
async function evaluate(expression) {
  const id = ++sequence;
  let timer;
  const reply = new Promise((resolve, reject) => {
    timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error('The app did not answer the benchmark command'));
    }, 30000);
    pending.set(id, resolve);
  });
  ws.send(
    JSON.stringify({
      id,
      method: 'Runtime.evaluate',
      params: { expression, awaitPromise: true, returnByValue: true, userGesture: true },
    }),
  );
  let r;
  try {
    r = await reply;
  } finally {
    clearTimeout(timer);
  }
  const exception =
    r.result?.exceptionDetails?.exception?.description?.split('\n')[0] ??
    r.result?.exceptionDetails?.text ??
    r.error?.message;
  assert(
    !exception,
    `WebView evaluation failed: ${(exception ?? '').replace(/https?:\/\/\S+/g, '[URL omitted]')}`,
  );
  return r.result.result.value;
}
async function until(expression, timeout = 120000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const value = await evaluate(expression);
    if (value) return value;
    await delay(100);
  }
  throw new Error('The requested playback stage did not complete before its deadline');
}

async function watch(milliseconds) {
  await evaluate(`(() => {
    const b=window.__streamBench,q=b.video.getVideoPlaybackQuality();
    b.watching=true;b.watchStart={time:performance.now(),frames:q.totalVideoFrames,dropped:q.droppedVideoFrames};
  })()`);
  await delay(milliseconds);
  await evaluate(`(() => {
    const b=window.__streamBench,q=b.video.getVideoPlaybackQuality(),start=b.watchStart;
    b.watching=false;
    b.watches.push({elapsedMs:performance.now()-start.time,frames:q.totalVideoFrames-start.frames,droppedFrames:q.droppedVideoFrames-start.dropped});
    if(b.waitingSince!==null){b.stallMs+=performance.now()-b.waitingSince;b.waitingSince=null;}
  })()`);
}

let result;
try {
  const queue = await evaluate(
    'window.__TAURI_INTERNALS__.invoke("load_queue").then(q=>({count:q.items.length,fixture:q.items[0]?.id}))',
  );
  assert.deepEqual(
    queue,
    { count: 1, fixture: 'benchmark-fixture' },
    'Refusing to change a personal queue',
  );
  const network = await evaluate(
    'window.__TAURI_INTERNALS__.invoke("network_status").then(s=>({mode:s.mode,allowed:s.trafficAllowed,udp:s.udpAllowed}))',
  );
  if (mode === 'playback') {
    assert.equal(network.mode, 'wireGuard');
    await until(
      'window.__TAURI_INTERNALS__.invoke("network_status").then(s=>s.trafficAllowed)',
      15000,
    );
  }
  await evaluate(`(() => {
    window.__streamBench={startedAt:performance.now(),firstFrameMs:null,attachedMs:null,seeks:[],watches:[],stalls:0,stallMs:0,watching:false,waitingSince:null,stopped:false};
    const b=window.__streamBench;
    b.attach=v=>{
      if(b.video)return;
      b.video=v; v.muted=true; b.attachedMs=performance.now()-b.startedAt;
      v.addEventListener('waiting',()=>{if(b.watching && !v.seeking && b.waitingSince===null){b.stalls++;b.waitingSince=performance.now();}});
      v.addEventListener('playing',()=>{if(b.waitingSince!==null){b.stallMs+=performance.now()-b.waitingSince;b.waitingSince=null;}});
      v.addEventListener('error',()=>{b.mediaError=v.error?.code??null;});
      const frame=(_,meta)=>{
        if(b.stopped)return;
        const now=performance.now();
        if(b.firstFrameMs===null)b.firstFrameMs=now-b.startedAt;
        const seek=b.pendingSeek;
        if(seek && Math.abs(meta.mediaTime-seek.target)<0.6){
          if(seek.firstFrameMs===null)seek.firstFrameMs=now-seek.startedAt;
          if(!v.paused){seek.resumeMs=now-seek.startedAt;b.pendingSeek=null;}
        }
        v.requestVideoFrameCallback(frame);
      };
      v.requestVideoFrameCallback(frame);
      v.play().catch(()=>{});
    };
  })()`);
  if (mode === 'playback') {
    await evaluate(
      '[...document.querySelectorAll("nav[aria-label=Main] button")].find(b=>b.querySelector(".label")?.textContent.trim().startsWith("Playing")).click()',
    );
    await until(
      '[...document.querySelectorAll("button")].some(b=>b.textContent.trim()==="Play first")',
      10000,
    );
    await evaluate(`(() => {
      const b=window.__streamBench;
      b.observer=new MutationObserver(()=>{const v=document.querySelector('.stage video');if(v)b.attach(v);});
      b.observer.observe(document.body,{childList:true,subtree:true});
      b.startedAt=performance.now();
      [...document.querySelectorAll('button')].find(button=>button.textContent.trim()==='Play first').click();
    })()`);
  } else {
    await evaluate(`(() => {
      const b=window.__streamBench; const v=document.createElement('video');
      v.style.cssText='position:fixed;inset:10%;width:80%;height:80%;z-index:99999;background:black';
      v.controls=true;v.playsInline=true;document.body.appendChild(v);
      b.startedAt=performance.now();v.src='http://127.0.0.1:46128/4k-pattern.mp4';b.attach(v);
    })()`);
  }
  console.log(`Waiting for ${mode} first decoded frame`);
  await until('window.__streamBench.firstFrameMs!==null');
  const dimensions = await evaluate(
    '({width:window.__streamBench.video.videoWidth,height:window.__streamBench.video.videoHeight})',
  );
  if (mode === 'decoder4k') assert.deepEqual(dimensions, { width: 3840, height: 2160 });
  await watch(mode === 'decoder4k' ? 3000 : 5000);
  for (const fraction of [0.5, 0.9, 0.25]) {
    await evaluate(`(() => {
      const b=window.__streamBench,v=b.video;
      const seek={fraction:${fraction},target:v.duration*${fraction},startedAt:performance.now(),firstFrameMs:null,resumeMs:null};
      b.seeks.push(seek);b.pendingSeek=seek;v.currentTime=seek.target;v.play().catch(()=>{});
    })()`);
    await until('window.__streamBench.pendingSeek===null', 45000);
    await watch(mode === 'decoder4k' ? 500 : 1500);
  }
  result = await evaluate(`(() => {
    const b=window.__streamBench,v=b.video,q=v.getVideoPlaybackQuality();
    return {success:true,firstFrameMs:b.firstFrameMs,timeToVideoElementMs:b.attachedMs,width:v.videoWidth,height:v.videoHeight,duration:v.duration,seeks:b.seeks.map(s=>({fraction:s.fraction,firstFrameMs:s.firstFrameMs,resumeMs:s.resumeMs})),watches:b.watches,stalls:b.stalls,stallMs:b.stallMs,totalFrames:q.totalVideoFrames,droppedFrames:q.droppedVideoFrames,mediaError:b.mediaError??null};
  })()`);
  if (mode === 'playback') {
    const stats = await evaluate(
      `(() => {const v=window.__streamBench.video,id=Number(new URL(v.src).pathname.split('/').at(-1));return window.__TAURI_INTERNALS__.invoke('stream_stats',{streamId:id}).then(s=>({engine:s.engine,peersLive:s.peersLive,peersSeen:s.peersSeen,fileDownloaded:s.fileDownloaded,playerFirstFrameMs:s.firstFrameMs,lastSeekMs:s.lastSeekMs}));})()`,
    );
    result.stats = stats;
  }
  result.mode = mode;
  result.network = network;
} catch (error) {
  result = { success: false, mode, error: error.message };
} finally {
  await evaluate(`(async () => {
    const b=window.__streamBench;if(!b)return;b.stopped=true;b.observer?.disconnect();
    const v=b.video;if(!v)return;v.pause();
    const url=new URL(v.src);if(url.pathname.startsWith('/s/')){
      const id=Number(url.pathname.split('/').at(-1));await window.__TAURI_INTERNALS__.invoke('stop_stream',{streamId:id}).catch(()=>{});
    }else{v.removeAttribute('src');v.load();v.remove();}
  })()`).catch(() => {});
  ws.close();
}
result.checkedAt = new Date().toISOString();
writeFileSync(output, JSON.stringify(result, null, 2));
console.log(JSON.stringify(result));
if (!result.success) process.exitCode = 1;
