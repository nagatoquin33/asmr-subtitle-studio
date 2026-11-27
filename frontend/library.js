'use strict';

(() => {
  if (!API.isDesktop()) return;

  const $ = (id) => document.getElementById(id);
  const library = $('libraryView');
  const browse = $('libraryBrowse');
  const detail = $('libraryDetail');
  const media = $('libraryMedia');
  const player = $('libraryPlayer');
  const caption = $('libraryCaption');
  const drawer = $('librarySubtitleDrawer');
  const drawerBody = $('librarySubtitleListBody');
  const collator = new Intl.Collator('zh-CN', { numeric: true, sensitivity: 'base' });
  const taskStatuses = new Map();

  const STORAGE_KEY = 'asmr_library_player_v1';
  const LOOP_MODES = [
    { id: 'order', label: '顺序', title: '循环模式：顺序播放' },
    { id: 'one', label: '单曲', title: '循环模式：单曲循环' },
    { id: 'all', label: '列表', title: '循环模式：列表循环' },
  ];
  const RATES = [0.75, 1, 1.25, 1.5];
  const SLEEP_OPTIONS = [
    { id: 'off', label: '定时', title: '睡眠定时：关闭' },
    { id: '15', minutes: 15, label: '15分', title: '睡眠定时：15 分钟后暂停' },
    { id: '30', minutes: 30, label: '30分', title: '睡眠定时：30 分钟后暂停' },
    { id: '60', minutes: 60, label: '60分', title: '睡眠定时：60 分钟后暂停' },
    { id: 'track', label: '本曲后', title: '睡眠定时：当前曲目播完后暂停' },
  ];

  function loadPrefs() {
    try {
      const parsed = JSON.parse(localStorage.getItem(STORAGE_KEY) || '{}');
      return {
        volume: Number.isFinite(parsed.volume) ? Math.min(1, Math.max(0, parsed.volume)) : 0.8,
        rate: RATES.includes(parsed.rate) ? parsed.rate : 1,
        loop: LOOP_MODES.some((m) => m.id === parsed.loop) ? parsed.loop : 'order',
        captions: parsed.captions !== false,
        resume: parsed.resume && typeof parsed.resume === 'object' ? parsed.resume : {},
      };
    } catch (_) {
      return { volume: 0.8, rate: 1, loop: 'order', captions: true, resume: {} };
    }
  }

  const prefs = loadPrefs();
  function savePrefs() {
    try {
      const entries = Object.entries(prefs.resume).slice(-80);
      prefs.resume = Object.fromEntries(entries);
      localStorage.setItem(STORAGE_KEY, JSON.stringify(prefs));
    } catch (_) {
      // 忽略本地存储空间限制。
    }
  }

  let works = [];
  let currentWork = null;
  let selectedWorkId = null;
  let playing = null;
  let busy = false;
  let loadingTrack = false;
  let captionsEnabled = prefs.captions;
  let drawerOpen = false;
  let videoVisible = false;
  let refreshTimer;
  let workspace = 'library';
  let lastSavedTime = 0;
  let sleepIndex = 0;
  let sleepEndsAt = 0;
  let sleepInterval = null;
  let activeCueNodes = [];

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function button(text, className, action) {
    const btn = el('button', className, text);
    btn.type = 'button';
    btn.addEventListener('click', action);
    return btn;
  }

  function message(text) {
    $('libraryMessage').textContent = text;
    $('libraryMessage').hidden = !text;
  }

  function formatBytes(bytes) {
    if (!Number.isFinite(bytes) || bytes <= 0) return '0 MB';
    if (bytes >= 1073741824) return (bytes / 1073741824).toFixed(2) + ' GB';
    return (bytes / 1048576).toFixed(1) + ' MB';
  }

  function setWorkspace(next) {
    workspace = next;
    document.body.classList.toggle('show-library', next === 'library');
    library.hidden = next !== 'library';
    $('layout').hidden = next !== 'tasks';
    $('workspaceNav').querySelectorAll('button').forEach((btn) => {
      btn.setAttribute('aria-pressed', String(btn.dataset.workspace === next));
    });
  }

  function cover(work, eager = false) {
    const picture = el('span', 'lib-cover');
    picture.setAttribute('aria-hidden', 'true');
    const fallback = el('span', 'lib-record');
    picture.append(fallback);
    if (work.has_cover) {
      const img = el('img');
      img.alt = '';
      img.loading = eager ? 'eager' : 'lazy';
      img.src = API.libraryAsset(work.id, 'cover');
      img.addEventListener('error', () => img.remove());
      picture.append(img);
    }
    return picture;
  }

  function renderBrowse() {
    const pattern = $('librarySearch').value.trim().toLowerCase();
    const matches = works.filter((work) => (work.title + ' ' + (work.rj || '')).toLowerCase().includes(pattern));
    const sort = $('librarySort').value;
    if (sort === 'title') matches.sort((a, b) => collator.compare(a.title, b.title));
    if (sort === 'rj') matches.sort((a, b) => collator.compare(a.rj || 'ZZ', b.rj || 'ZZ'));
    const grid = $('libraryGrid');
    grid.replaceChildren();
    if (!matches.length) {
      const empty = el('div', 'lib-empty');
      empty.append(
        el('span', 'lib-empty-icon', '♫'),
        el('h3', '', works.length ? '找不到这部作品' : '把第一部作品放进来'),
        el('p', '', works.length ? '试试作品名中的一部分，或输入 RJ 编号。' : '选择一个作品文件夹，保留里面的音轨、封面和字幕。')
      );
      if (!works.length) empty.append(button('选择文件夹', 'btn btn-primary', pickFolder));
      grid.append(empty);
      return;
    }
    for (const work of matches) {
      const card = button('', 'lib-work-card', () => showWork(work.id));
      const title = el('strong', 'lib-work-title', work.title);
      title.title = work.title;
      const image = cover(work);
      const meta = el('span', 'lib-work-meta');
      if (work.rj) meta.append(el('span', 'lib-rj', work.rj));
      meta.append(el('span', '', work.track_count + ' 条曲目'));
      card.append(image, title, meta);
      grid.append(card);
    }
  }

  async function refreshLibrary() {
    try {
      works = await API.listLibrary();
      $('libraryCount').textContent = works.length + ' 部作品 · ' +
        works.reduce((total, work) => total + work.track_count, 0) + ' 条曲目';
      renderBrowse();
    } catch (_) {
      $('libraryCount').textContent = '读取作品库失败';
    }
  }

  function showBrowse() {
    selectedWorkId = null;
    currentWork = null;
    browse.hidden = false;
    detail.hidden = true;
    $('librarySearch').focus();
  }

  async function showWork(id) {
    selectedWorkId = id;
    setWorkspace('library');
    try {
      const work = await API.libraryDetail(id);
      if (selectedWorkId !== id) return;
      currentWork = work;
      browse.hidden = true;
      detail.hidden = false;
      renderDetail(work);
      if (playing?.work.id === work.id) {
        const fresh = work.tracks.find((track) => track.id === playing.track.id);
        if (
          fresh &&
          (fresh.has_subtitles !== playing.track.has_subtitles ||
            fresh.subtitle_version !== playing.track.subtitle_version)
        ) {
          playing.track = fresh;
          attachCaptions(fresh.subtitle_version || Date.now());
        }
      }
    } catch (_) {
      if (selectedWorkId === id && detail.hidden) selectedWorkId = null;
    }
  }

  function pathParts(track) {
    return track.path.split(/[\\/]/);
  }

  function trackName(track) {
    return pathParts(track).at(-1);
  }

  function trackTitle(track) {
    return trackName(track).replace(/\.[^.]+$/, '');
  }

  function sortedTracks(work) {
    return [...work.tracks].sort((a, b) => collator.compare(a.path, b.path));
  }

  function renderDetail(work) {
    const breadcrumb = el('nav', 'lib-breadcrumb');
    breadcrumb.setAttribute('aria-label', '作品位置');
    breadcrumb.append(button('← 全部作品', 'lib-back', showBrowse));
    const layout = el('div', 'lib-detail-layout');
    const header = el('aside', 'lib-work-head');
    const info = el('div', 'lib-work-heading');
    const meta = el('p', 'lib-work-meta');
    if (work.rj) meta.append(el('span', 'lib-rj', work.rj));
    meta.append(el('span', '', work.tracks.length + ' 条曲目'));
    info.append(el('h3', '', work.title), meta);
    const tracks = sortedTracks(work);
    const saved = prefs.resume[work.id];
    const savedTrack = saved && tracks.find((t) => t.id === saved.trackId);
    const row = el('div', 'lib-actions');
    const toQueue = tracks.filter(
      (track) => (!track.task_id || track.status === 'error' || track.status === 'canceled') && !track.has_subtitles
    );
    const playLabel = savedTrack && saved.time > 3
      ? '继续播放（' + timeString(saved.time) + '）'
      : '播放作品';
    row.append(
      button(playLabel, 'btn btn-primary', () => {
        if (savedTrack && saved.time > 3) playTrack(work, savedTrack, saved.time);
        else playTrack(work, tracks[0], 0);
      }),
      button('打开作品目录', 'btn btn-ghost', () => API.openLibraryFolder(work.id).catch(() => {})),
      button('改名', 'btn btn-ghost', () => renameWork(work)),
      button('删除作品', 'btn btn-danger-ghost', () => deleteWork(work))
    );
    info.append(row);
    header.append(cover(work, true), info);

    const toolbar = el('div', 'lib-tracks-head');
    toolbar.append(el('h4', '', '曲目目录'));
    const subtitles = el('div', 'lib-actions lib-track-actions');
    subtitles.append(
      languageSelect('源语言', 'sourceLang', [['auto', '自动检测'], ['ja', '日语'], ['zh', '中文'], ['en', '英语']]),
      languageSelect('目标语言', 'targetLang', [['zh', '中文'], ['ja', '日语'], ['en', '英语']])
    );
    const bulk = button('批量生成缺失字幕', 'btn btn-ghost', () => queueTracks(work, toQueue, bulk));
    bulk.disabled = !toQueue.length;
    subtitles.append(bulk);
    toolbar.append(subtitles);

    const trackList = el('div', 'lib-track-list');
    const groups = new Map();
    for (const track of tracks) {
      const dir = pathParts(track).slice(0, -1).join(' / ');
      if (!groups.has(dir)) groups.set(dir, []);
      groups.get(dir).push(track);
    }
    for (const [dir, members] of groups) {
      const group = el('details', 'lib-track-group');
      group.open = true;
      group.append(el('summary', '', (dir || '作品主目录') + ' · ' + members.length));
      for (const track of members) group.append(trackRow(work, track));
      trackList.append(group);
    }
    const playlist = el('div', 'lib-playlist');
    playlist.append(toolbar, trackList);
    layout.append(header, playlist);
    detail.replaceChildren(breadcrumb, layout);
    updatePlayingRows();
  }

  function languageSelect(text, target, options) {
    const label = el('label', 'lib-language', text);
    const existing = $(target);
    const select = el('select');
    for (const [value, title] of options) {
      const option = el('option', '', title);
      option.value = value;
      select.append(option);
    }
    select.value = existing.value;
    select.addEventListener('change', () => {
      existing.value = select.value;
      existing.dispatchEvent(new Event('change'));
    });
    label.append(select);
    return label;
  }

  function trackRow(work, track) {
    const row = el('div', 'lib-track');
    row.dataset.track = track.id;
    const play = button('▶', 'lib-icon-button lib-track-play', () => playTrack(work, track, 0));
    play.setAttribute('aria-label', '播放 ' + trackTitle(track));
    const name = button(trackTitle(track), 'lib-track-name', () => playTrack(work, track, 0));
    name.title = track.path;
    const extension = trackName(track).split('.').at(-1).toUpperCase();
    const status = track.has_subtitles ? '有字幕'
      : ({ queued: '等待中', error: '失败', canceled: '已取消', done: '处理完成' })[track.status]
        || (track.status ? '处理中' : '待生成');
    const trackMeta = el('span', 'lib-track-meta');
    const trackStatus = el('span', 'lib-track-status', status);
    trackStatus.dataset.status = track.has_subtitles ? 'ready' : (track.status || 'idle');
    trackMeta.append(el('span', 'lib-track-extension', extension), trackStatus);
    const canRetry = track.status === 'error' || track.status === 'canceled';
    const action = track.task_id && !canRetry
      ? button('字幕任务', 'btn btn-ghost', () => openTask(track.task_id))
      : button(canRetry ? '重新生成' : '生成字幕', 'btn btn-ghost', () => queueTracks(work, [track], action));
    row.append(play, name, trackMeta, action);
    return row;
  }

  async function renameWork(work) {
    const title = await askValue({ title: '修改作品名称', label: '作品名称', value: work.title });
    if (!title?.trim()) return;
    try {
      await API.renameLibraryWork(work.id, title);
      if (playing?.work.id === work.id) {
        playing.work.title = title.trim();
        $('libraryNowWork').textContent = playing.work.title;
      }
      await refreshLibrary();
      if (selectedWorkId === work.id) await showWork(work.id);
    } catch (_) {
      // 后端错误已由 API 提示。
    }
  }

  async function deleteWork(work) {
    const confirmed = await API.confirmBox(
      `确定删除作品「${work.title}」吗？\n\n这会移除作品库内的媒体副本、生成字幕与关联任务记录（原位置的源文件不受影响）。`
    );
    if (!confirmed) return;
    if (playing?.work.id === work.id) stopPlayer();
    try {
      await API.deleteLibraryWork(work.id);
      delete prefs.resume[work.id];
      savePrefs();
      toast('已删除作品：' + work.title, 'success');
      showBrowse();
      await refreshLibrary();
      if (typeof loadTasks === 'function') loadTasks();
    } catch (_) {
      // 后端错误已由 API 提示。
    }
  }

  async function queueTracks(work, tracks, action) {
    action.disabled = true;
    try {
      const ids = await API.queueLibraryTracks(
        work.id, tracks.map((track) => track.id), $('sourceLang').value, $('targetLang').value
      );
      toast(ids.length + ' 条曲目已加入字幕任务', 'success');
      if (selectedWorkId === work.id) await showWork(work.id);
      loadTasks();
    } catch (_) {
      action.disabled = false;
    }
  }

  async function openTask(id) {
    try {
      const task = await API.getTask(id);
      setWorkspace('tasks');
      upsertTask(task);
      updateListChrome();
      selectTask(id);
    } catch (_) {
      toast('无法打开该字幕任务', 'error');
    }
  }

  function setBusy(value) {
    busy = value;
    for (const id of ['libraryFolder', 'libraryFile', 'libraryDownload']) $(id).disabled = value;
  }

  async function confirmDuplicates(paths) {
    try {
      const dups = await API.checkLibraryDuplicate(paths);
      if (!dups?.length) return true;
      const names = dups
        .slice(0, 3)
        .map((d) => `「${d.existing.title}${d.existing.rj ? ' / ' + d.existing.rj : ''}」`)
        .join('、');
      const more = dups.length > 3 ? ` 等 ${dups.length} 部` : '';
      return API.confirmBox(`作品库里已有相同作品 ${names}${more}。\n\n仍要再次复制一份吗？`);
    } catch (_) {
      return true;
    }
  }

  async function importWith(run, progress) {
    if (busy) return;
    setWorkspace('library');
    setBusy(true);
    message(progress);
    try {
      const result = await run();
      const imported = result.works || [result];
      const errors = result.errors || [];
      await refreshLibrary();
      if (imported.length === 1) await showWork(imported[0].id);
      message(errors.length ? errors.join('；') : '');
      if (imported.length) toast('已导入 ' + imported.length + ' 部作品', 'success');
    } catch (err) {
      message(err.message);
    } finally {
      setBusy(false);
    }
  }

  async function pickFolder() {
    if (busy) return;
    try {
      const path = await API.pickFolder();
      if (!path) return;
      if (!(await confirmDuplicates([path]))) return;
      await importWith(() => API.importLibraryFolder(path), '正在准备复制作品文件夹…');
    } catch (err) {
      toast(err.message || String(err), 'error');
    }
  }

  async function pickFiles() {
    if (busy) return;
    try {
      const paths = await API.pickMediaFiles();
      await importPaths(paths);
    } catch (err) {
      toast(err.message || String(err), 'error');
    }
  }

  async function importPaths(paths) {
    if (!paths.length || busy) return;
    if (!(await confirmDuplicates(paths))) return;
    await importWith(() => API.importLibraryPaths(paths), '正在复制作品文件…');
  }

  async function importLink(event) {
    event.preventDefault();
    const input = $('libraryUrl');
    const urls = [...new Set(input.value.split(/\r?\n/).map((line) => line.trim()).filter(Boolean))];
    if (!urls.length || busy) return;
    await importWith(async () => {
      const works = [];
      const errors = [];
      const failed = [];
      for (const url of urls) {
        try {
          works.push(await API.importLibraryUrl(url));
        } catch (err) {
          errors.push(err.message);
          failed.push(url);
        }
      }
      input.value = failed.join('\n');
      $('libraryLinkForm').hidden = !failed.length;
      return { works, errors };
    }, '正在解析视频链接…');
  }

  function timeString(seconds) {
    if (!Number.isFinite(seconds) || seconds < 0) return '0:00';
    const whole = Math.floor(seconds);
    const hours = Math.floor(whole / 3600);
    const minutes = Math.floor(whole / 60) % 60;
    return (hours ? hours + ':' + String(minutes).padStart(2, '0') : minutes) +
      ':' + String(whole % 60).padStart(2, '0');
  }

  function persistResume(force = false) {
    if (!playing || !Number.isFinite(media.currentTime)) return;
    const now = Date.now();
    if (!force && now - lastSavedTime < 3000) return;
    lastSavedTime = now;
    const duration = Number.isFinite(media.duration) ? media.duration : 0;
    const t = duration > 10 && media.currentTime > duration - 3 ? 0 : Math.floor(media.currentTime);
    prefs.resume[playing.work.id] = {
      trackId: playing.track.id,
      time: t,
    };
    savePrefs();
  }

  function refreshTime() {
    const seek = $('librarySeek');
    const duration = Number.isFinite(media.duration) ? media.duration : 0;
    seek.max = duration;
    seek.value = media.currentTime;
    seek.disabled = !duration;
    $('libraryElapsed').textContent = timeString(media.currentTime);
    $('libraryDuration').textContent = timeString(duration);
    persistResume(false);
  }

  function refreshTransport() {
    const text = media.paused ? '播放' : '暂停';
    const toggle = $('libraryToggle');
    toggle.setAttribute('aria-label', text);
    toggle.title = text + '（Space）';
    toggle.textContent = media.paused ? '▶' : '❚❚';
  }

  function refreshLoopButton() {
    const mode = LOOP_MODES.find((m) => m.id === prefs.loop) || LOOP_MODES[0];
    const btn = $('libraryLoop');
    btn.textContent = mode.label;
    btn.title = mode.title;
    btn.classList.toggle('is-active', mode.id !== 'order');
  }

  function refreshRateButton() {
    media.playbackRate = prefs.rate;
    const btn = $('libraryRate');
    btn.textContent = prefs.rate + '×';
    btn.classList.toggle('is-active', prefs.rate !== 1);
  }

  function clearSleepTimer() {
    if (sleepInterval) {
      clearInterval(sleepInterval);
      sleepInterval = null;
    }
    sleepEndsAt = 0;
  }

  function refreshSleepButton() {
    const opt = SLEEP_OPTIONS[sleepIndex];
    const btn = $('librarySleep');
    if (opt.minutes && sleepEndsAt > 0) {
      const left = Math.max(1, Math.ceil((sleepEndsAt - Date.now()) / 60000));
      btn.textContent = left + '分';
      btn.title = `睡眠定时：约 ${left} 分钟后暂停（点击切换）`;
      btn.classList.add('is-active');
    } else {
      btn.textContent = opt.label;
      btn.title = opt.title;
      btn.classList.toggle('is-active', opt.id !== 'off');
    }
  }

  function cycleSleepTimer() {
    clearSleepTimer();
    sleepIndex = (sleepIndex + 1) % SLEEP_OPTIONS.length;
    const opt = SLEEP_OPTIONS[sleepIndex];
    if (opt.minutes) {
      sleepEndsAt = Date.now() + opt.minutes * 60000;
      sleepInterval = setInterval(() => {
        if (Date.now() >= sleepEndsAt) {
          clearSleepTimer();
          sleepIndex = 0;
          refreshSleepButton();
          if (!media.paused) {
            media.pause();
            persistResume(true);
            toast('睡眠定时已到，已暂停播放');
          }
          return;
        }
        refreshSleepButton();
      }, 10000);
      toast(`已开启睡眠定时：${opt.minutes} 分钟后暂停`);
    } else if (opt.id === 'track') {
      toast('已开启睡眠定时：本曲播完后暂停');
    }
    refreshSleepButton();
  }

  function updatePlayingRows() {
    for (const row of detail.querySelectorAll('.lib-track')) {
      const active = currentWork?.id === playing?.work.id && row.dataset.track === playing?.track.id;
      row.classList.toggle('is-playing', !!active);
    }
  }

  function setVideoVisible(value) {
    videoVisible = value && !!playing?.track.video;
    media.hidden = !videoVisible;
    $('libraryShowVideo').setAttribute('aria-expanded', String(videoVisible));
  }

  function setDrawerOpen(value) {
    drawerOpen = value && !!playing?.track.has_subtitles;
    drawer.hidden = !drawerOpen;
    $('libraryShowDrawer').setAttribute('aria-pressed', String(drawerOpen));
    if (drawerOpen) {
      const active = drawerBody.querySelector('.lib-sub-cue.is-active');
      active?.scrollIntoView({ block: 'center', behavior: 'smooth' });
    }
  }

  function renderSubtitleDrawer(textTrack) {
    drawerBody.replaceChildren();
    activeCueNodes = [];
    const cues = Array.from(textTrack?.cues || []);
    if (!cues.length) {
      drawerBody.append(el('p', 'lib-sub-empty', '这条曲目暂时没有可显示的字幕条目'));
      return;
    }
    for (const cue of cues) {
      const text = cue.getCueAsHTML().textContent.trim();
      const row = el('button', 'lib-sub-cue');
      row.type = 'button';
      row.append(el('span', 'lib-sub-cue-time', timeString(cue.startTime)), el('span', '', text));
      row.addEventListener('click', () => {
        media.currentTime = Math.max(0, cue.startTime + 0.01);
        if (media.paused) media.play().catch(() => {});
      });
      drawerBody.append(row);
      activeCueNodes.push({ cue, row });
    }
  }

  function syncActiveDrawerCues(textTrack) {
    const activeSet = new Set(Array.from(textTrack?.activeCues || []));
    let firstActive = null;
    for (const item of activeCueNodes) {
      const on = activeSet.has(item.cue);
      item.row.classList.toggle('is-active', on);
      if (on && !firstActive) firstActive = item.row;
    }
    if (drawerOpen && firstActive) {
      firstActive.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    }
  }

  function attachCaptions(versionOverride) {
    media.querySelectorAll('track').forEach((t) => t.remove());
    caption.hidden = true;
    caption.textContent = '';
    drawerBody.replaceChildren();
    activeCueNodes = [];
    const hasSub = !!playing?.track.has_subtitles;
    $('libraryShowCaption').disabled = !hasSub;
    $('libraryShowDrawer').disabled = !hasSub;
    if (!hasSub) {
      setDrawerOpen(false);
      return;
    }
    const track = el('track');
    track.kind = 'subtitles';
    track.label = '作品字幕';
    const ver = versionOverride || playing.track.subtitle_version || Date.now();
    track.src = API.libraryVttUrl(playing.work.id, playing.track.id, ver);
    media.append(track);
    const sync = () => {
      const text = Array.from(track.track.activeCues || [], (item) =>
        item.getCueAsHTML().textContent).join('\n');
      caption.textContent = text;
      caption.hidden = !captionsEnabled || !text;
      syncActiveDrawerCues(track.track);
    };
    track.addEventListener('load', () => {
      renderSubtitleDrawer(track.track);
      sync();
    });
    track.track.addEventListener('cuechange', sync);
    track.track.mode = 'hidden';
  }

  function playTrack(work, track, startAt) {
    if (!track) return;
    if (playing?.work.id === work.id && playing.track.id === track.id) {
      if (typeof startAt === 'number' && startAt >= 0) media.currentTime = startAt;
      if (media.paused && !loadingTrack) media.play().catch(() => {});
      return;
    }
    persistResume(true);
    playing = { work, track, fallback: false, startAt: typeof startAt === 'number' ? startAt : 0 };
    loadingTrack = false;
    media.src = API.libraryAsset(work.id, track.id, 'media');
    media.playbackRate = prefs.rate;
    $('libraryNowTitle').textContent = trackTitle(track);
    $('libraryNowWork').textContent = work.title;
    $('libraryNowCover').replaceChildren(cover(work, true));
    $('libraryShowVideo').hidden = !track.video;
    player.hidden = false;
    document.body.classList.add('library-playing');
    setVideoVisible(track.video);
    attachCaptions(track.subtitle_version);
    updatePlayingRows();
    const tracks = sortedTracks(work);
    const index = tracks.findIndex((item) => item.id === track.id);
    $('libraryPrev').disabled = index === 0 && prefs.loop !== 'all';
    $('libraryNext').disabled = index === tracks.length - 1 && prefs.loop !== 'all';
    media.play().catch(() => {});
  }

  function stepTrack(delta, fromEnded = false) {
    if (!playing) return;
    if (fromEnded && SLEEP_OPTIONS[sleepIndex]?.id === 'track') {
      sleepIndex = 0;
      refreshSleepButton();
      toast('本曲已播完，睡眠定时已暂停播放');
      return;
    }
    if (fromEnded && prefs.loop === 'one') {
      media.currentTime = 0;
      media.play().catch(() => {});
      return;
    }
    const tracks = sortedTracks(playing.work);
    const index = tracks.findIndex((track) => track.id === playing.track.id);
    let nextIndex = index + delta;
    if (nextIndex >= tracks.length) {
      if (prefs.loop === 'all') nextIndex = 0;
      else return;
    } else if (nextIndex < 0) {
      if (prefs.loop === 'all') nextIndex = tracks.length - 1;
      else return;
    }
    playTrack(playing.work, tracks[nextIndex], 0);
  }

  async function recoverPlayback() {
    const request = playing;
    if (!request || loadingTrack) return;
    if (request.fallback) {
      toast('无法播放这条曲目', 'error');
      return;
    }
    request.fallback = true;
    loadingTrack = true;
    try {
      toast('正在生成兼容的播放副本');
      await API.prepareLibraryPreview(request.work.id, request.track.id);
      if (playing !== request) return;
      media.src = API.libraryAsset(request.work.id, request.track.id, 'preview');
      media.playbackRate = prefs.rate;
      attachCaptions(request.track.subtitle_version);
      media.play().catch(() => {});
    } catch (_) {
      // 后端错误已由 API 提示。
    } finally {
      if (playing === request) loadingTrack = false;
    }
  }

  function stopPlayer() {
    persistResume(true);
    clearSleepTimer();
    sleepIndex = 0;
    refreshSleepButton();
    setDrawerOpen(false);
    playing = null;
    loadingTrack = false;
    media.pause();
    media.removeAttribute('src');
    attachCaptions();
    media.load();
    player.hidden = true;
    document.body.classList.remove('library-playing');
    updatePlayingRows();
  }

  $('workspaceNav').hidden = false;
  document.body.classList.add('desktop-library');
  setWorkspace('library');

  $('workspaceNav').addEventListener('click', (event) => {
    const selected = event.target.closest('button[data-workspace]');
    if (!selected) return;
    setWorkspace(selected.dataset.workspace);
    if (workspace === 'library') {
      if (selectedWorkId) showWork(selectedWorkId);
      else refreshLibrary();
    }
  });
  $('librarySearch').addEventListener('input', renderBrowse);
  $('librarySort').addEventListener('change', renderBrowse);
  $('libraryFolder').addEventListener('click', pickFolder);
  $('libraryFile').addEventListener('click', pickFiles);
  $('libraryLink').addEventListener('click', () => {
    const form = $('libraryLinkForm');
    form.hidden = !form.hidden;
    if (!form.hidden) $('libraryUrl').focus();
  });
  $('libraryLinkForm').addEventListener('submit', importLink);

  $('libraryNowCover').addEventListener('click', () => playing && showWork(playing.work.id));
  $('libraryClosePlayer').addEventListener('click', stopPlayer);
  $('libraryPrev').addEventListener('click', () => stepTrack(-1, false));
  $('libraryNext').addEventListener('click', () => stepTrack(1, false));
  $('libraryToggle').addEventListener('click', () => {
    if (loadingTrack) return;
    if (media.paused) media.play().catch(() => {});
    else media.pause();
  });
  $('librarySeek').addEventListener('input', (event) => {
    media.currentTime = Number(event.target.value);
    persistResume(true);
  });
  $('libraryVolume').value = String(prefs.volume);
  media.volume = prefs.volume;
  $('libraryVolume').addEventListener('input', (event) => {
    media.volume = Number(event.target.value);
    prefs.volume = media.volume;
    savePrefs();
  });
  $('libraryLoop').addEventListener('click', () => {
    const idx = LOOP_MODES.findIndex((m) => m.id === prefs.loop);
    prefs.loop = LOOP_MODES[(idx + 1) % LOOP_MODES.length].id;
    savePrefs();
    refreshLoopButton();
    if (playing) {
      const tracks = sortedTracks(playing.work);
      const cur = tracks.findIndex((t) => t.id === playing.track.id);
      $('libraryPrev').disabled = cur === 0 && prefs.loop !== 'all';
      $('libraryNext').disabled = cur === tracks.length - 1 && prefs.loop !== 'all';
    }
  });
  $('libraryRate').addEventListener('click', () => {
    const idx = RATES.indexOf(prefs.rate);
    prefs.rate = RATES[(idx + 1) % RATES.length];
    savePrefs();
    refreshRateButton();
  });
  $('librarySleep').addEventListener('click', cycleSleepTimer);
  $('libraryShowVideo').addEventListener('click', () => setVideoVisible(!videoVisible));
  $('libraryShowCaption').setAttribute('aria-pressed', String(captionsEnabled));
  $('libraryShowCaption').addEventListener('click', (event) => {
    captionsEnabled = !captionsEnabled;
    prefs.captions = captionsEnabled;
    savePrefs();
    event.currentTarget.setAttribute('aria-pressed', String(captionsEnabled));
    caption.hidden = !captionsEnabled || !caption.textContent;
  });
  $('libraryShowDrawer').addEventListener('click', () => setDrawerOpen(!drawerOpen));
  $('libraryCloseDrawer').addEventListener('click', () => setDrawerOpen(false));

  refreshLoopButton();
  refreshRateButton();
  refreshSleepButton();

  media.addEventListener('timeupdate', refreshTime);
  media.addEventListener('loadedmetadata', () => {
    media.playbackRate = prefs.rate;
    if (playing?.startAt > 0 && Number.isFinite(media.duration) && playing.startAt < media.duration - 2) {
      media.currentTime = playing.startAt;
      playing.startAt = 0;
    }
    refreshTime();
  });
  media.addEventListener('play', refreshTransport);
  media.addEventListener('pause', () => {
    refreshTransport();
    persistResume(true);
  });
  media.addEventListener('ended', () => stepTrack(1, true));
  media.addEventListener('error', recoverPlayback);

  document.addEventListener('play', (event) => {
    if (event.target === media) {
      document.querySelectorAll('#layout video, #layout audio').forEach((item) => item.pause());
    } else if (event.target.closest?.('#layout')) {
      media.pause();
    }
  }, true);

  // 作品库播放快捷键（非输入态且无弹窗打开时生效）
  document.addEventListener('keydown', (event) => {
    if (workspace !== 'library' || !playing || player.hidden) return;
    if (event.ctrlKey || event.metaKey || event.altKey) return;
    const tag = (event.target?.tagName || '').toLowerCase();
    if (tag === 'input' || tag === 'textarea' || tag === 'select' || event.target?.isContentEditable) return;
    if (document.querySelector('.modal:not([hidden]), .modal-backdrop.open')) return;
    if (event.code === 'Space') {
      event.preventDefault();
      if (!loadingTrack) {
        if (media.paused) media.play().catch(() => {});
        else media.pause();
      }
    } else if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
      event.preventDefault();
      const step = (event.shiftKey ? 15 : 5) * (event.key === 'ArrowLeft' ? -1 : 1);
      if (Number.isFinite(media.duration)) {
        media.currentTime = Math.min(media.duration, Math.max(0, media.currentTime + step));
      }
    } else if (event.key === '[') {
      event.preventDefault();
      stepTrack(-1, false);
    } else if (event.key === ']') {
      event.preventDefault();
      stepTrack(1, false);
    }
  });

  window.addEventListener('beforeunload', () => persistResume(true));

  API.onTaskEvents(({ task, type }) => {
    const workId = task?.options?.library_work;
    if (!workId) return;
    const previous = taskStatuses.get(task.id);
    taskStatuses.set(task.id, task.status);
    if (previous === task.status && type !== 'done') return;
    if (playing?.work.id === workId && playing.track.id === task.options.library_track && task.has_result) {
      const ver = Date.now();
      playing.track = { ...playing.track, has_subtitles: true, subtitle_version: ver };
      attachCaptions(ver);
    }
    if (selectedWorkId === workId && workspace === 'library') {
      clearTimeout(refreshTimer);
      refreshTimer = setTimeout(() => {
        if (selectedWorkId === workId && workspace === 'library') showWork(workId);
      }, 220);
    }
  });

  window.__TAURI__.event.listen('library-download', ({ payload }) => {
    if (!busy || $('libraryLinkForm').hidden) return;
    const done = formatBytes(payload.done);
    const total = payload.total ? ' / ' + formatBytes(payload.total) : '';
    message('正在下载 ' + done + total + '…');
  }).catch(() => {});

  window.__TAURI__.event.listen('library-import-progress', ({ payload }) => {
    if (!busy) return;
    const files = payload.total_files ? `${payload.done_files}/${payload.total_files}` : '';
    const bytes = payload.total_bytes
      ? `（${formatBytes(payload.done_bytes)} / ${formatBytes(payload.total_bytes)}）`
      : '';
    message(`正在复制作品 ${files}${bytes}…`);
  }).catch(() => {});

  function reloadCaptionsForTask(task) {
    const workId = task?.options?.library_work;
    const trackId = task?.options?.library_track;
    if (playing && workId && playing.work.id === workId && playing.track.id === trackId) {
      const ver = Date.now();
      playing.track = { ...playing.track, has_subtitles: true, subtitle_version: ver };
      attachCaptions(ver);
    }
  }

  window.AsmrLibrary = { importPaths, reloadCaptionsForTask };
  refreshLibrary();
})();
