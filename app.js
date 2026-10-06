(() => {
  'use strict';
  const $ = (id) => document.getElementById(id);
  const state = { data: null };
  const setStatus = (message, kind = 'muted', live = '') => {
    const el = $('status'); el.className = `status ${kind}`;
    el.firstElementChild.textContent = message; $('liveLatency').textContent = live;
  };
  const download = (name, text, type) => {
    const blob = new Blob([text], { type }); const a = document.createElement('a');
    a.href = URL.createObjectURL(blob); a.download = name; a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  };
  const srtTime = (seconds) => {
    const ms = Math.max(0, Math.round(seconds * 1000));
    const h = Math.floor(ms / 3600000); const m = Math.floor(ms % 3600000 / 60000);
    const s = Math.floor(ms % 60000 / 1000); const x = ms % 1000;
    return `${String(h).padStart(2,'0')}:${String(m).padStart(2,'0')}:${String(s).padStart(2,'0')},${String(x).padStart(3,'0')}`;
  };
  const makeSrt = (segments) => segments.map((x, i) => `${i+1}\n${srtTime(x.start)} --> ${srtTime(x.start + Math.max(.1, x.duration))}\n${x.text}\n`).join('\n');
  const showResult = (d, clientMs) => {
    state.data = d; $('result').classList.add('show');
    $('title').textContent = d.video.title || d.video.id; $('channel').textContent = d.video.channel || '—';
    $('language').textContent = `${d.language.name || d.language.code}${d.language.generated ? ' · auto' : ' · manual'}`;
    $('segments').textContent = d.transcript.segments.length.toLocaleString();
    $('serverMs').textContent = `${Math.round(d.meta.serverMs)} ms · ${d.meta.method}`;
    $('clientMs').textContent = `${Math.round(clientMs)} ms`;
    $('transcript').textContent = d.transcript.text;
    $('notice').style.display = 'block'; $('notice').textContent = `Acquisition: ${Math.round(d.meta.acquisitionMs)} ms · ${d.meta.charCount.toLocaleString()} chars · ${d.meta.wordCount.toLocaleString()} words.`;
  };
  const extract = async () => {
    const url = $('url').value.trim();
    if (!url) { setStatus('Paste a public YouTube URL first.', 'bad'); return; }
    $('extract').disabled = true; $('result').classList.remove('show'); $('notice').style.display = 'none';
    const started = performance.now(); setStatus('Extracting…', 'muted', '0 ms');
    let ticker = setInterval(() => $('liveLatency').textContent = `${Math.round(performance.now() - started)} ms`, 50);
    try {
      const response = await fetch('/api/transcript', { method: 'POST', headers: {'content-type':'application/json','accept':'application/json'}, body: JSON.stringify({ url }) });
      const clientMs = performance.now() - started; clearInterval(ticker); $('liveLatency').textContent = `${Math.round(clientMs)} ms`;
      let payload; try { payload = await response.json(); } catch { throw new Error('Backend returned a non-JSON response.'); }
      if (!response.ok || !payload.ok) throw new Error(payload?.error?.message || 'Transcript extraction failed.');
      showResult(payload, clientMs); setStatus('Transcript ready.', 'good', `${Math.round(clientMs)} ms total`);
    } catch (err) {
      clearInterval(ticker); const clientMs = performance.now() - started; $('liveLatency').textContent = `${Math.round(clientMs)} ms`;
      setStatus(err?.message || 'Extraction failed.', 'bad');
    } finally { $('extract').disabled = false; }
  };
  $('extract').addEventListener('click', extract); $('url').addEventListener('keydown', e => { if (e.key === 'Enter') extract(); });
  $('copy').addEventListener('click', async () => { if (!state.data) return; await navigator.clipboard.writeText(state.data.transcript.text); setStatus('Copied transcript text.', 'good'); });
  $('txt').addEventListener('click', () => { if (state.data) download(`${state.data.video.id}.txt`, state.data.transcript.text, 'text/plain'); });
  $('srt').addEventListener('click', () => { if (state.data) download(`${state.data.video.id}.srt`, makeSrt(state.data.transcript.segments), 'application/x-subrip'); });
  $('json').addEventListener('click', () => { if (state.data) download(`${state.data.video.id}.json`, JSON.stringify(state.data, null, 2), 'application/json'); });
  const saved = localStorage.getItem('arix-theme'); if (saved === 'light') document.documentElement.classList.add('light');
  $('theme').addEventListener('click', () => { document.documentElement.classList.toggle('light'); localStorage.setItem('arix-theme', document.documentElement.classList.contains('light') ? 'light' : 'dark'); });
})();
